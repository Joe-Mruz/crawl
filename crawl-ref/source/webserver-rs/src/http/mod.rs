//! HTTP layer: Axum router + handlers, matching the endpoints Tornado
//! registers in `webtiles/server.py:bind_server`. See `../ARCHITECTURE.md`
//! §2 and `PROTOCOL.md` §8.

pub mod game_data;
pub mod handlers;
pub mod templates;

use axum::routing::get;
use axum::Router;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the full application router.
pub fn build_router(state: AppState) -> Router {
    let static_path = state.config.static_path.clone();

    Router::new()
        .route("/", get(handlers::main_page))
        .route("/socket", get(crate::websocket::upgrade))
        .route("/gamedata/{*rest}", get(game_data::serve))
        .route("/status/lobby/", get(handlers::status_lobby))
        .route("/status/version/", get(handlers::status_version))
        .nest_service("/static", ServeDir::new(static_path))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
