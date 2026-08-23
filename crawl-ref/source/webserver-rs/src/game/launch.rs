//! Ties `game::manager`, `game::process`, and `game::socket` together into
//! the "a player clicked Play" flow, matching `ws_handler.start_crawl` /
//! `CrawlProcessHandler._start_process` / `_purge_locks_and_start`.
//!
//! Scope note: this covers the core spawn -> attach -> forward -> exit
//! lifecycle, validated against the real `crawl` binary
//! (`tests/play_flow.rs`). NOT YET PORTED: ttyrec recording, the
//! stale-lock purge flow (`ARCHITECTURE.md` §4.3), `-no-player-bones` for
//! account holds, and save-slot info.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sha1::{Digest, Sha1};

use crate::config::ResolvedGame;
use crate::error::{Result, WebtilesError};
use crate::game::manager::GameManager;
use crate::game::process::{self, ProcessSpawnArgs, TerminalProcess};
use crate::game::session::{GameSession, ProcessControl, WhereInfo};
use crate::game::socket::{classify_process_message, FromProcessMessage, GameSocket, ProcessControlMessage};
use crate::http::game_data::GameDataRegistry;
use crate::protocol::ServerMessage;

/// How long to wait for the DCSS process to create its webtiles socket
/// before giving up. Python retries indefinitely; a finite timeout here
/// is a deliberate robustness improvement (a genuinely broken
/// crawl_binary/config should not hang a connection forever).
const SOCKET_APPEAR_TIMEOUT: Duration = Duration::from_secs(20);

/// Start a new game process for `username` under the configured game
/// `game_id`, register it, and spawn the supervising/bridging background
/// tasks. Returns the new [`GameSession`] once the socket handshake has
/// been sent (not once the game has fully booted - matching Python, which
/// also doesn't block `start_crawl` on that).
pub async fn start_game(
    config: &crate::config::ServerConfig,
    game_manager: &GameManager,
    game_data: &GameDataRegistry,
    username: &str,
    game_id: &str,
) -> Result<Arc<GameSession>> {
    let resolved = config
        .resolve_game(game_id)
        .ok_or_else(|| WebtilesError::Game(format!("unknown game id: {game_id}")))?;

    let crawl_binary = resolved
        .fields
        .crawl_binary
        .clone()
        .ok_or_else(|| WebtilesError::Game(format!("game {game_id} has no crawl_binary configured")))?;

    let rcfile_dir = templated_path(&resolved, "rcfile_path", username)?;
    let macro_dir = templated_path(&resolved, "macro_path", username)?;
    let morgue_dir = templated_path(&resolved, "morgue_path", username)?;
    let socket_dir = templated_path(&resolved, "socket_path", username)?;

    for dir in [&rcfile_dir, &macro_dir, &morgue_dir, &socket_dir] {
        tokio::fs::create_dir_all(dir).await?;
    }

    let timestamp = format_timestamp();
    let process_socket_path = socket_dir.join(format!("{username}:{timestamp}.sock"));

    let mut argv = vec![crawl_binary.to_string_lossy().to_string()];
    argv.extend(resolved.fields.pre_options.iter().cloned());
    argv.push("-name".to_string());
    argv.push(username.to_string());
    argv.push("-rc".to_string());
    argv.push(rcfile_dir.join(format!("{username}.rc")).to_string_lossy().to_string());
    argv.push("-macro".to_string());
    argv.push(macro_dir.join(format!("{username}.macro")).to_string_lossy().to_string());
    argv.push("-morgue".to_string());
    argv.push(morgue_dir.to_string_lossy().to_string());
    argv.extend(resolved.fields.options.iter().cloned());
    if let Some(dir_path) = &resolved.fields.dir_path {
        argv.push("-dir".to_string());
        argv.push(resolved.templated(dir_path, Some(username))?);
    }
    argv.push("-webtiles-socket".to_string());
    argv.push(process_socket_path.to_string_lossy().to_string());
    argv.push("-await-connection".to_string());

    let env: Vec<(String, String)> = resolved
        .fields
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let cwd = match &resolved.fields.cwd {
        Some(cwd) => Some(PathBuf::from(resolved.templated(cwd, Some(username))?)),
        None => None,
    };

    let (term_cols, term_rows) = config.recording_term_size;
    let spawn_args = ProcessSpawnArgs {
        argv,
        env,
        cwd,
        term_rows,
        term_cols,
    };

    let terminal_process = TerminalProcess::spawn(spawn_args, None, None).await?;

    wait_for_socket(&process_socket_path).await?;

    let own_socket_dir = config
        .server_socket_path
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let game_socket = GameSocket::connect(&own_socket_dir, &process_socket_path, true).await?;

    let (session, input_rx, control_rx) = GameSession::new_with_channels(username, game_id);
    let session = Arc::new(session);
    game_manager.register(session.clone()).await;

    let morgue_url = resolved
        .fields
        .morgue_url
        .as_ref()
        .map(|u| resolved.templated(u, Some(username)))
        .transpose()?;

    tokio::spawn(bridge_socket(
        game_socket,
        input_rx,
        session.clone(),
        game_data.clone(),
        morgue_url,
    ));
    tokio::spawn(supervise_process(
        terminal_process,
        control_rx,
        session.clone(),
        game_manager.clone(),
    ));

    Ok(session)
}

fn templated_path(resolved: &ResolvedGame, field: &str, username: &str) -> Result<PathBuf> {
    let value = match field {
        "rcfile_path" => resolved.fields.rcfile_path.as_deref(),
        "macro_path" => resolved.fields.macro_path.as_deref(),
        "morgue_path" => resolved.fields.morgue_path.as_deref(),
        "socket_path" => resolved.fields.socket_path.as_deref(),
        _ => None,
    }
    .ok_or_else(|| WebtilesError::Config(format!("game is missing required field `{field}`")))?;
    Ok(PathBuf::from(resolved.templated(value, Some(username))?))
}

async fn wait_for_socket(path: &std::path::Path) -> Result<()> {
    let start = tokio::time::Instant::now();
    while !path.exists() {
        if start.elapsed() > SOCKET_APPEAR_TIMEOUT {
            return Err(WebtilesError::Process(
                "timed out waiting for the game process to create its webtiles socket".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn format_timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}.{:02}:{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Owns the [`TerminalProcess`], waits for it to exit (or a
/// [`ProcessControl`] signal asking it to stop/be killed), then notifies
/// watchers and unregisters the game. Matches
/// `CrawlProcessHandler.stop`/`.kill`/`handle_process_end`.
async fn supervise_process(
    mut process: TerminalProcess,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<ProcessControl>,
    session: Arc<GameSession>,
    game_manager: GameManager,
) {
    loop {
        tokio::select! {
            status = process.wait() => {
                tracing::info!(game_id = session.id, username = %session.username, status = ?status, "game process exited");
                break;
            }
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(ProcessControl::Stop) => { let _ = process.send_sighup(); }
                    Some(ProcessControl::Kill) | None => { let _ = process.send_sigabrt(); }
                }
            }
        }
    }

    let exit = session.exit_info().await;
    session
        .broadcast(ServerMessage::GameEnded {
            reason: exit.reason,
            message: exit.message,
            dump: exit.dump_url,
        })
        .await;
    game_manager.unregister(session.id).await;
}

/// Owns the [`GameSocket`], forwards queued player input to it, and
/// classifies/dispatches everything it receives - matching
/// `CrawlProcessHandler._on_socket_message`/`handle_process_message`.
async fn bridge_socket(
    mut game_socket: GameSocket,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    session: Arc<GameSession>,
    game_data: GameDataRegistry,
    morgue_url: Option<String>,
) {
    loop {
        tokio::select! {
            raw = game_socket.recv_message() => {
                match raw {
                    Ok(bytes) => handle_process_message(bytes, &session, &game_data, morgue_url.as_deref()).await,
                    Err(e) => {
                        tracing::debug!(game_id = session.id, error = %e, "game socket closed");
                        break;
                    }
                }
            }
            input = input_rx.recv() => {
                match input {
                    Some(text) => { let _ = game_socket.send_raw(text.as_bytes()).await; }
                    None => break,
                }
            }
        }
    }
}

async fn handle_process_message(
    raw: Vec<u8>,
    session: &Arc<GameSession>,
    game_data: &GameDataRegistry,
    morgue_url: Option<&str>,
) {
    let classified = match classify_process_message(raw) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(game_id = session.id, error = %e, "malformed process control message");
            return;
        }
    };

    match classified {
        FromProcessMessage::Control(ProcessControlMessage::ClientPath { path, version }) => {
            let mut hasher = Sha1::new();
            hasher.update(path.as_bytes());
            if let Some(v) = &version {
                hasher.update(v.as_bytes());
            }
            let hash = hex::encode(hasher.finalize());

            game_data.register(hash.clone(), PathBuf::from(&path).join("static")).await;

            let template_dir = PathBuf::from(&path).join("templates");
            let ctx = crate::http::templates::TemplateContext::default().with_string("version", &hash);
            match crate::http::templates::render_file(&template_dir, "game.html", &ctx) {
                Ok(content) => {
                    session.broadcast(ServerMessage::GameClient { version: hash, content }).await;
                }
                Err(e) => tracing::warn!(game_id = session.id, error = %e, "failed to render game.html"),
            }
        }
        FromProcessMessage::Control(ProcessControlMessage::FlushMessages) => {
            // NOT YET PORTED: Python switches from immediate-send to
            // batch-queued delivery mode at this point; our batching is
            // already per-flush at the websocket layer regardless, so
            // there is no equivalent mode switch needed here.
        }
        FromProcessMessage::Control(ProcessControlMessage::Dump { kind, filename }) => {
            let stem = process::strip_extension(process::basename(&filename)).to_string();
            if kind == "command" {
                if let Some(url) = morgue_url {
                    session.broadcast(ServerMessage::Dump { url: format!("{url}{stem}") }).await;
                }
            } else {
                let mut info = session.exit_info().await;
                info.dump_url = morgue_url.map(|url| format!("{url}{stem}"));
                session.set_exit_info(info).await;
            }
        }
        FromProcessMessage::Control(ProcessControlMessage::ExitReason { kind, message }) => {
            let mut info = session.exit_info().await;
            info.reason = Some(kind);
            info.message = message;
            session.set_exit_info(info).await;
        }
        FromProcessMessage::Control(ProcessControlMessage::Milestone { fields }) => {
            let get = |k: &str| fields.get(k).and_then(|v| v.as_str()).map(str::to_string);
            session
                .set_where_info(WhereInfo {
                    xl: get("xl"),
                    char: get("char"),
                    place: get("place"),
                    turn: get("turn"),
                    dur: get("dur"),
                    god: get("god"),
                    title: get("title"),
                })
                .await;
            session.set_last_milestone(get("milestone")).await;
        }
        FromProcessMessage::ForwardVerbatim(bytes) => {
            if let Ok(text) = String::from_utf8(bytes) {
                session.broadcast(crate::game::session::OutgoingMessage::Raw(text)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_timestamp_matches_python_strftime_shape() {
        let ts = format_timestamp();
        // "YYYY-MM-DD.HH:MM:SS" - matches formatted_time in process_handler.py
        assert_eq!(ts.len(), 19);
        assert_eq!(ts.as_bytes()[4], b'-');
        assert_eq!(ts.as_bytes()[7], b'-');
        assert_eq!(ts.as_bytes()[10], b'.');
        assert_eq!(ts.as_bytes()[13], b':');
        assert_eq!(ts.as_bytes()[16], b':');
    }
}
