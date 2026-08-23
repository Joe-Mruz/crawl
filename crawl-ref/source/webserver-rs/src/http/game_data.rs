//! `GET /gamedata/<version>/<path>`, matching `game_data_handler.py`'s
//! `GameDataHandler`: static client assets (tiles, JS, sounds) served
//! from a per-binary-version root directory, so multiple concurrently
//! running crawl versions/binaries can serve distinct asset sets.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use tokio::sync::RwLock;

use crate::state::AppState;

/// Registry of version-hash -> asset root directory, matching
/// `GameDataHandler._client_paths`. Populated when a game process reports
/// its `client_path` (see `game::socket::ProcessControlMessage::ClientPath`
/// handling, wired up by the session/manager layer).
#[derive(Clone, Default)]
pub struct GameDataRegistry {
    paths: Arc<RwLock<HashMap<String, PathBuf>>>,
}

impl GameDataRegistry {
    pub async fn register(&self, version: impl Into<String>, path: PathBuf) {
        self.paths.write().await.insert(version.into(), path);
    }

    pub async fn resolve(&self, version: &str) -> Option<PathBuf> {
        self.paths.read().await.get(version).cloned()
    }
}

pub async fn serve(
    State(state): State<AppState>,
    AxumPath(rest): AxumPath<String>,
) -> impl IntoResponse {
    // matches the Python route regex `/gamedata/([0-9a-f]*\/.*)`: the
    // first path segment is the version hash, the remainder is the asset
    // path within that version's client_path root.
    let Some((version, path)) = rest.split_once('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(root) = state.game_data.resolve(version).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // reject path traversal defensively even though `path` comes from a
    // matched axum wildcard segment (not raw, unnormalized user input) -
    // see the Security requirements in ARCHITECTURE.md.
    if path.split('/').any(|segment| segment == "..") {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let full_path = root.join(path);
    match tokio::fs::read(&full_path).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&full_path).first_or_octet_stream();
            let mut response = (StatusCode::OK, bytes).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, mime.as_ref().parse().unwrap());
            if state.config.game_data_no_cache {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    "no-cache, no-store, must-revalidate".parse().unwrap(),
                );
            }
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_round_trips() {
        let registry = GameDataRegistry::default();
        registry.register("abc123", PathBuf::from("/tmp/game_data")).await;
        assert_eq!(
            registry.resolve("abc123").await,
            Some(PathBuf::from("/tmp/game_data"))
        );
        assert_eq!(registry.resolve("unknown").await, None);
    }
}
