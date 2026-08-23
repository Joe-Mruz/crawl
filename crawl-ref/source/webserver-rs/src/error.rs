//! Centralized error types. Distinguish error *categories* so that call
//! sites (HTTP handlers, websocket loop, game manager) can decide what to
//! expose to a client vs. what to only log.

use thiserror::Error;

/// Top level error type for anything that can go wrong while serving a
/// WebTiles connection. Deliberately not a single "catch-all" enum member -
/// each variant maps to a distinct cause so logs and (where applicable)
/// client-visible messages stay accurate.
#[derive(Debug, Error)]
pub enum WebtilesError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("game error: {0}")]
    Game(String),

    #[error("websocket error: {0}")]
    WebSocket(#[from] axum::Error),

    #[error("subprocess error: {0}")]
    Process(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("internal server error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, WebtilesError>;
