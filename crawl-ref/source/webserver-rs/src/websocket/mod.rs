//! Axum WebSocket transport for the `/socket` endpoint. Transport
//! concerns (upgrade handshake) live here; protocol decoding and
//! game/session logic live in `connection.rs`, kept separate per
//! `ARCHITECTURE.md`'s layering requirement.

pub mod connection;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::response::IntoResponse;

use crate::state::AppState;

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| connection::handle_socket(socket, state))
}
