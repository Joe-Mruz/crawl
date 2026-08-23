//! Runtime smoke test for PTY process spawning (`game::process`). Unlike
//! the unit tests in `src/game/process.rs` (pure parsing logic), this
//! actually forks a real child process through a real PTY, to confirm the
//! `pty-process` integration works at runtime and not just at compile
//! time.

use webtiles_rs::game::process::{ProcessSpawnArgs, TerminalProcess};

#[tokio::test]
async fn spawns_a_real_process_and_reads_its_pty_output() {
    let args = ProcessSpawnArgs {
        argv: vec!["/bin/echo".to_string(), "hello from the pty".to_string()],
        env: vec![],
        cwd: None,
        term_rows: 24,
        term_cols: 80,
    };

    let mut process = TerminalProcess::spawn(args, None, None)
        .await
        .expect("failed to spawn /bin/echo through a pty");

    let mut collected = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            process.read_pty_chunk(&mut buf),
        )
        .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => collected.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => break, // pty closed (EIO), normal on child exit
            Err(_) => panic!("timed out waiting for pty output"),
        }
    }

    let status = tokio::time::timeout(std::time::Duration::from_secs(5), process.wait())
        .await
        .expect("timed out waiting for process exit")
        .expect("wait() failed");
    assert!(status.success());

    let output = String::from_utf8_lossy(&collected);
    assert!(
        output.contains("hello from the pty"),
        "unexpected pty output: {output:?}"
    );
}

#[tokio::test]
async fn sighup_terminates_a_sleeping_child() {
    let args = ProcessSpawnArgs {
        argv: vec!["/bin/sleep".to_string(), "30".to_string()],
        env: vec![],
        cwd: None,
        term_rows: 24,
        term_cols: 80,
    };

    let mut process = TerminalProcess::spawn(args, None, None)
        .await
        .expect("failed to spawn /bin/sleep through a pty");

    process.send_sighup().expect("failed to send SIGHUP");

    let status = tokio::time::timeout(std::time::Duration::from_secs(5), process.wait())
        .await
        .expect("process did not exit after SIGHUP within 5s")
        .expect("wait() failed");

    assert!(!status.success());
}
