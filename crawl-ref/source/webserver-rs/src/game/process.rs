//! DCSS game process management: PTY spawn, ttyrec recording, crash-reason
//! parsing. See `../ARCHITECTURE.md` §4.1 and `PROTOCOL.md` §5 for the
//! Python behavior this reproduces.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{Result, WebtilesError};

/// Arguments needed to start one DCSS game process, already resolved from
/// [`crate::config::ResolvedGame`] + username (i.e. `_base_call` +
/// `-webtiles-socket ... -await-connection` already appended by the
/// caller — this type does not itself know about game config).
#[derive(Debug, Clone)]
pub struct ProcessSpawnArgs {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub term_rows: u16,
    pub term_cols: u16,
}

/// One line of output from the process, tagged by which stream it came
/// from (mirrors the two separate callbacks Python registers:
/// `output_callback` for PTY/stdout, `error_callback` for the stderr
/// pipe).
#[derive(Debug, Clone)]
pub enum ProcessOutputLine {
    Stdout(String),
    Stderr(String),
}

/// A running DCSS process. Owns the PTY and the ttyrec file (if enabled);
/// output lines are delivered over an mpsc channel rather than callbacks
/// (idiomatic replacement for Python's callback-attribute style).
pub struct TerminalProcess {
    pty: pty_process::Pty,
    child: tokio::process::Child,
    pub output_rx: mpsc::UnboundedReceiver<ProcessOutputLine>,
}

impl TerminalProcess {
    /// Spawn the process. `ttyrec` is an already-opened file to append
    /// chunk-framed recording data to (or `None` if `enable_ttyrecs` is
    /// off); `id_header` is written as the very first ttyrec chunk,
    /// matching `TerminalRecorder.start`.
    pub async fn spawn(
        args: ProcessSpawnArgs,
        mut ttyrec: Option<tokio::fs::File>,
        id_header: Option<Vec<u8>>,
    ) -> Result<Self> {
        let Some((program, rest)) = args.argv.split_first() else {
            return Err(WebtilesError::Process("empty argv".to_string()));
        };

        let pty = pty_process::Pty::new()
            .map_err(|e| WebtilesError::Process(format!("failed to allocate a pty: {e}")))?;
        pty.resize(pty_process::Size::new(args.term_rows, args.term_cols))
            .map_err(|e| WebtilesError::Process(format!("failed to size the pty: {e}")))?;
        let pts = pty
            .pts()
            .map_err(|e| WebtilesError::Process(format!("failed to open the pty slave: {e}")))?;

        let mut command = pty_process::Command::new(program);
        command.args(rest);
        command.env("COLUMNS", args.term_cols.to_string());
        command.env("LINES", args.term_rows.to_string());
        command.env("TERM", "linux");
        for (key, value) in &args.env {
            command.env(key, value);
        }
        if let Some(cwd) = &args.cwd {
            command.current_dir(cwd);
        }
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn(&pts)
            .map_err(|e| WebtilesError::Process(format!("failed to spawn game process: {e}")))?;

        if let Some(header) = id_header {
            if let Some(f) = ttyrec.as_mut() {
                write_ttyrec_chunk(f, &header).await?;
                f.flush().await?;
            }
        }

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| WebtilesError::Process("child stderr was not captured".to_string()))?;

        let (tx, rx) = mpsc::unbounded_channel();

        // Stderr reader task: line-buffered, tagged, forwarded to the
        // channel. Errors here just end the task; the process itself is
        // supervised via `child.wait()` by the caller.
        let stderr_tx = tx.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                        if !trimmed.is_empty() && stderr_tx.send(ProcessOutputLine::Stderr(trimmed)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            pty,
            child,
            output_rx: rx,
        })
    }

    /// Write raw bytes to the pty (player keystrokes forwarded from the
    /// webserver during the stdout-bootstrap phase, before the game's own
    /// Unix socket takes over — see `ARCHITECTURE.md` §4.1/§4.2).
    pub async fn write_input(&mut self, data: &[u8]) -> Result<()> {
        self.pty
            .write_all(data)
            .await
            .map_err(|e| WebtilesError::Process(format!("failed to write pty input: {e}")))
    }

    /// Read one chunk from the PTY master. Returns `Ok(0)` at EOF (process
    /// closed its end). Callers are expected to drive this in a loop,
    /// writing each chunk to the ttyrec file and line-splitting it for
    /// `ProcessOutputLine::Stdout` delivery (kept out of this method so the
    /// ttyrec-writing and ttyrec-disabled cases share one read loop without
    /// duplicating buffering logic).
    pub async fn read_pty_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.pty
            .read(buf)
            .await
            .map_err(|e| WebtilesError::Process(format!("failed to read pty output: {e}")))
    }

    /// Send `SIGHUP` (cooperative stop — DCSS auto-saves and quits).
    pub fn send_sighup(&self) -> Result<()> {
        self.send_signal(libc::SIGHUP)
    }

    /// Send `SIGABRT` (forced kill after `kill_timeout`, see
    /// `ARCHITECTURE.md` §4.1).
    pub fn send_sigabrt(&self) -> Result<()> {
        self.send_signal(libc::SIGABRT)
    }

    fn send_signal(&self, signal: i32) -> Result<()> {
        let Some(pid) = self.child.id() else {
            return Err(WebtilesError::Process(
                "process has already exited".to_string(),
            ));
        };
        // SAFETY: `pid` is a valid process id obtained from `Child::id()`
        // for a child we are still holding a handle to (or has just
        // exited, in which case `kill` harmlessly returns ESRCH).
        let result = unsafe { libc::kill(pid as i32, signal) };
        if result != 0 {
            return Err(WebtilesError::Process(format!(
                "kill({pid}, {signal}) failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Wait for the process to exit, returning its raw exit/signal status.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        self.child
            .wait()
            .await
            .map_err(|e| WebtilesError::Process(format!("waitpid failed: {e}")))
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

/// Append one ttyrec-framed chunk: a 12-byte little-endian
/// `<seconds:4><microseconds:4><length:4>` header (matching
/// `TerminalRecorder.write_ttyrec_header`'s `struct.pack("<iii", ...)`)
/// followed by the raw bytes.
pub async fn write_ttyrec_chunk(file: &mut tokio::fs::File, data: &[u8]) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let sec = now.as_secs() as i32;
    let usec = now.subsec_micros() as i32;
    let len = data.len() as i32;

    let mut header = Vec::with_capacity(12);
    header.extend_from_slice(&sec.to_le_bytes());
    header.extend_from_slice(&usec.to_le_bytes());
    header.extend_from_slice(&len.to_le_bytes());

    file.write_all(&header).await?;
    file.write_all(data).await?;
    Ok(())
}

/// Build the ttyrec id-header chunk written once at process start,
/// matching `CrawlProcessHandler._ttyrec_id_header`.
pub fn ttyrec_id_header(
    username: &str,
    game_name: &str,
    server_id: &str,
    lock_basename: &str,
) -> Vec<u8> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "\x1b[2J\x1b[1;1H\r\nPlayer: {username}\r\nGame: {game_name}\r\nServer: {server_id}\r\nFilename: {lock_basename}\r\nTime: ({epoch}) epoch-seconds\r\n\x1b[2J",
        epoch = now.as_secs(),
    )
    .into_bytes()
}

/// Structured crash-reason info, matching what
/// `CrawlProcessHandler._on_process_error` extracts from stderr lines (the
/// format DCSS's `dbg-asrt.cc: do_crash_dump` writes). See `PROTOCOL.md` §4
/// for the corresponding `exit_reason`/`game_ended` fields this feeds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CrashInfo {
    pub reason: Option<String>,
    pub message: Option<String>,
    pub dump_filename: Option<String>,
}

/// Incrementally update `info` given the next stderr line, matching the
/// four heuristics `_on_process_error` checks (across DCSS versions old
/// and new). Call once per stderr line as it arrives; the final `info`
/// after the process exits becomes the crash-related fields of
/// `game_ended` (unless a `milestone`/`exit_reason` socket message already
/// supplied a more precise reason first — that takes priority upstream).
pub fn update_crash_info(info: &mut CrashInfo, line: &str, morgue_url: Option<&str>) {
    if let Some(rest) = line.strip_prefix("ERROR") {
        info.reason = Some("crash".to_string());
        if let Some(idx) = rest.rfind(':') {
            info.message = Some(rest[idx + 1..].trim().to_string());
        }
    } else if let Some(idx) = line.find("crash report: ") {
        info.reason = Some("crash".to_string());
        if let Some(url) = morgue_url {
            let path = &line[idx + "crash report: ".len()..];
            if !path.is_empty() {
                info.dump_filename = Some(format!("{url}{}", strip_extension(basename(path))));
            }
        }
    } else if line.starts_with("We crashed!") {
        info.reason = Some("crash".to_string());
        if let Some(url) = morgue_url {
            if let Some(open) = line.find('(') {
                if let Some(close) = line[open..].find(')') {
                    let inner = &line[open + 1..open + close];
                    info.dump_filename = Some(format!("{url}{}", strip_extension(basename(inner))));
                }
            }
        }
    } else if line.starts_with("Writing crash info to") {
        info.reason = Some("crash".to_string());
        if let Some(url) = morgue_url {
            let tail = line
                .rfind('/')
                .map(|i| &line[i + 1..])
                .or_else(|| line.rfind(' ').map(|i| &line[i + 1..]))
                .map(|s| s.trim());
            if let Some(name) = tail {
                info.dump_filename = Some(format!("{url}{}", strip_extension(name)));
            }
        }
    }
}

pub(crate) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(crate) fn strip_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(idx) => &name[..idx],
        None => name,
    }
}

/// The 3-line lock file format written by `gen_inprogress_lock`:
/// `<pid>\n<lines>\n<cols>\n`.
pub fn format_lock_file(pid: u32, term_rows: u16, term_cols: u16) -> String {
    format!("{pid}\n{term_rows}\n{term_cols}\n")
}

/// Parse a lock file back into `(pid, rows, cols)`, matching
/// `_purge_locks_and_start`'s read of an existing lock.
pub fn parse_lock_file(contents: &str) -> Option<(u32, u16, u16)> {
    let mut lines = contents.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let rows: u16 = lines.next()?.trim().parse().ok()?;
    let cols: u16 = lines.next()?.trim().parse().ok()?;
    Some((pid, rows, cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_header_line_extracts_message() {
        let mut info = CrashInfo::default();
        update_crash_info(
            &mut info,
            "ERROR in 'wizard.cc' at line 79: Intentional crash",
            None,
        );
        assert_eq!(info.reason.as_deref(), Some("crash"));
        assert_eq!(info.message.as_deref(), Some("Intentional crash"));
    }

    #[test]
    fn crash_report_line_builds_dump_url() {
        let mut info = CrashInfo::default();
        update_crash_info(
            &mut info,
            "crash report: /rcs/alice/crash-alice-20240101-000000.txt",
            Some("http://example.com/morgue/alice/"),
        );
        assert_eq!(
            info.dump_filename.as_deref(),
            Some("http://example.com/morgue/alice/crash-alice-20240101-000000")
        );
    }

    #[test]
    fn we_crashed_legacy_format_extracts_paren_group() {
        let mut info = CrashInfo::default();
        update_crash_info(
            &mut info,
            "We crashed! (crash-alice-20240101-000000.txt)",
            Some("http://example.com/morgue/alice/"),
        );
        assert_eq!(
            info.dump_filename.as_deref(),
            Some("http://example.com/morgue/alice/crash-alice-20240101-000000")
        );
    }

    #[test]
    fn writing_crash_info_legacy_format() {
        let mut info = CrashInfo::default();
        update_crash_info(
            &mut info,
            "Writing crash info to /rcs/alice/crash-alice-20240101-000000.txt",
            Some("http://example.com/morgue/alice/"),
        );
        assert_eq!(
            info.dump_filename.as_deref(),
            Some("http://example.com/morgue/alice/crash-alice-20240101-000000")
        );
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let mut info = CrashInfo::default();
        update_crash_info(&mut info, "just some ordinary stderr chatter", None);
        assert_eq!(info, CrashInfo::default());
    }

    #[test]
    fn lock_file_round_trips() {
        let text = format_lock_file(12345, 24, 80);
        assert_eq!(text, "12345\n24\n80\n");
        assert_eq!(parse_lock_file(&text), Some((12345, 24, 80)));
    }

    #[test]
    fn corrupt_lock_file_fails_to_parse() {
        assert_eq!(parse_lock_file("not-a-pid\n24\n80\n"), None);
        assert_eq!(parse_lock_file(""), None);
    }

    #[tokio::test]
    async fn ttyrec_chunk_has_correct_12_byte_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ttyrec");
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        write_ttyrec_chunk(&mut file, b"hello").await.unwrap();
        drop(file);

        let bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes.len(), 12 + 5);
        let len = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(len, 5);
        assert_eq!(&bytes[12..], b"hello");
    }

    #[test]
    fn ttyrec_id_header_contains_expected_fields() {
        let header = ttyrec_id_header("alice", "Play Trunk", "myserver", "2024-01-01.00:00:00.ttyrec");
        let text = String::from_utf8(header).unwrap();
        assert!(text.contains("Player: alice"));
        assert!(text.contains("Game: Play Trunk"));
        assert!(text.contains("Server: myserver"));
        assert!(text.contains("Filename: 2024-01-01.00:00:00.ttyrec"));
    }
}
