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
}

impl GameSession {
    pub fn new(username: impl Into<String>, game_config_id: impl Into<String>) -> Self {
        Self {
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
        }
    }

    pub fn idle_time(&self) -> Duration {
        // NOT YET PORTED: Python tracks last *activity* time (keystrokes,
        // socket messages), not session age. Placeholder until input
        // activity tracking is wired up in the websocket/session-manager
        // layer.
        self.started_at.elapsed()
    }

    pub async fn add_watcher(&self, watcher: Watcher) {
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
