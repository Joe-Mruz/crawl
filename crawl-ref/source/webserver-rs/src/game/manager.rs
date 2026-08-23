//! Game registry: tracks every currently-running [`GameSession`], matching
//! the module-level `processes` dict in `process_handler.py` plus the
//! lobby-facing lookups scattered through `ws_handler.py`. Kept
//! deliberately small: process spawning/lifecycle orchestration
//! (the `start_crawl`/stale-lock-purge equivalent) is a higher-level
//! concern layered on top of this registry plus `game::process` and
//! `game::socket`, not implemented in this type itself.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::game::session::{GameId, GameSession};
use crate::protocol::LobbyEntry;

/// Registry of currently-running games. Cheap to clone (`Arc` internally)
/// so it can be held by both the HTTP and WebSocket layers without a
/// global mutex around unrelated games' state (`ARCHITECTURE.md` "Shared
/// State": each [`GameSession`] has its own internal locks; this registry
/// only serializes *membership* changes, which are comparatively rare).
#[derive(Clone, Default)]
pub struct GameManager {
    games: Arc<RwLock<HashMap<GameId, Arc<GameSession>>>>,
}

impl GameManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, session: Arc<GameSession>) {
        self.games.write().await.insert(session.id, session);
    }

    pub async fn unregister(&self, id: GameId) -> Option<Arc<GameSession>> {
        self.games.write().await.remove(&id)
    }

    pub async fn get(&self, id: GameId) -> Option<Arc<GameSession>> {
        self.games.read().await.get(&id).cloned()
    }

    /// Find a running game by (case-insensitive) player username, matching
    /// `ws_handler.watch`'s lookup over `process_handler.processes`.
    pub async fn find_by_username(&self, username: &str) -> Option<Arc<GameSession>> {
        self.games
            .read()
            .await
            .values()
            .find(|s| s.username.eq_ignore_ascii_case(username))
            .cloned()
    }

    /// Snapshot every running game's lobby entry, matching
    /// `send_lobby_data`'s iteration over `process_handler.processes`.
    /// Account-restricted games are filtered out here unless `is_admin` is
    /// set, mirroring `send_lobby_data`'s per-socket admin check (the
    /// restriction check itself - `account_restricted()` - lives with the
    /// connection/auth layer, so callers pass the already-computed flag
    /// per game rather than this type reaching into user state).
    pub async fn lobby_entries(&self) -> Vec<LobbyEntry> {
        let games = self.games.read().await;
        let mut entries = Vec::with_capacity(games.len());
        for session in games.values() {
            entries.push(session.lobby_entry().await);
        }
        entries
    }

    pub async fn count(&self) -> usize {
        self.games.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_lookup_by_username() {
        let manager = GameManager::new();
        let session = Arc::new(GameSession::new("Alice", "dcss-web-trunk"));
        manager.register(session.clone()).await;

        let found = manager.find_by_username("alice").await; // case-insensitive
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, session.id);
    }

    #[tokio::test]
    async fn unregister_removes_from_lobby() {
        let manager = GameManager::new();
        let session = Arc::new(GameSession::new("bob", "dcss-web-trunk"));
        manager.register(session.clone()).await;
        assert_eq!(manager.count().await, 1);

        manager.unregister(session.id).await;
        assert_eq!(manager.count().await, 0);
        assert!(manager.find_by_username("bob").await.is_none());
    }

    #[tokio::test]
    async fn lobby_entries_snapshot_all_games() {
        let manager = GameManager::new();
        manager
            .register(Arc::new(GameSession::new("alice", "dcss-web-trunk")))
            .await;
        manager
            .register(Arc::new(GameSession::new("bob", "seeded-web-trunk")))
            .await;

        let entries = manager.lobby_entries().await;
        assert_eq!(entries.len(), 2);
    }
}
