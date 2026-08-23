//! The crawl↔webtiles protocol socket: a `SOCK_DGRAM` Unix domain socket
//! the webserver uses to talk to one running DCSS process. See
//! `../ARCHITECTURE.md` §4.2 and `PROTOCOL.md` §1/§4 for the wire format
//! this implements (newline-terminated JSON datagrams, `*`-prefixed
//! control messages, an `attach` handshake).

use std::path::{Path, PathBuf};

use rand::Rng;
use tokio::net::UnixDatagram;

use crate::error::{Result, WebtilesError};

/// Maximum size of one read, matching Python's
/// `self.socket.recv(128 * 1024, MSG_DONTWAIT)`.
const RECV_BUFFER_SIZE: usize = 128 * 1024;

/// The webserver's end of the protocol socket for one game process.
/// Binds its own throwaway datagram socket and `sendto()`s to the DCSS
/// process's socket path - matching `WebtilesSocketConnection`, which does
/// not use `connect(2)` (the "connection" is purely a shared path
/// convention, not a kernel-level association).
pub struct GameSocket {
    socket: UnixDatagram,
    own_path: PathBuf,
    peer_path: PathBuf,
    /// Holds a partial datagram if a previous `recv` did not end in `\n`
    /// (see `PROTOCOL.md` §1: fragmentation is rare but must be handled).
    fragment_buffer: Vec<u8>,
}

impl GameSocket {
    /// Bind an ephemeral socket under `socket_dir` (Python:
    /// `tempfile.mktemp(dir=server_socket_path, prefix="crawl", suffix=".socket")`,
    /// reimplemented here without the TOCTOU-prone `mktemp` by retrying
    /// `bind` on `EEXIST` with a fresh random name), then send the
    /// `attach` handshake to `peer_path` (the DCSS process's own socket,
    /// created by the process itself via `-webtiles-socket`).
    pub async fn connect(socket_dir: &Path, peer_path: &Path, primary: bool) -> Result<Self> {
        tokio::fs::create_dir_all(socket_dir).await.ok();

        let mut last_err = None;
        for _ in 0..8 {
            let candidate = socket_dir.join(format!("crawl{}.socket", random_suffix()));
            match UnixDatagram::bind(&candidate) {
                Ok(socket) => {
                    let this = Self {
                        socket,
                        own_path: candidate,
                        peer_path: peer_path.to_path_buf(),
                        fragment_buffer: Vec::new(),
                    };
                    this.send_json(&serde_json::json!({"msg": "attach", "primary": primary}))
                        .await?;
                    return Ok(this);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(WebtilesError::Io(e)),
            }
        }
        Err(WebtilesError::Process(format!(
            "failed to bind an ephemeral game socket after 8 attempts: {:?}",
            last_err
        )))
    }

    /// Send a raw, already-encoded message (used both for typed control
    /// messages built here and for opaque client messages forwarded
    /// byte-for-byte, per `PROTOCOL.md` §4).
    pub async fn send_raw(&self, data: &[u8]) -> Result<()> {
        self.socket
            .send_to(data, &self.peer_path)
            .await
            .map_err(WebtilesError::Io)?;
        Ok(())
    }

    async fn send_json(&self, value: &serde_json::Value) -> Result<()> {
        let text = serde_json::to_vec(value)?;
        self.send_raw(&text).await
    }

    /// Receive the next complete, newline-stripped message. Transparently
    /// buffers and re-assembles a message that arrived split across
    /// multiple datagrams (see `PROTOCOL.md` §1).
    pub async fn recv_message(&mut self) -> Result<Vec<u8>> {
        loop {
            let mut buf = vec![0u8; RECV_BUFFER_SIZE];
            let n = self.socket.recv(&mut buf).await.map_err(WebtilesError::Io)?;
            buf.truncate(n);

            let data = if self.fragment_buffer.is_empty() {
                buf
            } else {
                let mut combined = std::mem::take(&mut self.fragment_buffer);
                combined.extend_from_slice(&buf);
                combined
            };

            if data.last() == Some(&b'\n') {
                let mut data = data;
                data.pop();
                return Ok(data);
            }
            // Not newline-terminated: DCSS always terminates messages with
            // `\n`, so this must be a fragment - hold it and read more.
            self.fragment_buffer = data;
        }
    }

    pub fn own_path(&self) -> &Path {
        &self.own_path
    }
}

impl Drop for GameSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.own_path);
    }
}

fn random_suffix() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// The subset of messages sent *from* the DCSS process that the webserver
/// intercepts rather than forwarding to the browser (identified by a
/// leading `*` on the wire, stripped before parsing - see `PROTOCOL.md`
/// §4).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ProcessControlMessage {
    ClientPath {
        path: String,
        #[serde(default)]
        version: Option<String>,
    },
    FlushMessages,
    Dump {
        #[serde(rename = "type")]
        kind: String,
        filename: String,
    },
    ExitReason {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        message: Option<String>,
    },
    Milestone {
        #[serde(flatten)]
        fields: serde_json::Map<String, serde_json::Value>,
    },
}

/// A message read from the process socket, either intercepted or meant to
/// be forwarded byte-for-byte to the browser.
#[derive(Debug, Clone, PartialEq)]
pub enum FromProcessMessage {
    Control(ProcessControlMessage),
    /// Forward this exact byte sequence to the browser (no re-encoding),
    /// per the performance requirement in `ARCHITECTURE.md`.
    ForwardVerbatim(Vec<u8>),
}

/// Classify one message read via [`GameSocket::recv_message`], matching
/// `_on_socket_message`'s `msg.startswith("*")` check.
pub fn classify_process_message(data: Vec<u8>) -> Result<FromProcessMessage> {
    if data.first() == Some(&b'*') {
        let without_prefix = &data[1..];
        let control: ProcessControlMessage = serde_json::from_slice(without_prefix)?;
        Ok(FromProcessMessage::Control(control))
    } else {
        Ok(FromProcessMessage::ForwardVerbatim(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_client_path_control_message() {
        let msg = br#"*{"msg":"client_path","path":"/x/y","version":"abc123"}"#.to_vec();
        let classified = classify_process_message(msg).unwrap();
        assert_eq!(
            classified,
            FromProcessMessage::Control(ProcessControlMessage::ClientPath {
                path: "/x/y".to_string(),
                version: Some("abc123".to_string()),
            })
        );
    }

    #[test]
    fn classifies_flush_messages() {
        let msg = br#"*{"msg":"flush_messages"}"#.to_vec();
        assert_eq!(
            classify_process_message(msg).unwrap(),
            FromProcessMessage::Control(ProcessControlMessage::FlushMessages)
        );
    }

    #[test]
    fn unprefixed_messages_are_forwarded_verbatim() {
        let msg = br#"{"msg":"map","cells":[]}"#.to_vec();
        assert_eq!(
            classify_process_message(msg.clone()).unwrap(),
            FromProcessMessage::ForwardVerbatim(msg)
        );
    }

    #[tokio::test]
    async fn attach_handshake_round_trips_over_real_unix_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let peer_path = dir.path().join("peer.sock");
        let peer = UnixDatagram::bind(&peer_path).unwrap();

        let client = GameSocket::connect(dir.path(), &peer_path, true)
            .await
            .expect("failed to connect (does this sandbox allow AF_UNIX sockets?)");

        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv(&mut buf))
            .await
            .expect("timed out waiting for attach handshake")
            .unwrap();
        let received: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
        assert_eq!(received["msg"], "attach");
        assert_eq!(received["primary"], true);
        drop(client);
    }
}
