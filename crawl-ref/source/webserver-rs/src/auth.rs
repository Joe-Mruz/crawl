//! Authentication: in-memory login tokens and password hashing.
//!
//! Password hashing uses [`argon2`] (Argon2id, via the RustCrypto
//! `password-hash` PHC-string traits) for every password created or changed
//! through this server — a deliberate improvement over the Python
//! implementation's `crypt(3)`-based schemes (traditional DES, or
//! MD5/SHA-256/SHA-512 crypt), which are legacy, non-memory-hard, and in
//! one configurable mode (`crypt_algorithm = "broken"`) use the password
//! itself as the salt. There is no requirement to preserve that design; see
//! `ARCHITECTURE.md` §5 for why it existed in Python.
//!
//! An existing deployment migrating its `passwd.db3` from the Python server
//! still has accounts whose stored hash is in one of those legacy crypt
//! formats. To avoid forcing a mass password reset, [`verify_password`]
//! recognizes a stored hash's format from its prefix and verifies it with
//! the matching pure-Rust implementation from the `pwhash` crate (no
//! `unsafe`, no system `libcrypt` linking). Callers are expected to
//! transparently upgrade a successful legacy verification to an Argon2
//! hash (see [`needs_rehash`]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;
use tokio::sync::RwLock;

use crate::error::WebtilesError;

/// Hash a new/changed password. Always produces an Argon2id PHC string
/// (e.g. `$argon2id$v=19$m=19456,t=2,p=1$...`), regardless of what a
/// migrated config's (now-legacy) `crypt_algorithm` setting says.
pub fn hash_password(password: &str) -> crate::error::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| WebtilesError::Auth(format!("failed to hash password: {e}")))
}

/// Verify a password against a stored hash, whatever format that hash is
/// in (current Argon2, or a legacy crypt(3)-style hash inherited from a
/// migrated Python-server database).
pub fn verify_password(password: &str, stored_hash: &str) -> crate::error::Result<bool> {
    if stored_hash.starts_with("$argon2") {
        let parsed = PasswordHash::new(stored_hash)
            .map_err(|e| WebtilesError::Auth(format!("corrupt password hash: {e}")))?;
        return Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok());
    }

    Ok(verify_legacy_crypt(password, stored_hash))
}

/// `true` if `stored_hash` is in a legacy crypt(3) format and should be
/// replaced with a fresh Argon2 hash the next time this password is
/// successfully verified (transparent migration on login).
pub fn needs_rehash(stored_hash: &str) -> bool {
    !stored_hash.starts_with("$argon2")
}

/// Verify against the legacy formats the Python implementation could have
/// produced (`ARCHITECTURE.md` §5): traditional DES crypt (bare 2-char
/// salt, used by `crypt_algorithm = "broken"`), and glibc
/// MD5/SHA-256/SHA-512 crypt (`$1$`/`$5$`/`$6$`). Implemented with the
/// pure-Rust `pwhash` crate - no FFI, no system `libcrypt` dependency.
fn verify_legacy_crypt(password: &str, stored_hash: &str) -> bool {
    use pwhash::{md5_crypt, sha256_crypt, sha512_crypt, unix_crypt};

    if stored_hash.starts_with("$6$") {
        sha512_crypt::verify(password, stored_hash)
    } else if stored_hash.starts_with("$5$") {
        sha256_crypt::verify(password, stored_hash)
    } else if stored_hash.starts_with("$1$") {
        md5_crypt::verify(password, stored_hash)
    } else {
        // No recognized `$id$` prefix: traditional DES crypt, a bare
        // 2-character salt (this is what `crypt_algorithm = "broken"`
        // produced, using the password itself as the salt).
        unix_crypt::verify(password, stored_hash)
    }
}

/// In-memory login-token store, matching `auth.py`'s `login_tokens` dict:
/// `(token, username) -> expiry`. Tokens are 128-bit random values, minted
/// on `set_login_cookie` and consumed (deleted) by `token_login`/logout.
#[derive(Debug, Default)]
pub struct LoginTokenStore {
    tokens: RwLock<HashMap<(u128, String), Instant>>,
}

impl LoginTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a new token for `username`, valid for `lifetime`. Returns the
    /// token formatted as the `"<username>%20<token>"` cookie value Python
    /// uses (see `auth.log_in_as_user`).
    pub async fn issue(&self, username: &str, lifetime: Duration) -> String {
        let token: u128 = rand::Rng::gen(&mut rand::thread_rng());
        let expires = Instant::now() + lifetime;
        self.tokens
            .write()
            .await
            .insert((token, username.to_string()), expires);
        format!("{username}%20{token}")
    }

    /// Validate and (matching Python's `token_login` flow) forget a cookie
    /// in one step; returns the username if valid.
    pub async fn consume(&self, cookie: &str) -> Option<String> {
        let (username, token) = parse_cookie(cookie)?;
        let key = (token, username.clone());
        let mut tokens = self.tokens.write().await;
        if let std::collections::hash_map::Entry::Occupied(entry) = tokens.entry(key) {
            if Instant::now() <= *entry.get() {
                entry.remove();
                return Some(username);
            }
            entry.remove();
        }
        None
    }

    /// Forget a cookie without checking validity (matches
    /// `forget_login_cookie`, called on explicit client logout).
    pub async fn forget(&self, cookie: &str) {
        if let Some((username, token)) = parse_cookie(cookie) {
            self.tokens.write().await.remove(&(token, username));
        }
    }

    /// Drop every expired token. Intended to be called on a periodic timer
    /// (Python does this hourly).
    pub async fn purge_expired(&self) {
        let now = Instant::now();
        self.tokens.write().await.retain(|_, expires| *expires > now);
    }
}

fn parse_cookie(cookie: &str) -> Option<(String, u128)> {
    let (username, token_str) = cookie.split_once("%20")?;
    let token: u128 = token_str.parse().ok()?;
    Some((username.to_string(), token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_round_trips() {
        let hash = hash_password("hunter2").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("hunter2", &hash).unwrap());
        assert!(!verify_password("wrong", &hash).unwrap());
        assert!(!needs_rehash(&hash));
    }

    #[test]
    fn legacy_sha512_crypt_hash_still_verifies() {
        // a hash as it would exist in a passwd.db3 migrated from the
        // Python server (crypt_algorithm = 6).
        let legacy_hash = pwhash::sha512_crypt::hash("hunter2").unwrap();
        assert!(legacy_hash.starts_with("$6$"));
        assert!(verify_password("hunter2", &legacy_hash).unwrap());
        assert!(!verify_password("wrong", &legacy_hash).unwrap());
        assert!(needs_rehash(&legacy_hash));
    }

    #[test]
    fn legacy_broken_scheme_des_crypt_still_verifies() {
        // crypt_algorithm = "broken": salt was the password itself.
        let legacy_hash = pwhash::unix_crypt::hash("hunter2").unwrap();
        assert!(verify_password("hunter2", &legacy_hash).unwrap());
        assert!(!verify_password("wrong", &legacy_hash).unwrap());
        assert!(needs_rehash(&legacy_hash));
    }

    #[tokio::test]
    async fn token_issue_and_consume_round_trip() {
        let store = LoginTokenStore::new();
        let cookie = store.issue("alice", Duration::from_secs(60)).await;
        assert!(cookie.starts_with("alice%20"));
        let username = store.consume(&cookie).await;
        assert_eq!(username.as_deref(), Some("alice"));
        // tokens are single-use: consuming twice fails the second time
        assert!(store.consume(&cookie).await.is_none());
    }

    #[tokio::test]
    async fn expired_tokens_are_purged() {
        let store = LoginTokenStore::new();
        let cookie = store.issue("bob", Duration::from_millis(0)).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        store.purge_expired().await;
        assert!(store.consume(&cookie).await.is_none());
    }
}

