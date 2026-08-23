//! Shared application state, matching the `ApplicationState` shape called
//! for in `ARCHITECTURE.md`/the task brief: configuration, the game
//! registry, authentication state, and server-wide resources, each with
//! its own internal synchronization rather than one global lock.

use std::sync::Arc;

use crate::auth::LoginTokenStore;
use crate::config::ServerConfig;
use crate::game::manager::GameManager;
use crate::http::game_data::GameDataRegistry;
use crate::userdb::UserDb;

/// Cloneable handle to all server-wide shared resources. Cheap to clone
/// (every field is an `Arc` or already `Clone`-cheap internally), so it can
/// be passed into Axum handlers and per-connection WebSocket tasks
/// directly as `axum::extract::State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub games: GameManager,
    pub login_tokens: Arc<LoginTokenStore>,
    pub users: Arc<UserDb>,
    pub game_data: GameDataRegistry,
}

impl AppState {
    pub fn new(config: ServerConfig, users: UserDb) -> Self {
        Self {
            config: Arc::new(config),
            games: GameManager::new(),
            login_tokens: Arc::new(LoginTokenStore::new()),
            users: Arc::new(users),
            game_data: GameDataRegistry::default(),
        }
    }
}
