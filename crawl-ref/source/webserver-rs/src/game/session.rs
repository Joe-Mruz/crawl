//! Per-game session state: watchers, chat moderation, "where" tracking,
//! and lobby-entry construction. Roughly the Rust counterpart of
//! `CrawlProcessHandlerBase`/`CrawlProcessHandler` in `process_handler.py`,
//! minus the low-level PTY/socket plumbing (that lives in
//! `game::process`/`game::socket`) and minus a few lower-priority Python
//! behaviors not yet ported (see the `NOT YET PORTED` notes below) -
//! this is a foundation to build on, not a byte-for-byte port of every
//! line of `process_handler.py`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, RwLock};

use crate::protocol::{LobbyEntry, ServerMessage};

/// If `raw` is a `{"msg":"input", "data": [codepoint, ...], "text": "..."}`
/// message, decode it to the literal keystroke bytes it represents
/// (`data` codepoints first, then `text` appended - matching Python's
/// `handle_input`: `for x in obj.get("data", []): data += chr(x); data +=
/// obj.get("text", "")`). Returns `None` for anything else (including
/// malformed `input` messages), which the caller forwards to the process
/// socket unchanged instead.
fn decode_input_message(raw: &str) -> Option<Vec<u8>> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if value.get("msg")?.as_str()? != "input" {
        return None;
    }
    let mut text = String::new();
    if let Some(codes) = value.get("data").and_then(|d| d.as_array()) {
        for code in codes {
            let code = code.as_u64()? as u32;
            text.push(char::from_u32(code)?);
        }
    }
    if let Some(t) = value.get("text").and_then(|t| t.as_str()) {
        text.push_str(t);
    }
    Some(text.into_bytes())
}

/// Per-connection outgoing mailbox. Bounded so that one slow/stuck watcher
/// cannot apply backpressure to the game process or to other watchers
/// (`ARCHITECTURE.md` "Connection Management"): a full queue means that
/// connection is falling behind and gets disconnected rather than
/// blocking the sender, matching the requirement to explicitly define
/// full-queue behavior.
pub const WATCHER_QUEUE_CAPACITY: usize = 512;

/// A unique id for one running game, matching `CrawlProcessHandlerBase.id`
/// (Python's global incrementing counter).
pub type GameId = u64;

fn next_game_id() -> GameId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A message queued for delivery to one watcher: either one of the
/// webserver's own typed messages, or an already-serialized JSON object
/// forwarded verbatim from the DCSS process socket (kept as raw text so it
/// is never re-parsed/re-serialized, per the performance requirements in
/// `ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq)]
pub enum OutgoingMessage {
    Typed(ServerMessage),
    Raw(String),
}

impl From<ServerMessage> for OutgoingMessage {
    fn from(msg: ServerMessage) -> Self {
        OutgoingMessage::Typed(msg)
    }
}

/// One registered watcher (player or spectator) of a [`GameSession`).
pub struct Watcher {
    pub connection_id: u64,
    pub username: Option<String>,
    pub is_player: bool,
    pub is_admin: bool,
    pub chat_hidden: bool,
    sender: mpsc::Sender<OutgoingMessage>,
}

impl Watcher {
    pub fn new(
        connection_id: u64,
        username: Option<String>,
        is_player: bool,
        is_admin: bool,
    ) -> (Self, mpsc::Receiver<OutgoingMessage>) {
        let (sender, receiver) = mpsc::channel(WATCHER_QUEUE_CAPACITY);
        (
            Self {
                connection_id,
                username,
                is_player,
                is_admin,
                chat_hidden: false,
                sender,
            },
            receiver,
        )
    }

    /// Enqueue a message for this watcher. Returns `false` (and does not
    /// block) if the watcher's queue is full - the caller should treat
    /// that as "this connection is not keeping up" and disconnect it,
    /// rather than stalling everyone else.
    pub fn try_send(&self, message: impl Into<OutgoingMessage>) -> bool {
        self.sender.try_send(message.into()).is_ok()
    }
}

/// "Where" info tracked per game, matching the subset of fields
/// `CrawlProcessHandlerBase.interesting_info`/`lobby_entry` expose.
#[derive(Debug, Clone, Default)]
pub struct WhereInfo {
    pub xl: Option<String>,
    pub char: Option<String>,
    pub place: Option<String>,
    pub turn: Option<String>,
    pub dur: Option<String>,
    pub god: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExitInfo {
    pub reason: Option<String>,
    pub message: Option<String>,
    pub dump_url: Option<String>,
}

/// Signal sent from a websocket connection to the task supervising this
/// game's DCSS process (`game::launch::supervise_process`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessControl {
    /// `SIGHUP`: cooperative stop, matches `CrawlProcessHandlerBase.stop`.
    Stop,
    /// `SIGABRT`: forced kill, matches `.kill()` after `kill_timeout`.
    Kill,
}

/// State for one running game. Shared behind an `Arc` + internal
/// `RwLock`s so unrelated games never contend on the same lock (see
/// `ARCHITECTURE.md` "Shared State").
pub struct GameSession {
    pub id: GameId,
    pub username: String,
    pub game_config_id: String,
    pub started_at: Instant,

    watchers: RwLock<HashMap<u64, Watcher>>,
    blocked: RwLock<HashSet<String>>,
    /// username -> (kicked_at, duration)
    kicked: RwLock<HashMap<String, (Instant, Duration)>>,
    where_info: RwLock<WhereInfo>,
    last_milestone: RwLock<Option<String>>,
    exit_info: RwLock<ExitInfo>,
    /// The rendered `game.html` (version hash, content), matching
    /// `CrawlProcessHandler.client_path`/`_send_client`. Set once - either
    /// up-front from the game config's `client_path` (the common case, at
    /// `game::launch::start_game` time) or, as a fallback, from the DCSS
    /// process's own `client_path` socket message (only if the config
    /// didn't already provide one - matches Python's
    /// `if self.client_path == None` guard). New watchers get this sent
    /// to them immediately on `add_watcher`, rather than waiting for a
    /// broadcast that may never come.
    client_html: RwLock<Option<(String, String)>>,
    /// Raw client messages to forward verbatim to the process socket
    /// (`game::launch::bridge_socket` owns the receiving end).
    input_tx: mpsc::UnboundedSender<String>,
    /// Decoded keystroke bytes to write directly to the process's PTY
    /// stdin, matching `CrawlProcessHandler.handle_input`'s special case
    /// for `{"msg":"input"}` (extracting `data`/`text` and calling
    /// `process.write_input` - unlike every other client message, which
    /// goes to the AF_UNIX game socket, `input` is never understood by
    /// that socket's protocol at all). `game::launch::supervise_process`
    /// owns the receiving end (it - not `bridge_socket` - holds the PTY).
    pty_input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Stop/kill signals for the process (`game::launch::supervise_process`
    /// owns the receiving end).
    control_tx: mpsc::UnboundedSender<ProcessControl>,
}

impl GameSession {
    /// Plain constructor for tests/lobby-only use, where nothing is
    /// actually driving a process (input/stop are accepted but silently
    /// go nowhere, since the paired receivers are dropped immediately).
    pub fn new(username: impl Into<String>, game_config_id: impl Into<String>) -> Self {
        Self::new_with_channels(username, game_config_id).0
    }

    /// Real constructor used by `game::launch::start_game`: also returns
    /// the receiving ends of the input/control channels.
    pub fn new_with_channels(
        username: impl Into<String>,
        game_config_id: impl Into<String>,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<String>,
        mpsc::UnboundedReceiver<Vec<u8>>,
        mpsc::UnboundedReceiver<ProcessControl>,
    ) {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (pty_input_tx, pty_input_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let session = Self {
            id: next_game_id(),
            username: username.into(),
            game_config_id: game_config_id.into(),
            started_at: Instant::now(),
            watchers: RwLock::new(HashMap::new()),
            blocked: RwLock::new(HashSet::new()),
            kicked: RwLock::new(HashMap::new()),
            where_info: RwLock::new(WhereInfo::default()),
            last_milestone: RwLock::new(None),
            exit_info: RwLock::new(ExitInfo::default()),
            client_html: RwLock::new(None),
            input_tx,
            pty_input_tx,
            control_tx,
        };
        (session, input_rx, pty_input_rx, control_rx)
    }

    /// Record the rendered game client (version hash + `game.html`
    /// content) if not already set. Returns `true` if this call is the
    /// one that set it (i.e. the caller should broadcast it to any
    /// already-connected watchers; a fresh session has none yet, but the
    /// process-message fallback path can race with watchers already
    /// having joined).
    pub async fn set_client_html_if_unset(&self, version: String, content: String) -> bool {
        let mut guard = self.client_html.write().await;
        if guard.is_some() {
            return false;
        }
        *guard = Some((version, content));
        true
    }

    pub async fn client_html(&self) -> Option<(String, String)> {
        self.client_html.read().await.clone()
    }

    /// Forward one raw client message to wherever it belongs: `input`
    /// messages are decoded (`data`/`text`) and written to the process's
    /// PTY stdin; everything else is forwarded verbatim to the process
    /// socket. Matches `CrawlProcessHandler.handle_input` (minus
    /// `force_terminate`/`stop_stale_process_purge`, which are
    /// webserver-only and NOT YET PORTED here - see `ARCHITECTURE.md`
    /// §4.3).
    pub fn send_input(&self, raw_message: impl Into<String>) {
        let raw_message = raw_message.into();
        if let Some(bytes) = decode_input_message(&raw_message) {
            let _ = self.pty_input_tx.send(bytes);
        } else {
            let _ = self.input_tx.send(raw_message);
        }
    }

    /// Request a cooperative stop (`SIGHUP`), matching `.stop()`.
    pub fn request_stop(&self) {
        let _ = self.control_tx.send(ProcessControl::Stop);
    }

    /// Request a forced kill (`SIGABRT`), matching `.kill()`.
    pub fn request_kill(&self) {
        let _ = self.control_tx.send(ProcessControl::Kill);
    }

    pub fn idle_time(&self) -> Duration {
        // NOT YET PORTED: Python tracks last *activity* time (keystrokes,
        // socket messages), not session age. Placeholder until input
        // activity tracking is wired up in the websocket/session-manager
        // layer.
        self.started_at.elapsed()
    }

    pub async fn add_watcher(&self, watcher: Watcher) {
        // matches `CrawlProcessHandler.add_watcher`'s immediate
        // `_send_client` call - the browser needs this to render anything
        // at all, so it must not wait for a subsequent broadcast.
        if let Some((version, content)) = self.client_html().await {
            watcher.try_send(ServerMessage::GameClient { version, content });
        }
        self.watchers.write().await.insert(watcher.connection_id, watcher);
    }

    pub async fn remove_watcher(&self, connection_id: u64) {
        self.watchers.write().await.remove(&connection_id);
    }

    /// Broadcast a message to every current watcher. Connections whose
    /// queue is full are returned so the caller can disconnect them.
    pub async fn broadcast(&self, message: impl Into<OutgoingMessage>) -> Vec<u64> {
        let message = message.into();
        let watchers = self.watchers.read().await;
        let mut overflowed = Vec::new();
        for watcher in watchers.values() {
            if !watcher.try_send(message.clone()) {
                overflowed.push(watcher.connection_id);
            }
        }
        overflowed
    }

    pub async fn watcher_count(&self) -> usize {
        self.watchers
            .read()
            .await
            .values()
            .filter(|w| !w.is_player && !w.chat_hidden)
            .count()
    }

    /// Is `username` currently blocked from spectating this game, matching
    /// `CrawlProcessHandlerBase.is_blocked` (including the special
    /// `[anon]`/`[all]` block targets and expiring timed kicks).
    pub async fn is_blocked(&self, username: Option<&str>) -> bool {
        let blocked = self.blocked.read().await;
        if blocked.contains("[all]") {
            return username != Some(self.username.as_str());
        }
        let Some(username) = username else {
            return blocked.contains("[anon]");
        };
        let mut kicked = self.kicked.write().await;
        if let Some((started, interval)) = kicked.get(username).copied() {
            if started.elapsed() < interval {
                return true;
            }
            kicked.remove(username);
        }
        blocked.contains(username)
    }

    pub async fn block(&self, target: impl Into<String>) {
        self.blocked.write().await.insert(target.into());
    }

    pub async fn unblock(&self, target: &str) {
        self.blocked.write().await.remove(target);
    }

    pub async fn kick(&self, target: impl Into<String>, minutes: u64) {
        self.kicked
            .write()
            .await
            .insert(target.into(), (Instant::now(), Duration::from_secs(minutes * 60)));
    }

    pub async fn set_where_info(&self, info: WhereInfo) {
        *self.where_info.write().await = info;
    }

    pub async fn set_last_milestone(&self, milestone: Option<String>) {
        *self.last_milestone.write().await = milestone;
    }

    pub async fn set_exit_info(&self, info: ExitInfo) {
        *self.exit_info.write().await = info;
    }

    pub async fn exit_info(&self) -> ExitInfo {
        self.exit_info.read().await.clone()
    }

    /// Build a [`LobbyEntry`], matching `CrawlProcessHandlerBase.lobby_entry`.
    pub async fn lobby_entry(&self) -> LobbyEntry {
        let where_info = self.where_info.read().await.clone();
        let last_milestone = self.last_milestone.read().await.clone();
        LobbyEntry {
            id: self.id,
            username: self.username.clone(),
            spectator_count: self.watcher_count().await as u32,
            idle_time: self.idle_time().as_secs(),
            game_id: self.game_config_id.clone(),
            xl: where_info.xl,
            char: where_info.char,
            place: where_info.place,
            turn: where_info.turn,
            dur: where_info.dur,
            god: where_info.god,
            title: where_info.title,
            milestone: last_milestone,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watcher_count_excludes_the_player_and_hidden_chatters() {
        let session = GameSession::new("alice", "dcss-web-trunk");
        let (player, _rx1) = Watcher::new(1, Some("alice".to_string()), true, false);
        let (spectator, _rx2) = Watcher::new(2, Some("bob".to_string()), false, false);
        session.add_watcher(player).await;
        session.add_watcher(spectator).await;
        assert_eq!(session.watcher_count().await, 1);
    }

    #[tokio::test]
    async fn broadcast_reaches_all_watchers() {
        let session = GameSession::new("alice", "dcss-web-trunk");
        let (w1, mut rx1) = Watcher::new(1, Some("alice".to_string()), true, false);
        let (w2, mut rx2) = Watcher::new(2, None, false, false);
        session.add_watcher(w1).await;
        session.add_watcher(w2).await;

        let overflowed = session.broadcast(ServerMessage::GameStarted).await;
        assert!(overflowed.is_empty());
        assert_eq!(
            rx1.recv().await,
            Some(OutgoingMessage::Typed(ServerMessage::GameStarted))
        );
        assert_eq!(
            rx2.recv().await,
            Some(OutgoingMessage::Typed(ServerMessage::GameStarted))
        );
    }

    #[tokio::test]
    async fn full_watcher_queue_is_reported_for_disconnection_not_blocked_on() {
        let session = GameSession::new("alice", "dcss-web-trunk");
        let (watcher, mut rx) = Watcher::new(1, None, false, false);
        session.add_watcher(watcher).await;

        for _ in 0..WATCHER_QUEUE_CAPACITY {
            session.broadcast(ServerMessage::Ping).await;
        }
        // the queue should now be full; this call must return immediately
        // (not block) and report the overflowed connection id.
        let overflowed = session.broadcast(ServerMessage::Ping).await;
        assert_eq!(overflowed, vec![1]);
        // drain so the channel doesn't leak in the test
        while rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn block_all_exempts_only_the_player() {
        let session = GameSession::new("alice", "dcss-web-trunk");
        session.block("[all]").await;
        assert!(session.is_blocked(Some("bob")).await);
        assert!(!session.is_blocked(Some("alice")).await);
    }

    #[tokio::test]
    async fn timed_kick_expires() {
        let session = GameSession::new("alice", "dcss-web-trunk");
        session.kick("bob", 0).await; // 0 minutes: expires immediately
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!session.is_blocked(Some("bob")).await);
    }

    #[tokio::test]
    async fn lobby_entry_reflects_where_info() {
        let session = GameSession::new("alice", "dcss-web-trunk");
        session
            .set_where_info(WhereInfo {
                xl: Some("5".to_string()),
                char: Some("HuFi".to_string()),
                place: Some("D:3".to_string()),
                ..Default::default()
            })
            .await;
        let entry = session.lobby_entry().await;
        assert_eq!(entry.username, "alice");
        assert_eq!(entry.xl.as_deref(), Some("5"));
        assert_eq!(entry.place.as_deref(), Some("D:3"));
    }
}
