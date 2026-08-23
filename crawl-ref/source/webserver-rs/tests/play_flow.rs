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

async fn spawn_test_server(crawl_binary: PathBuf, rcs_dir: &std::path::Path) -> std::net::SocketAddr {
    let users = UserDb::open(rcs_dir.join("passwd.db3"), rcs_dir.join("settings.db3")).unwrap();
    users
        .register_user("alice", "hunter2", None)
        .await
        .unwrap()
        .unwrap();

    let mut config = ServerConfig::default();
    config.dgl_mode = true;
    config.template_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../webserver/templates");

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
                ..Default::default()
            },
        },
    );
    config.games = games;

    let state = AppState::new(config, users);
    let router = webtiles_rs::http::build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
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
    let addr = spawn_test_server(crawl_binary, rcs_dir.path()).await;

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

    // ask to leave; this should SIGHUP the real crawl process
    ws.send(WsMessage::text(r#"{"msg":"go_lobby"}"#)).await.unwrap();

    let saw_game_ended_or_lobby = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let msg = recv_json(&mut ws, &mut decompressor).await;
            if batch_contains_msg(&msg, "game_ended") || batch_contains_msg(&msg, "go_lobby") {
                return;
            }
        }
    })
    .await;
    assert!(saw_game_ended_or_lobby.is_ok(), "never saw game_ended/go_lobby after requesting to leave");
}
