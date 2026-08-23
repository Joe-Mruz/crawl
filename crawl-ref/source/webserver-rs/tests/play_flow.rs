//! End-to-end test of the `play` flow through the real websocket
//! connection handler, using the real compiled `crawl` binary. Skips
//! itself if `../crawl` doesn't exist. Complements
//! `tests/real_crawl_handshake.rs` (which exercises `game::process`/
//! `game::socket` directly) by validating the full HTTP-login ->
//! websocket `play` -> real game process -> `go_lobby` shutdown pipeline.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use webtiles_rs::config::{GameConfig, GameFields, ServerConfig};
use webtiles_rs::protocol::FrameDecompressor;
use webtiles_rs::state::AppState;
use webtiles_rs::userdb::UserDb;

fn crawl_binary_path() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crawl");
    path.exists().then_some(path)
}

/// Minimal raw HTTP/1.1 GET, returning (status_code, body_len). Used to
/// check that `/gamedata/<hash>/...` assets are actually servable -
/// `tokio_tungstenite`/`reqwest` aren't dev-deps, and this is simple
/// enough not to need them.
async fn http_get_status(addr: std::net::SocketAddr, path: &str) -> (u16, usize) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or("");
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body_len = text.split("\r\n\r\n").nth(1).map(|b| b.len()).unwrap_or(0);
    (status, body_len)
}

async fn spawn_test_server(crawl_binary: PathBuf, rcs_dir: &std::path::Path) -> (std::net::SocketAddr, AppState) {
    let users = UserDb::open(rcs_dir.join("passwd.db3"), rcs_dir.join("settings.db3")).unwrap();
    users
        .register_user("alice", "hunter2", None)
        .await
        .unwrap()
        .unwrap();
    std::fs::create_dir_all(rcs_dir.join("saves")).unwrap();

    let mut config = ServerConfig::default();
    config.dgl_mode = true;

    let mut games = BTreeMap::new();
    games.insert(
        "dcss-web-trunk".to_string(),
        GameConfig {
            id: "dcss-web-trunk".to_string(),
            template: None,
            fields: GameFields {
                name: Some("Play".to_string()),
                crawl_binary: Some(crawl_binary),
                rcfile_path: Some(rcs_dir.join("rcs").to_string_lossy().to_string()),
                macro_path: Some(rcs_dir.join("rcs").to_string_lossy().to_string()),
                morgue_path: Some(rcs_dir.join("morgue").to_string_lossy().to_string()),
                socket_path: Some(rcs_dir.join("sockets").to_string_lossy().to_string()),
                // without this, DCSS defaults to saving in its CWD (this
                // test binary's, i.e. the crate root) instead of the
                // tempdir - leaving stray `.alice.cs` files behind.
                dir_path: Some(rcs_dir.join("saves").to_string_lossy().to_string()),
                client_path: Some(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../webserver/game_data")
                        .to_string_lossy()
                        .to_string(),
                ),
                ..Default::default()
            },
        },
    );
    config.games = games;

    let state = AppState::new(config, users);
    let router = webtiles_rs::http::build_router(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, state)
}

async fn recv_json(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    decompressor: &mut FrameDecompressor,
) -> serde_json::Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("timed out waiting for a websocket frame")
            .expect("stream ended")
            .expect("websocket error");
        let text = match msg {
            WsMessage::Text(t) => t.to_string(),
            // the server always compresses frames (raw deflate, matching
            // the real JS client) - see PROTOCOL.md \u00a71.
            WsMessage::Binary(bytes) => {
                let decompressed = decompressor.decompress_frame(&bytes).unwrap();
                String::from_utf8(decompressed).unwrap()
            }
            _ => continue,
        };
        return serde_json::from_str(&text).unwrap();
    }
}

/// Does `value` (a `{"msgs":[...]}` batch) contain a message with this
/// `msg` name? Used because several unrelated messages (lobby_clear,
/// set_game_links, ping, ...) can share a batch with the one under test.
fn batch_contains_msg(value: &serde_json::Value, msg_name: &str) -> bool {
    value["msgs"]
        .as_array()
        .map(|msgs| msgs.iter().any(|m| m["msg"] == msg_name))
        .unwrap_or(false)
}

#[tokio::test]
async fn play_spawns_a_real_game_and_go_lobby_stops_it() {
    let Some(crawl_binary) = crawl_binary_path() else {
        eprintln!("skipping: no compiled crawl binary at ../crawl");
        return;
    };
    let rcs_dir = tempfile::tempdir().unwrap();
    let (addr, state) = spawn_test_server(crawl_binary, rcs_dir.path()).await;

    let url = format!("ws://{addr}/socket");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let mut decompressor = FrameDecompressor::new();

    // initial lobby batch
    let lobby = recv_json(&mut ws, &mut decompressor).await;
    assert!(batch_contains_msg(&lobby, "lobby_complete"));

    ws.send(WsMessage::text(r#"{"msg":"login","username":"alice","password":"hunter2"}"#))
        .await
        .unwrap();
    let login_response = recv_json(&mut ws, &mut decompressor).await;
    assert!(batch_contains_msg(&login_response, "login_success"));
    assert!(
        batch_contains_msg(&login_response, "set_game_links"),
        "expected set_game_links after login: {login_response}"
    );

    ws.send(WsMessage::text(r#"{"msg":"play","game_id":"dcss-web-trunk"}"#))
        .await
        .unwrap();
    let play_response = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let msg = recv_json(&mut ws, &mut decompressor).await;
            if batch_contains_msg(&msg, "game_started") {
                return msg;
            }
        }
    })
    .await
    .expect("timed out waiting for game_started");
    assert!(batch_contains_msg(&play_response, "game_started"));

    // does the actual game screen ever get sent? (not checked by the
    // game_started assertion above - this is the part that renders
    // something in the browser)
    let game_client = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let msg = recv_json(&mut ws, &mut decompressor).await;
            eprintln!("DIAG: received batch: {msg}");
            if let Some(msgs) = msg["msgs"].as_array() {
                if let Some(gc) = msgs.iter().find(|m| m["msg"] == "game_client") {
                    return gc.clone();
                }
            }
        }
    })
    .await
    .expect("timed out waiting for game_client");
    eprintln!(
        "DIAG: got game_client, version={:?}, content_len={}",
        game_client["version"],
        game_client["content"].as_str().unwrap_or("").len()
    );

    // is the game-specific JS bundle actually servable? (this is what
    // handles regular alphanumeric key input via "game_keypress" -
    // if it 404s, requirejs fails silently and typing breaks while
    // arrow keys, handled independently in client.js, keep working)
    let version = game_client["version"].as_str().unwrap();
    for asset in ["game.js", "ui.js"] {
        let (status, len) = http_get_status(addr, &format!("/gamedata/{version}/{asset}")).await;
        eprintln!("DIAG: GET /gamedata/{version}/{asset} -> {status} ({len} bytes)");
    }

    // Drain any backlog so the next message we see is provably a reaction
    // to the key we're about to send, not something already in flight.
    loop {
        let drained = tokio::time::timeout(Duration::from_millis(500), recv_json(&mut ws, &mut decompressor)).await;
        match drained {
            Ok(msg) => eprintln!("DIAG: draining backlog before key test: {msg}"),
            Err(_) => break,
        }
    }

    // does a "key" message (what real keyboard input sends - see
    // client.js's send_keycode) actually reach the real crawl process?
    // Send 'a' (keycode 97), which on the species-select chargen screen
    // picks the first listed species and should provoke a visible
    // reaction (a new ui-push/ui-state message) from the game.
    ws.send(WsMessage::text(r#"{"msg":"key","keycode":97}"#))
        .await
        .unwrap();
    let key_reaction = tokio::time::timeout(Duration::from_secs(10), async {
        recv_json(&mut ws, &mut decompressor).await
    })
    .await;
    eprintln!("DIAG: reaction after sending key 'a': {key_reaction:?}");
    assert!(key_reaction.is_ok(), "no reaction at all from the game process after sending a `key` message - forwarding is broken");

    // does an "input" message (what real *typing* sends - see
    // client.js's handle_keypress -> send_message("input", {text: s}))
    // also reach the real crawl process? This is NOT forwarded to the
    // game socket like "key" is - Python's handle_input decodes it and
    // writes straight to the PTY (see CrawlProcessHandler.handle_input).
    loop {
        let drained = tokio::time::timeout(Duration::from_millis(500), recv_json(&mut ws, &mut decompressor)).await;
        match drained {
            Ok(msg) => eprintln!("DIAG: draining backlog before input test: {msg}"),
            Err(_) => break,
        }
    }
    ws.send(WsMessage::text(r#"{"msg":"input","text":"a"}"#))
        .await
        .unwrap();
    let input_reaction = tokio::time::timeout(Duration::from_secs(10), async {
        recv_json(&mut ws, &mut decompressor).await
    })
    .await;
    eprintln!("DIAG: reaction after sending input text 'a': {input_reaction:?}");
    assert!(
        input_reaction.is_ok(),
        "no reaction at all from the game process after sending an `input` message - typing is broken"
    );

    // does the process lock up after a handful of keystrokes? send a
    // long rapid sequence of "input" messages (simulating fast typing)
    // and see how many get a reaction before one times out.
    let chars = "bcdefghijklmnopqrstuvwxyz0123456789";
    for (i, ch) in chars.chars().enumerate() {
        ws.send(WsMessage::text(format!(r#"{{"msg":"input","text":"{ch}"}}"#)))
            .await
            .unwrap();
        let reaction = tokio::time::timeout(Duration::from_secs(5), recv_json(&mut ws, &mut decompressor)).await;
        match &reaction {
            Ok(msg) => eprintln!("DIAG: keystroke #{i} ('{ch}') reaction: {msg}"),
            Err(_) => eprintln!("DIAG: keystroke #{i} ('{ch}') got NO reaction within 5s - LOCKUP HERE"),
        }
        assert!(reaction.is_ok(), "process stopped responding after {i} rapid keystrokes");
    }

    // Does the client get sent back to the lobby when the *game* ends on
    // its own (quit/save-exit in-game), as opposed to the client
    // explicitly asking to leave via "go_lobby"? Simulate this by killing
    // the process directly (bypassing the websocket entirely, like an
    // admin action or a crash would), and confirm both `game_ended` *and*
    // `go_lobby` arrive without the client ever sending anything - this
    // is the part `_on_crawl_end` handles in Python that a bare
    // `game_ended` broadcast alone does not.
    state
        .games
        .find_by_username("alice")
        .await
        .expect("session should still be registered")
        .request_kill();

    let mut saw_game_ended = false;
    let mut saw_go_lobby = false;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let msg = recv_json(&mut ws, &mut decompressor).await;
            eprintln!("DIAG: after external kill: {msg}");
            saw_game_ended |= batch_contains_msg(&msg, "game_ended");
            saw_go_lobby |= batch_contains_msg(&msg, "go_lobby");
            if saw_game_ended && saw_go_lobby {
                return;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for both game_ended and go_lobby after the process ended on its own \
         (saw_game_ended={saw_game_ended}, saw_go_lobby={saw_go_lobby}) - the client would be stuck \
         on the game screen forever"
    );
}
