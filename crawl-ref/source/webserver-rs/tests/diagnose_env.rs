use webtiles_rs::game::process::{ProcessOutputLine, ProcessSpawnArgs, TerminalProcess};
use webtiles_rs::game::socket::GameSocket;

#[tokio::test]
async fn diagnose_child_environment() {
    std::env::set_var("LANG", "en_US.UTF-8");
    let args = ProcessSpawnArgs {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo LANG=$LANG; locale charmap 2>&1; echo done".to_string(),
        ],
        env: vec![],
        cwd: None,
        term_rows: 24,
        term_cols: 80,
    };
    let mut process = TerminalProcess::spawn(args, None, None).await.unwrap();
    let mut pty_reader = process.pty_reader.take().unwrap();
    let mut collected = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read(&mut pty_reader, &mut buf),
        )
        .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => collected.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    process.wait().await.ok();
    let output = String::from_utf8_lossy(&collected);
    eprintln!("CHILD OUTPUT:\n{output}");
}

/// Spawn the real crawl binary with the same argv shape `game::launch`
/// builds, and actually read stderr this time (our production code
/// currently never drains `TerminalProcess::output_rx` - see the bug this
/// is diagnosing).
#[tokio::test]
async fn diagnose_real_crawl_startup_failure() {
    let crawl_binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crawl");
    if !crawl_binary.exists() {
        eprintln!("skipping: no compiled crawl binary at ../crawl");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("rcs")).unwrap();
    std::fs::create_dir_all(dir.path().join("morgue")).unwrap();
    std::fs::create_dir_all(dir.path().join("sockets")).unwrap();

    let socket_path = dir.path().join("sockets/test:diag.sock");
    let args = ProcessSpawnArgs {
        argv: vec![
            crawl_binary.to_string_lossy().to_string(),
            "-name".to_string(),
            "test".to_string(),
            "-rc".to_string(),
            dir.path().join("rcs/test.rc").to_string_lossy().to_string(),
            "-macro".to_string(),
            dir.path().join("rcs/test.macro").to_string_lossy().to_string(),
            "-morgue".to_string(),
            dir.path().join("morgue").to_string_lossy().to_string(),
            "-webtiles-socket".to_string(),
            socket_path.to_string_lossy().to_string(),
            "-await-connection".to_string(),
        ],
        env: vec![],
        cwd: None,
        term_rows: 24,
        term_cols: 80,
    };

    let mut process = TerminalProcess::spawn(args, None, None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::select! {
            line = process.output_rx.recv() => {
                match line {
                    Some(ProcessOutputLine::Stdout(l)) => stdout_lines.push(l),
                    Some(ProcessOutputLine::Stderr(l)) => stderr_lines.push(l),
                    None => break,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                if socket_path.exists() {
                    break;
                }
            }
        }
    }

    eprintln!("socket created: {}", socket_path.exists());
    eprintln!("STDOUT lines: {stdout_lines:?}");
    eprintln!("STDERR lines: {stderr_lines:?}");
    let _ = process.send_sigabrt();
}

/// Same as above, but this time completes the `attach` handshake and stays
/// connected, mirroring exactly what `game::launch::start_game` does for
/// a real "Play" click - the previous test never attached, so the process
/// just sat blocked in `_await_connection()`.
#[tokio::test]
async fn diagnose_real_crawl_after_attach() {
    let crawl_binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crawl");
    if !crawl_binary.exists() {
        eprintln!("skipping: no compiled crawl binary at ../crawl");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("rcs")).unwrap();
    std::fs::create_dir_all(dir.path().join("morgue")).unwrap();
    std::fs::create_dir_all(dir.path().join("sockets")).unwrap();

    let socket_path = dir.path().join("sockets/test:diag2.sock");
    let args = ProcessSpawnArgs {
        argv: vec![
            crawl_binary.to_string_lossy().to_string(),
            "-name".to_string(),
            "test".to_string(),
            "-rc".to_string(),
            dir.path().join("rcs/test.rc").to_string_lossy().to_string(),
            "-macro".to_string(),
            dir.path().join("rcs/test.macro").to_string_lossy().to_string(),
            "-morgue".to_string(),
            dir.path().join("morgue").to_string_lossy().to_string(),
            "-webtiles-socket".to_string(),
            socket_path.to_string_lossy().to_string(),
            "-await-connection".to_string(),
        ],
        env: vec![],
        cwd: None,
        term_rows: 24,
        term_cols: 80,
    };

    let mut process = TerminalProcess::spawn(args, None, None).await.unwrap();

    while !socket_path.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let mut game_socket = GameSocket::connect(dir.path(), &socket_path, true).await.unwrap();

    let mut stderr_lines = Vec::new();
    let mut socket_messages = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    loop {
        if tokio::time::Instant::now() > deadline {
            eprintln!("(deadline reached, stopping observation)");
            break;
        }
        tokio::select! {
            line = process.output_rx.recv() => {
                match line {
                    Some(ProcessOutputLine::Stdout(l)) => eprintln!("STDOUT: {l}"),
                    Some(ProcessOutputLine::Stderr(l)) => { eprintln!("STDERR: {l}"); stderr_lines.push(l); }
                    None => {}
                }
            }
            msg = game_socket.recv_message() => {
                match msg {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        eprintln!("SOCKET MSG: {text}");
                        socket_messages.push(text);
                    }
                    Err(e) => { eprintln!("SOCKET ERROR: {e}"); break; }
                }
            }
        }
    }

    eprintln!("--- summary ---");
    eprintln!("stderr lines: {stderr_lines:?}");
    eprintln!("socket messages received: {}", socket_messages.len());
}
