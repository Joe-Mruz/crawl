//! HTTP layer: Axum router + handlers, matching the endpoints Tornado
//! registers in `webtiles/server.py:bind_server`. See `../ARCHITECTURE.md`
//! §2 and `PROTOCOL.md` §8.

pub mod assets;
pub mod game_data;
pub mod handlers;
pub mod templates;

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the full application router.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::main_page))
        .route("/socket", get(crate::websocket::upgrade))
        .route("/gamedata/{*rest}", get(game_data::serve))
        .route("/status/lobby/", get(handlers::status_lobby))
        .route("/status/version/", get(handlers::status_version))
        .route("/static/{*file}", get(assets::serve_static))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
