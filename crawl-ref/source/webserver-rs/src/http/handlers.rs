//! HTTP handlers, matching `MainHandler`/`status.LobbyHandler`/
//! `status.VersionHandler` in the Python implementation.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Json};

use crate::http::templates::{render_file, TemplateContext};
use crate::state::AppState;

/// `GET /`: renders `client.html`. See `../../ARCHITECTURE.md` §2 and
/// `http/templates.rs` for the (purpose-built, non-Tornado) template
/// engine backing this.
pub async fn main_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let is_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "https")
        .unwrap_or(false);
    let protocol = if is_https { "wss://" } else { "ws://" };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let socket_server = format!("{protocol}{host}/socket");

    let mut ctx = TemplateContext::default()
        .with_string("socket_server", socket_server)
        .with_string("game_version", env!("CARGO_PKG_VERSION"))
        // Python's template aliases `game_version` to this name via
        // `{% set fail_safe_game_version = globals().get('game_version', '') %}`;
        // our template engine strips `{% set %}` as a no-op, so the alias
        // is provided directly instead.
        .with_string("fail_safe_game_version", env!("CARGO_PKG_VERSION"))
        .with_bool("allow_password_reset", state.config.allow_password_reset)
        .with_bool("admin_password_reset", state.config.admin_password_reset);

    if let Some(token) = params.get("ResetToken") {
        // NOT YET PORTED: full recovery-token lookup/expiry semantics
        // (userdb.rs does not yet implement the `recovery_tokens` table
        // queries `userdb.find_recovery_token` performs); this currently
        // always treats a present token as valid.
        ctx = ctx.with_bool("reset_token", true).with_string("reset_token", token);
    }

    match render_file(&state.config.template_path, "client.html", &ctx) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "failed to render client.html");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "template rendering error",
            )
                .into_response()
        }
    }
}

/// `GET /status/version/`, matching `status.VersionHandler`. Field names
/// are intentionally renamed for the Rust implementation
/// (`tornado`->`axum`, `python`->`rust`) - documented as an intentional,
/// client-invisible difference in `PROTOCOL.md` §8.
pub async fn status_version() -> impl IntoResponse {
    Json(serde_json::json!({
        "webtiles": env!("CARGO_PKG_VERSION"),
        "axum": "0.8",
        "rust": rustc_version(),
        "rust_supported": true,
    }))
}

fn rustc_version() -> &'static str {
    option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("unknown")
}

/// `GET /status/lobby/`, matching `status.LobbyHandler`.
///
/// Simplification: reuses the internal [`crate::protocol::LobbyEntry`]
/// shape (same one sent over the websocket `lobby_entry` message) rather
/// than reproducing Python's slightly different ad-hoc JSON schema for
/// this endpoint (`name`/`viewers`/`watch_url` plus flattened `v`, `vlong`,
/// `tiles`, `race`, `cls` fields not currently tracked here). Any external
/// dashboard consuming this endpoint's exact field names would need
/// updating - see `PROTOCOL.md` §8.
pub async fn status_lobby(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.games.lobby_entries().await)
}
