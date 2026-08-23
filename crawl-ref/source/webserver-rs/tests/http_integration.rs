//! In-process integration test: runs the real Axum app end-to-end (HTTP +
//! WebSocket), per the task's "Integration tests... Test the Axum
//! application with an in-process server" requirement. Complements the
//! manual `curl`/`tokio-tungstenite` validation already performed
//! against a real running instance during development.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use webtiles_rs::config::ServerConfig;
use webtiles_rs::state::AppState;
use webtiles_rs::userdb::UserDb;

async fn spawn_test_server() -> std::net::SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let users = UserDb::open(dir.path().join("passwd.db3"), dir.path().join("settings.db3")).unwrap();
    // leak the tempdir so it outlives the spawned server task for the
    // duration of the test process (acceptable for a short-lived test).
    std::mem::forget(dir);

    let mut config = ServerConfig::default();
    config.dgl_mode = true;
    config.template_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../webserver/templates");
    let state = AppState::new(config, users);
    let router = webtiles_rs::http::build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    // give the server a moment to start accepting
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Minimal raw HTTP/1.1 GET, avoiding a full HTTP client dependency for
/// what is just a handful of sanity-check requests. Async (not blocking
/// `std::net`), so it cooperatively yields on the same single-threaded
/// test runtime that's also driving the spawned server task.
async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status_line = response.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, response)
}

#[tokio::test]
async fn status_version_endpoint_returns_expected_json() {
    let addr = spawn_test_server().await;
    let (status, response) = http_get(addr, "/status/version/").await;
    assert_eq!(status, 200);
    assert!(response.contains(r#""webtiles""#));
    assert!(response.contains(r#""rust_supported":true"#));
}

#[tokio::test]
async fn status_lobby_endpoint_returns_empty_json_array_with_no_games() {
    let addr = spawn_test_server().await;
    let (status, response) = http_get(addr, "/status/lobby/").await;
    assert_eq!(status, 200);
    assert!(response.trim_end().ends_with("[]"));
}

#[tokio::test]
async fn main_page_renders_client_html_with_socket_server_substituted() {
    let addr = spawn_test_server().await;
    let (status, response) = http_get(addr, "/").await;
    assert_eq!(status, 200);
    assert!(response.contains(&format!("ws://{addr}/socket")));
}

#[tokio::test]
async fn unknown_gamedata_version_is_not_found() {
    let addr = spawn_test_server().await;
    let (status, _) = http_get(addr, "/gamedata/deadbeef/foo.png").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn websocket_connection_receives_a_lobby_batch_on_connect() {
    let addr = spawn_test_server().await;
    let url = format!("ws://{addr}/socket");
    let (mut ws, response) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(response.status(), 101);

    let first_frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for the lobby frame")
        .expect("stream ended unexpectedly")
        .expect("websocket error");

    // dgl_mode with no games configured sends an (optionally compressed)
    // lobby_clear/lobby_complete batch on connect; a real client would
    // inflate a Binary frame, but we just confirm *some* non-empty frame
    // arrived promptly, exercising the full connect -> handler -> flush
    // pipeline end-to-end.
    match first_frame {
        WsMessage::Binary(bytes) => assert!(!bytes.is_empty()),
        WsMessage::Text(text) => assert!(!text.is_empty()),
        other => panic!("unexpected first frame: {other:?}"),
    }

    ws.close(None).await.ok();
}
