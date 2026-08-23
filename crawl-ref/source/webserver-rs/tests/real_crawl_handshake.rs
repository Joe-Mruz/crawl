//! End-to-end smoke test against the *real* compiled DCSS binary
//! (`crawl-ref/source/crawl`), validating the actual crawl↔webtiles Unix
//! datagram protocol described in `PROTOCOL.md` §1/§4 - not just our own
//! code in isolation. Skips itself if the binary isn't present (e.g. a
//! CI/dev environment that hasn't built it) or if this environment
//! disallows AF_UNIX socket creation.

use std::path::PathBuf;
use std::time::Duration;

use webtiles_rs::game::process::{ProcessSpawnArgs, TerminalProcess};
use webtiles_rs::game::socket::{classify_process_message, FromProcessMessage, GameSocket, ProcessControlMessage};

fn crawl_binary_path() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crawl");
    path.exists().then_some(path)
}

#[tokio::test]
async fn real_crawl_binary_completes_the_attach_handshake() {
    let Some(crawl_binary) = crawl_binary_path() else {
        eprintln!("skipping: no compiled crawl binary at ../crawl");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let rc_path = dir.path().join("test.rc");
    let macro_path = dir.path().join("test.macro");
    let morgue_path = dir.path();
    let peer_socket_path = dir.path().join("smoketest:handshake.sock");

    let args = ProcessSpawnArgs {
        argv: vec![
            crawl_binary.to_string_lossy().to_string(),
            "-name".to_string(),
            "smoketest".to_string(),
            "-rc".to_string(),
            rc_path.to_string_lossy().to_string(),
            "-macro".to_string(),
            macro_path.to_string_lossy().to_string(),
            "-morgue".to_string(),
            morgue_path.to_string_lossy().to_string(),
            "-webtiles-socket".to_string(),
            peer_socket_path.to_string_lossy().to_string(),
            "-await-connection".to_string(),
        ],
        env: vec![],
        cwd: None,
        term_rows: 24,
        term_cols: 80,
    };

    let mut process = match TerminalProcess::spawn(args, None, None).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping: failed to spawn crawl (sandboxed environment?): {e}");
            return;
        }
    };

    // wait for the process to create its socket (mirrors
    // WebtilesSocketConnection.connect's retry loop)
    let mut waited = Duration::ZERO;
    while !peer_socket_path.exists() {
        if waited > Duration::from_secs(10) {
            panic!("crawl never created its webtiles socket");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    }

    let mut game_socket = match GameSocket::connect(dir.path(), &peer_socket_path, true).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: failed to create our own AF_UNIX socket: {e}");
            let _ = process.send_sigabrt();
            return;
        }
    };

    let raw = tokio::time::timeout(Duration::from_secs(10), game_socket.recv_message())
        .await
        .expect("timed out waiting for crawl's first protocol message")
        .expect("recv_message failed");

    // This build may or may not have been compiled with `WEB_DIR_PATH` set
    // (see tileweb.cc:_send_version); if it was, the very first message is
    // the `*`-prefixed `client_path` control message, otherwise the first
    // message is the ordinary (unprefixed, forward-to-browser) `version`
    // message. Both are valid per PROTOCOL.md - accept either, since this
    // test is about validating the real wire protocol, not this build's
    // compile-time flags.
    match classify_process_message(raw).unwrap() {
        FromProcessMessage::Control(ProcessControlMessage::ClientPath { path, version }) => {
            assert!(!path.is_empty());
            eprintln!("real crawl handshake ok: client_path={path} version={version:?}");
        }
        FromProcessMessage::ForwardVerbatim(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                text.contains(r#""msg":"version""#),
                "expected a version message, got: {text}"
            );
            eprintln!("real crawl handshake ok (no WEB_DIR_PATH build): {text}");
        }
        other => panic!("unexpected first message from crawl: {other:?}"),
    }

    // clean shutdown: SIGHUP should make a real, freshly-started (no
    // character yet) crawl process exit on its own within a few seconds.
    process.send_sighup().expect("failed to send SIGHUP");
    let status = tokio::time::timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("crawl did not exit after SIGHUP within 10s")
        .expect("wait() failed");
    eprintln!("crawl exited with status: {status:?}");
}
