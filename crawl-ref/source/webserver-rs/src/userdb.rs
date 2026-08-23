//! User/account database, matching `userdb.py`'s SQLite schema exactly so
//! an existing `passwd.db3`/`user_settings.db3` can be reused as-is.
//!
//! Blocking `rusqlite` calls are wrapped in `spawn_blocking` at the async
//! API boundary; the connection itself is guarded by a `std::sync::Mutex`
//! (SQLite connections are not `Sync`), mirroring the single persistent
//! connection Python keeps open for the process lifetime.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Result, WebtilesError};

// Bit flags, matching dgamelaunch.h / userdb.py exactly (values are part of
// the on-disk format shared with existing dgamelaunch-config deployments;
// do not renumber).
pub const DGLACCT_ADMIN: i64 = 1;
pub const DGLACCT_LOGIN_LOCK: i64 = 2;
pub const DGLACCT_PASSWD_LOCK: i64 = 4;
pub const DGLACCT_EMAIL_LOCK: i64 = 8;
pub const DGLACCT_ACCOUNT_HOLD: i64 = 16;
pub const DGLACCT_WIZARD: i64 = 32;
pub const DGLACCT_BOT: i64 = 64;

pub fn is_admin(flags: i64) -> bool {
    flags & DGLACCT_ADMIN != 0
}

pub fn is_account_hold(flags: i64) -> bool {
    flags & DGLACCT_ACCOUNT_HOLD != 0
}

/// `dgl_is_banned`: banned unless the ban is actually just a (weaker)
/// account hold.
pub fn is_banned(flags: i64) -> bool {
    (flags & DGLACCT_LOGIN_LOCK != 0) && !is_account_hold(flags)
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInfo {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub flags: i64,
}

/// Handle to the two SQLite databases the webserver uses: the account/
/// password database, and the (separate, less sensitive) per-user settings
/// database (blocklists).
pub struct UserDb {
    users: Arc<Mutex<Connection>>,
    settings: Arc<Mutex<Connection>>,
}

impl UserDb {
    /// Open (creating + migrating schema if needed) both databases, exactly
    /// matching `userdb.ensure_user_db_exists`/`ensure_settings_db_exists`.
    pub fn open(password_db: impl AsRef<Path>, settings_db: impl AsRef<Path>) -> Result<Self> {
        let users = Connection::open(password_db)?;
        ensure_user_schema(&users)?;
        let settings = Connection::open(settings_db)?;
        ensure_settings_schema(&settings)?;
        Ok(Self {
            users: Arc::new(Mutex::new(users)),
            settings: Arc::new(Mutex::new(settings)),
        })
    }

    /// In-memory instance for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let users = Connection::open_in_memory()?;
        ensure_user_schema(&users)?;
        let settings = Connection::open_in_memory()?;
        ensure_settings_schema(&settings)?;
        Ok(Self {
            users: Arc::new(Mutex::new(users)),
            settings: Arc::new(Mutex::new(settings)),
        })
    }

    pub async fn get_user_info(&self, username: &str) -> Result<Option<UserInfo>> {
        let conn = self.users.clone();
        let username = username.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("user db lock poisoned");
            conn.query_row(
                "SELECT id, username, email, flags FROM dglusers WHERE username = ?1 COLLATE NOCASE",
                params![username],
                |row| {
                    Ok(UserInfo {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        email: row.get(2)?,
                        flags: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))?
    }

    /// Matches `userdb.user_passwd_match`: returns `(success, canonical_username, fail_reason)`.
    pub async fn check_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(bool, Option<String>, Option<String>)> {
        let conn = self.users.clone();
        let username_owned = username.to_string();
        let row = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("user db lock poisoned");
            conn.query_row(
                "SELECT username, password, flags FROM dglusers WHERE username = ?1 COLLATE NOCASE",
                params![username_owned],
                |row| {
                    let username: String = row.get(0)?;
                    let password: String = row.get(1)?;
                    let flags: i64 = row.get(2)?;
                    Ok((username, password, flags))
                },
            )
            .optional()
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))??;

        let Some((real_username, stored_hash, flags)) = row else {
            return Ok((false, None, None));
        };
        if is_banned(flags) {
            return Ok((false, Some(real_username), Some("Account is disabled.".to_string())));
        }
        let ok = crate::auth::verify_password(password, &stored_hash)?;
        if ok {
            if crate::auth::needs_rehash(&stored_hash) {
                // transparent migration off legacy crypt(3)-style hashes
                // (inherited from a migrated Python passwd.db3) onto Argon2.
                if let Ok(new_hash) = crate::auth::hash_password(password) {
                    let _ = self.update_password_hash(&real_username, &new_hash).await;
                }
            }
            Ok((true, Some(real_username), None))
        } else {
            Ok((false, None, None))
        }
    }

    async fn update_password_hash(&self, username: &str, hash: &str) -> Result<()> {
        let conn = self.users.clone();
        let username = username.to_string();
        let hash = hash.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("user db lock poisoned");
            conn.execute(
                "UPDATE dglusers SET password = ?1 WHERE username = ?2 COLLATE NOCASE",
                params![hash, username],
            )
            .map(|_| ())
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))?
    }

    /// Matches `userdb.register_user`. Returns `Err` (as an error message,
    /// like Python) on validation failure, `Ok(())` on success.
    pub async fn register_user(
        &self,
        username: &str,
        password: &str,
        email: Option<&str>,
    ) -> Result<std::result::Result<(), String>> {
        if password.is_empty() {
            return Ok(Err("The password can't be empty!".to_string()));
        }
        if self.get_user_info(username).await?.is_some() {
            return Ok(Err("User already exists!".to_string()));
        }
        let hash = crate::auth::hash_password(password)?;
        let conn = self.users.clone();
        let username = username.to_string();
        let email = email.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("user db lock poisoned");
            conn.execute(
                "INSERT INTO dglusers (username, email, password, flags, env) VALUES (?1, ?2, ?3, 0, '')",
                params![username, email, hash],
            )
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))??;
        Ok(Ok(()))
    }

    pub async fn set_flags(&self, username: &str, flags: i64, mask: i64) -> Result<()> {
        let Some(info) = self.get_user_info(username).await? else {
            return Err(WebtilesError::Auth("Invalid username!".to_string()));
        };
        let new_flags = (info.flags & !mask) | (flags & mask);
        let conn = self.users.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("user db lock poisoned");
            conn.execute(
                "UPDATE dglusers SET flags = ?1 WHERE id = ?2",
                params![new_flags, info.id],
            )
            .map(|_| ())
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))?
    }

    pub async fn get_blocklist(&self, username: &str) -> Result<Vec<String>> {
        let conn = self.settings.clone();
        let username = username.to_string();
        let raw: Option<String> = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("settings db lock poisoned");
            conn.query_row(
                "SELECT mutelist FROM mutesettings WHERE username = ?1 COLLATE NOCASE",
                params![username],
                |row| row.get(0),
            )
            .optional()
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))??;

        Ok(raw
            .unwrap_or_default()
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    pub async fn set_blocklist(&self, username: &str, blocklist: &[String]) -> Result<()> {
        let conn = self.settings.clone();
        let username = username.to_string();
        let joined = blocklist.join(" ");
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("settings db lock poisoned");
            conn.execute(
                "INSERT OR REPLACE INTO mutesettings (username, mutelist) VALUES (?1, ?2)",
                params![username, joined],
            )
            .map(|_| ())
            .map_err(WebtilesError::from)
        })
        .await
        .map_err(|e| WebtilesError::Internal(e.to_string()))?
    }
}

fn ensure_user_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dglusers (
            id INTEGER PRIMARY KEY,
            username TEXT,
            email TEXT,
            env TEXT,
            password TEXT,
            flags INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS index_username ON dglusers (username COLLATE NOCASE);
        CREATE TABLE IF NOT EXISTS recovery_tokens (
            token TEXT PRIMARY KEY,
            token_time TEXT,
            user_id INTEGER NOT NULL,
            FOREIGN KEY(user_id) REFERENCES dglusers(id)
        );",
    )?;
    Ok(())
}

fn ensure_settings_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mutesettings (
            username TEXT PRIMARY KEY NOT NULL UNIQUE,
            mutelist TEXT DEFAULT ''
        );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_check_password_round_trip() {
        let db = UserDb::open_in_memory().unwrap();
        let result = db
            .register_user("Alice", "hunter2", Some("alice@example.com"))
            .await
            .unwrap();
        assert!(result.is_ok());

        let (ok, username, reason) = db.check_password("alice", "hunter2").await.unwrap();
        assert!(ok);
        assert_eq!(username.as_deref(), Some("Alice")); // canonicalized casing
        assert!(reason.is_none());

        let (ok, _, _) = db.check_password("alice", "wrong").await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn legacy_hash_is_transparently_upgraded_to_argon2_on_login() {
        let db = UserDb::open_in_memory().unwrap();
        db.register_user("legacy", "hunter2", None).await.unwrap().unwrap();
        // simulate a row migrated from the Python server's passwd.db3
        let legacy_hash = pwhash::sha512_crypt::hash("hunter2").unwrap();
        db.update_password_hash("legacy", &legacy_hash).await.unwrap();

        let (ok, _, _) = db.check_password("legacy", "hunter2").await.unwrap();
        assert!(ok);

        // the login above should have rehashed it to argon2 in place
        let conn = db.users.clone();
        let stored: String = tokio::task::spawn_blocking(move || {
            conn.lock()
                .unwrap()
                .query_row(
                    "SELECT password FROM dglusers WHERE username = 'legacy'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        })
        .await
        .unwrap();
        assert!(stored.starts_with("$argon2id$"));
    }

    #[tokio::test]
    async fn duplicate_registration_is_rejected() {
        let db = UserDb::open_in_memory().unwrap();
        db.register_user("bob", "pw", None).await.unwrap().unwrap();
        let second = db.register_user("bob", "pw2", None).await.unwrap();
        assert_eq!(second, Err("User already exists!".to_string()));
    }

    #[tokio::test]
    async fn banned_account_always_fails_login() {
        let db = UserDb::open_in_memory().unwrap();
        db.register_user("evil", "pw", None).await.unwrap().unwrap();
        db.set_flags("evil", DGLACCT_LOGIN_LOCK, DGLACCT_LOGIN_LOCK)
            .await
            .unwrap();
        let (ok, _, reason) = db.check_password("evil", "pw").await.unwrap();
        assert!(!ok);
        assert_eq!(reason.as_deref(), Some("Account is disabled."));
    }

    #[tokio::test]
    async fn blocklist_round_trips() {
        let db = UserDb::open_in_memory().unwrap();
        db.set_blocklist("carol", &["dave".to_string(), "[anon]".to_string()])
            .await
            .unwrap();
        let list = db.get_blocklist("carol").await.unwrap();
        assert_eq!(list, vec!["dave".to_string(), "[anon]".to_string()]);
    }

    #[test]
    fn flag_helpers_match_python_semantics() {
        assert!(is_admin(DGLACCT_ADMIN));
        assert!(!is_admin(0));
        assert!(is_banned(DGLACCT_LOGIN_LOCK));
        // a hold sets LOGIN_LOCK too, but is_banned must return false for it
        assert!(!is_banned(DGLACCT_LOGIN_LOCK | DGLACCT_ACCOUNT_HOLD));
        assert!(is_account_hold(DGLACCT_ACCOUNT_HOLD));
    }
}
