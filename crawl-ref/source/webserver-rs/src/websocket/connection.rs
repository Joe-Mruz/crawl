//! Per-connection WebSocket handling: transport <-> protocol decode/encode
//! <-> game/lobby logic. See `../ARCHITECTURE.md` §3 for the layering this
//! implements and `PROTOCOL.md` for the exact message catalog.
//!
//! Scope note: this implements the core connect/login/lobby/play/watch/
//! chat/disconnect flows end-to-end (validated against the real `crawl`
//! binary for the process-management half in
//! `tests/real_crawl_handshake.rs`). Several Python features are not yet
//! ported here - see the `NOT YET PORTED` markers - registration/password
//! reset email flows, RC file editing, admin commands, save-slot info,
//! and reconnection via `watch_socket_dirs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use tokio::sync::mpsc;

use crate::game::session::{GameSession, OutgoingMessage, Watcher};
use crate::protocol::{ClientMessage, FrameCompressor, KnownClientMessage, MessageBatcher, ServerMessage};
use crate::state::AppState;

fn next_connection_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Per-connection state, matching the instance fields of
/// `ws_handler.CrawlWebSocket`.
struct Connection {
    id: u64,
    state: AppState,
    username: Option<String>,
    is_admin: bool,
    watching: Option<Arc<GameSession>>,
    outgoing_rx: Option<mpsc::Receiver<OutgoingMessage>>,
    compressor: FrameCompressor,
    batcher: MessageBatcher,
}

/// Entry point called from the Axum WebSocket upgrade handler.
pub async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut conn = Connection {
        id: next_connection_id(),
        state,
        username: None,
        is_admin: false,
        watching: None,
        outgoing_rx: None,
        compressor: FrameCompressor::new(),
        batcher: MessageBatcher::new(),
    };

    tracing::info!(connection_id = conn.id, "socket opened");

    if conn.state.config.max_connections > 0
        && conn.state.games.count().await as u32 >= conn.state.config.max_connections
    {
        // Matches `open()`'s rejection: queued and flushed through the
        // normal batch/compression path like any other message, even
        // though the literal itself is a raw JS statement rather than a
        // JSON-encoded string - see PROTOCOL.md §1.
        conn.batcher.queue_raw(
            "connection_closed('The maximum number of connections has been reached, sorry :(');",
        );
        flush(&mut conn, &mut socket).await;
        return;
    }

    if conn.state.config.dgl_mode {
        send_lobby(&mut conn).await;
    }

    // flush whatever was queued above (lobby data) immediately, rather
    // than waiting for the first select! branch to fire in the message
    // loop below - matches Python's `send_message`, which flushes as soon
    // as it's called rather than on some later event.
    if !flush(&mut conn, &mut socket).await {
        return;
    }

    run_message_loop(socket, conn).await;
}

async fn run_message_loop(mut socket: WebSocket, mut conn: Connection) {
    let mut ping_interval = tokio::time::interval(Duration::from_secs(
        conn.state.config.connection_timeout_secs.max(1),
    ));
    ping_interval.tick().await; // first tick fires immediately; discard it

    loop {
        let outgoing = conn.outgoing_rx.as_mut();
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_frame(&mut conn, &text).await;
                        if !flush(&mut conn, &mut socket).await { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ping/pong/binary frames from client: ignored
                    Some(Err(e)) => {
                        tracing::warn!(connection_id = conn.id, error = %e, "websocket error");
                        break;
                    }
                }
            }
            Some(msg) = async { match outgoing { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                queue_outgoing(&mut conn, msg);
                if !flush(&mut conn, &mut socket).await { break; }
            }
            _ = ping_interval.tick() => {
                conn.batcher.queue(&ServerMessage::Ping).ok();
                if !flush(&mut conn, &mut socket).await { break; }
            }
        }
    }

    if let Some(watching) = &conn.watching {
        watching.remove_watcher(conn.id).await;
    }
    tracing::info!(connection_id = conn.id, "socket closed");
}

fn queue_outgoing(conn: &mut Connection, message: OutgoingMessage) {
    match message {
        OutgoingMessage::Typed(msg) => {
            let _ = conn.batcher.queue(&msg);
        }
        OutgoingMessage::Raw(text) => conn.batcher.queue_raw(text),
    }
}

async fn handle_client_frame(conn: &mut Connection, frame: &str) {
    match ClientMessage::parse(frame) {
        Ok(ClientMessage::Known(known)) => handle_known_message(conn, known).await,
        Ok(ClientMessage::PassThrough { msg, .. }) => {
            // NOT YET PORTED: forwarding raw input to an attached game
            // process's socket (requires the play/spawn flow to be wired
            // up first - see the module doc). For now, unrecognized
            // messages while not playing are logged like Python does.
            tracing::debug!(connection_id = conn.id, %msg, "unhandled pass-through message");
        }
        Err(e) => {
            tracing::warn!(connection_id = conn.id, error = %e, "failed to parse client message");
        }
    }
}

async fn handle_known_message(conn: &mut Connection, message: KnownClientMessage) {
    match message {
        KnownClientMessage::Login { username, password } => login(conn, &username, &password).await,
        KnownClientMessage::TokenLogin { cookie } => token_login(conn, &cookie).await,
        KnownClientMessage::SetLoginCookie => set_login_cookie(conn).await,
        KnownClientMessage::ForgetLoginCookie { cookie } => {
            conn.state.login_tokens.forget(&cookie).await;
        }
        KnownClientMessage::Pong => {}
        KnownClientMessage::Watch { username } => watch(conn, &username).await,
        KnownClientMessage::GoLobby => go_lobby(conn).await,
        KnownClientMessage::ChatMsg { text } => chat(conn, &text).await,
        // NOT YET PORTED: play/register/rc-editing/admin/password-reset -
        // see module doc.
        _ => {
            tracing::debug!(connection_id = conn.id, "message type not yet implemented");
        }
    }
}

async fn login(conn: &mut Connection, username: &str, password: &str) {
    match conn.state.users.check_password(username, password).await {
        Ok((true, Some(real_username), _)) => {
            conn.username = Some(real_username.clone());
            if let Ok(Some(info)) = conn.state.users.get_user_info(&real_username).await {
                conn.is_admin = crate::userdb::is_admin(info.flags);
            }
            conn.batcher
                .queue(&ServerMessage::LoginSuccess {
                    username: real_username,
                    admin: conn.is_admin,
                })
                .ok();
        }
        Ok((false, _, Some(reason))) => {
            conn.batcher.queue(&ServerMessage::LoginFail { reason: Some(reason) }).ok();
        }
        _ => {
            conn.batcher.queue(&ServerMessage::LoginFail { reason: None }).ok();
        }
    }
}

async fn token_login(conn: &mut Connection, cookie: &str) {
    if let Some(username) = conn.state.login_tokens.consume(cookie).await {
        conn.username = Some(username.clone());
        conn.batcher
            .queue(&ServerMessage::LoginSuccess { username, admin: conn.is_admin })
            .ok();
    } else {
        conn.batcher.queue(&ServerMessage::LoginFail { reason: None }).ok();
    }
}

async fn set_login_cookie(conn: &mut Connection) {
    let Some(username) = conn.username.clone() else { return };
    let lifetime = Duration::from_secs(
        (conn.state.config.login_token_lifetime_days.max(0) as u64) * 24 * 60 * 60,
    );
    let cookie = conn.state.login_tokens.issue(&username, lifetime).await;
    conn.batcher
        .queue(&ServerMessage::LoginCookie {
            cookie,
            expires: conn.state.config.login_token_lifetime_days,
        })
        .ok();
}

async fn watch(conn: &mut Connection, username: &str) {
    if let Some(previous) = conn.watching.take() {
        previous.remove_watcher(conn.id).await;
    }
    let Some(session) = conn.state.games.find_by_username(username).await else {
        go_lobby(conn).await;
        return;
    };
    if session.is_blocked(conn.username.as_deref()).await && !conn.is_admin {
        conn.batcher
            .queue(&ServerMessage::AuthError {
                reason: "Spectating this player is restricted.".to_string(),
            })
            .ok();
        go_lobby(conn).await;
        return;
    }

    let (watcher, rx) = Watcher::new(conn.id, conn.username.clone(), false, conn.is_admin);
    session.add_watcher(watcher).await;
    conn.outgoing_rx = Some(rx);
    conn.watching = Some(session);
    conn.batcher
        .queue(&ServerMessage::WatchingStarted { username: username.to_string() })
        .ok();
}

async fn go_lobby(conn: &mut Connection) {
    if !conn.state.config.dgl_mode {
        return;
    }
    if let Some(session) = conn.watching.take() {
        session.remove_watcher(conn.id).await;
    }
    conn.outgoing_rx = None;
    conn.batcher.queue(&ServerMessage::GoLobby).ok();
    send_lobby(conn).await;
}

async fn chat(conn: &mut Connection, text: &str) {
    let Some(session) = &conn.watching else { return };
    let Some(username) = &conn.username else {
        conn.batcher
            .queue(&ServerMessage::Chat {
                content: "You need to log in to send messages!".to_string(),
                meta: None,
            })
            .ok();
        return;
    };
    let max_length = conn.state.config.max_chat_length;
    let mut text = text.to_string();
    if max_length > 0 && text.len() >= max_length {
        text.truncate(max_length.saturating_sub(5));
        text.push_str("[...]");
    }
    let content = format!(
        "<span class='chat_sender'>{}</span>: <span class='chat_msg'>{}</span>",
        html_escape(username),
        html_escape(&text)
    );
    session.broadcast(ServerMessage::Chat { content, meta: None }).await;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn send_lobby(conn: &mut Connection) {
    conn.batcher.queue(&ServerMessage::LobbyClear).ok();
    for entry in conn.state.games.lobby_entries().await {
        conn.batcher
            .queue(&ServerMessage::LobbyEntry { entry })
            .ok();
    }
    conn.batcher.queue(&ServerMessage::LobbyComplete).ok();
}

/// Flush any queued messages as one batched, optionally-compressed frame.
/// Returns `false` if the send failed and the connection should close.
async fn flush(conn: &mut Connection, socket: &mut WebSocket) -> bool {
    let Some(body) = conn.batcher.flush() else {
        return true;
    };
    match conn.compressor.compress_frame(body.as_bytes()) {
        Ok(compressed) => socket.send(Message::Binary(compressed.into())).await.is_ok(),
        Err(_) => socket.send(Message::Text(body.into())).await.is_ok(),
    }
}
