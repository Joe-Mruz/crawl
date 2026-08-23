//! Typed client → server WebSocket messages. See `../PROTOCOL.md` §2 for
//! the authoritative message catalog (transcribed from
//! `ws_handler.CrawlWebSocket.message_handlers`).

use serde::{Deserialize, Serialize};

/// A message the webserver itself understands and handles (login, lobby
/// navigation, chat, account management, admin actions).
///
/// Deliberately modeled as a `serde(tag = "msg")` enum instead of a
/// `HashMap<String, Value>`: every field the Python implementation reads
/// off `**kwargs` for these message types is named here, so a missing or
/// mistyped field is caught at parse time rather than silently defaulting
/// to `None`/empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum KnownClientMessage {
    Login {
        username: String,
        password: String,
    },
    TokenLogin {
        cookie: String,
    },
    SetLoginCookie,
    ForgetLoginCookie {
        cookie: String,
    },
    Play {
        game_id: String,
    },
    Pong,
    Watch {
        username: String,
    },
    ChatMsg {
        text: String,
    },
    Register {
        username: String,
        password: String,
        email: String,
    },
    StartChangeEmail,
    ChangeEmail {
        email: String,
    },
    StartChangePassword,
    ChangePassword {
        cur_password: String,
        new_password: String,
    },
    ForgotPassword {
        email: String,
    },
    ResetPassword {
        token: String,
        password: String,
    },
    GoLobby,
    GoAdmin,
    GetRc {
        game_id: String,
    },
    SetRc {
        game_id: String,
        contents: String,
    },
    AdminAnnounce {
        text: String,
    },
    AdminPwReset {
        username: String,
    },
    AdminPwResetClear {
        username: String,
    },
}

/// A fully decoded incoming client frame: either one of the webserver's own
/// message types, or an opaque message meant for the attached game process
/// (`key`, `input`, `ui_state_sync`, `menu_action`, etc. — these are owned
/// by the DCSS client/engine, not the webserver, and are forwarded
/// byte-for-byte; see `PROTOCOL.md` §2 and §4).
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Known(KnownClientMessage),
    PassThrough { msg: String, raw: String },
}

/// Errors returned by [`ClientMessage::parse`].
#[derive(Debug, thiserror::Error)]
pub enum ClientMessageParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("message object missing a `msg` field")]
    MissingMsgField,
}

impl ClientMessage {
    /// Parse one raw WebSocket text frame. Frames that name a known
    /// `msg` but fail to deserialize into it (missing/mistyped fields) are
    /// treated as a protocol error, matching the fact that these message
    /// types are exhaustively specified. Frames naming an unrecognized
    /// `msg` are passed through unmodified.
    pub fn parse(frame: &str) -> Result<Self, ClientMessageParseError> {
        let value: serde_json::Value = serde_json::from_str(frame)?;
        let msg_name = value
            .get("msg")
            .and_then(|v| v.as_str())
            .ok_or(ClientMessageParseError::MissingMsgField)?
            .to_string();

        if is_known_message_name(&msg_name) {
            let known: KnownClientMessage = serde_json::from_value(value)?;
            Ok(ClientMessage::Known(known))
        } else {
            Ok(ClientMessage::PassThrough {
                msg: msg_name,
                raw: frame.to_string(),
            })
        }
    }
}

fn is_known_message_name(name: &str) -> bool {
    matches!(
        name,
        "login"
            | "token_login"
            | "set_login_cookie"
            | "forget_login_cookie"
            | "play"
            | "pong"
            | "watch"
            | "chat_msg"
            | "register"
            | "start_change_email"
            | "change_email"
            | "start_change_password"
            | "change_password"
            | "forgot_password"
            | "reset_password"
            | "go_lobby"
            | "go_admin"
            | "get_rc"
            | "set_rc"
            | "admin_announce"
            | "admin_pw_reset"
            | "admin_pw_reset_clear"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_login() {
        let frame = r#"{"msg":"login","username":"alice","password":"hunter2"}"#;
        let parsed = ClientMessage::parse(frame).unwrap();
        assert_eq!(
            parsed,
            ClientMessage::Known(KnownClientMessage::Login {
                username: "alice".to_string(),
                password: "hunter2".to_string(),
            })
        );
    }

    #[test]
    fn parses_play() {
        let frame = r#"{"msg":"play","game_id":"dcss-web-trunk"}"#;
        assert_eq!(
            ClientMessage::parse(frame).unwrap(),
            ClientMessage::Known(KnownClientMessage::Play {
                game_id: "dcss-web-trunk".to_string()
            })
        );
    }

    #[test]
    fn unknown_message_is_passed_through_verbatim() {
        let frame = r#"{"msg":"key","keycode":13}"#;
        let parsed = ClientMessage::parse(frame).unwrap();
        assert_eq!(
            parsed,
            ClientMessage::PassThrough {
                msg: "key".to_string(),
                raw: frame.to_string(),
            }
        );
    }

    #[test]
    fn missing_msg_field_is_an_error() {
        let err = ClientMessage::parse(r#"{"foo":"bar"}"#).unwrap_err();
        assert!(matches!(err, ClientMessageParseError::MissingMsgField));
    }

    #[test]
    fn unit_variants_round_trip() {
        for frame in ["pong", "go_lobby", "go_admin", "set_login_cookie"] {
            let json = format!(r#"{{"msg":"{frame}"}}"#);
            assert!(ClientMessage::parse(&json).is_ok());
        }
    }
}
