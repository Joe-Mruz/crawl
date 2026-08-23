//! Typed server → client WebSocket messages. See `../PROTOCOL.md` §3 for
//! the authoritative catalog (transcribed from every `send_message`/
//! `queue_message` call site in `ws_handler.py`/`process_handler.py`/
//! `status.py`).
//!
//! Every variant serializes to `{"msg": "<snake_case name>", ...fields}`,
//! matching Python's `data["msg"] = msg; json_encode(data)`. Fields that
//! Python only includes when explicitly passed use `Option` +
//! `skip_serializing_if` so the emitted JSON keys match exactly (Python
//! never sends a field with an explicit `null` for these).

use serde::{Deserialize, Serialize};

fn is_none<T>(opt: &Option<T>) -> bool {
    opt.is_none()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ServerMessage {
    Ping,
    Close {
        reason: String,
    },
    LoginSuccess {
        username: String,
        admin: bool,
    },
    LoginFail {
        #[serde(skip_serializing_if = "is_none")]
        reason: Option<String>,
    },
    LoginCookie {
        cookie: String,
        expires: i64,
    },
    SetAccountHold,
    Logout {
        reason: String,
    },
    RegisterFail {
        reason: String,
    },
    LoginRequired {
        game: String,
    },
    LobbyClear,
    LobbyEntry {
        #[serde(flatten)]
        entry: LobbyEntry,
    },
    LobbyComplete,
    LobbyRemove {
        id: u64,
        #[serde(skip_serializing_if = "is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        dump: Option<String>,
    },
    Html {
        id: String,
        content: String,
    },
    SetGameLinks {
        content: String,
    },
    GameStarted,
    GameEnded {
        #[serde(skip_serializing_if = "is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        dump: Option<String>,
    },
    GameClient {
        version: String,
        content: String,
    },
    Dump {
        url: String,
    },
    GoLobby,
    GoAdmin,
    WatchingStarted {
        username: String,
    },
    UpdateSpectators {
        count: u32,
        names: String,
    },
    Chat {
        content: String,
        #[serde(skip_serializing_if = "is_none")]
        meta: Option<bool>,
    },
    ServerAnnouncement {
        text: String,
    },
    StaleProcesses {
        timeout: u64,
        game: String,
    },
    #[serde(rename = "force_terminate?")]
    ForceTerminateQuery,
    HideDialog,
    RcfileContents {
        game_id: String,
        contents: String,
    },
    AdminLog {
        text: String,
    },
    AdminPwResetDone {
        #[serde(skip_serializing_if = "is_none")]
        email_body: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        username: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        email: Option<String>,
        #[serde(skip_serializing_if = "is_none")]
        error: Option<String>,
    },
    AuthError {
        reason: String,
    },
    StartChangeEmail {
        #[serde(skip_serializing_if = "is_none")]
        email: Option<String>,
    },
    ChangeEmailDone {
        #[serde(skip_serializing_if = "is_none")]
        email: Option<String>,
    },
    ChangeEmailFail {
        reason: String,
    },
    StartChangePassword,
    ChangePasswordDone,
    ChangePasswordFail {
        reason: String,
    },
    ForgotPasswordDone,
    ForgotPasswordFail {
        reason: String,
    },
    ResetPasswordFail {
        reason: String,
    },
    ReloadUrl,
    ToggleChat,
    SuperHideChat,
}

/// Fields of a lobby entry, matching `CrawlProcessHandlerBase.lobby_entry()`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LobbyEntry {
    pub id: u64,
    pub username: String,
    pub spectator_count: u32,
    pub idle_time: u64,
    pub game_id: String,
    #[serde(skip_serializing_if = "is_none")]
    pub xl: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub char: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub place: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub turn: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub dur: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub god: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "is_none")]
    pub milestone: Option<String>,
}

impl ServerMessage {
    /// Serialize to the exact single-object JSON text Python would produce
    /// for this message (before batching).
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_serializes_to_bare_msg_object() {
        assert_eq!(ServerMessage::Ping.to_json().unwrap(), r#"{"msg":"ping"}"#);
    }

    #[test]
    fn login_fail_omits_reason_when_none() {
        let msg = ServerMessage::LoginFail { reason: None };
        assert_eq!(msg.to_json().unwrap(), r#"{"msg":"login_fail"}"#);
    }

    #[test]
    fn login_fail_includes_reason_when_some() {
        let msg = ServerMessage::LoginFail {
            reason: Some("bad password".to_string()),
        };
        assert_eq!(
            msg.to_json().unwrap(),
            r#"{"msg":"login_fail","reason":"bad password"}"#
        );
    }

    #[test]
    fn force_terminate_query_uses_literal_msg_name() {
        assert_eq!(
            ServerMessage::ForceTerminateQuery.to_json().unwrap(),
            r#"{"msg":"force_terminate?"}"#
        );
    }

    #[test]
    fn login_required_serializes_game_field() {
        let msg = ServerMessage::LoginRequired { game: "Play Trunk".to_string() };
        assert_eq!(msg.to_json().unwrap(), r#"{"msg":"login_required","game":"Play Trunk"}"#);
    }

    #[test]
    fn lobby_entry_flattens_and_skips_absent_fields() {
        let msg = ServerMessage::LobbyEntry {
            entry: LobbyEntry {
                id: 1,
                username: "alice".to_string(),
                spectator_count: 2,
                idle_time: 0,
                game_id: "dcss-web-trunk".to_string(),
                ..Default::default()
            },
        };
        let json = msg.to_json().unwrap();
        assert_eq!(
            json,
            r#"{"msg":"lobby_entry","id":1,"username":"alice","spectator_count":2,"idle_time":0,"game_id":"dcss-web-trunk"}"#
        );
    }
}
