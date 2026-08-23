# WebTiles Protocol Reference

Concrete message catalog backing `ARCHITECTURE.md`. All messages are JSON
unless noted. Field names/casing are preserved exactly as the Python
implementation and JS client use them — this is a compatibility document,
not a proposal for a cleaner protocol.

## 1. Transport envelope

### Browser ⇄ server (`/socket` WebSocket)
- **Client → server**: one JSON object per frame: `{"msg": "<name>", ...}`.
- **Server → client**: batched. Every flush sends exactly one frame shaped
  `{"msgs": [ <json-object-as-string>, <json-object-as-string>, ... ]}`
  — note the inner elements are pre-serialized JSON text spliced together,
  not a JSON array of objects re-encoded; the Rust codec must produce
  byte-identical output (see `protocol/codec.rs`).
  - If compression is enabled (default; disabled by WS subprotocol
    `no-compression`, by legacy draft-76 clients, or by a `deflate-frame`
    WS extension already being active): the batch JSON is UTF-8 encoded,
    then raw-deflated (zlib `deflate`, `wbits = -15`, i.e. **no** zlib
    header/trailer), flushed with `Z_SYNC_FLUSH`, and the trailing 4 bytes
    (`00 00 FF FF`) are stripped — sent as a **binary** frame. This matches
    the `permessage-deflate`-style trick but is hand-rolled, not the real
    WS extension.
  - If compression is disabled: sent as a **text** frame, raw UTF-8 JSON.
  - One legacy quirk: the connection-limit-reached rejection goes through
    the exact same `append_message`/batch path as every other message, but
    the literal text queued is `connection_closed('<message>');` - a raw
    JS statement, not a JSON-encoded string. It is spliced into the
    `{"msgs":[...]}` array unquoted/unescaped, same as any other queued
    fragment, producing a technically-non-JSON batch body that the legacy
    JS client evals as a special case. Preserved byte-for-byte since old
    clients depend on this exact shape; not treated as a bug to fix.
- Idle liveness: server-initiated `{"msg":"ping"}` unbatched (always
  flushed immediately); client must reply `{"msg":"pong"}` within
  `connection_timeout` seconds (default 600) or the socket is closed.

### Server ⇄ DCSS process (Unix `SOCK_DGRAM`)
- Each logical message is one `\n`-terminated JSON document per datagram
  (buffered/reassembled only if a send was fragmented across multiple
  datagrams — rare).
- A message is intercepted by the webserver (not forwarded to the browser)
  iff its serialized text begins with `*`; the `*` is stripped before
  parsing the remainder as JSON. All other messages are forwarded verbatim.

## 2. Client → server messages (`ws_handler.CrawlWebSocket.message_handlers`)

| `msg` | Fields | Semantics |
|---|---|---|
| `login` | `username`, `password` | Password login. |
| `token_login` | `cookie` | Cookie/token login (`"<username>%20<token>"`). |
| `set_login_cookie` | — | Ask server to mint+return a login token for the current session (`login_cookie` reply). |
| `forget_login_cookie` | `cookie` | Invalidate a token (logout on other tabs). |
| `play` | `game_id` | Start/join the given configured game. |
| `pong` | — | Liveness reply. |
| `watch` | `username` | Spectate a running game by player name. |
| `chat_msg` | `text` | Chat line, or `/`-prefixed chat command. |
| `register` | `username`, `password`, `email` | Create account. |
| `start_change_email` | — | Request current email (`start_change_email` reply). |
| `change_email` | `email` | Apply email change. |
| `start_change_password` | — | Ack, prompts UI to show the form. |
| `change_password` | `cur_password`, `new_password` | Apply password change. |
| `forgot_password` | `email` | Trigger reset-token email. |
| `reset_password` | `token`, `password` | Consume reset token. |
| `go_lobby` | — | Leave current game/spectate, return to lobby. |
| `go_admin` | — | Same as `go_lobby` plus a `go_admin` reply (open admin panel). |
| `get_rc` | `game_id` | Fetch rc file contents for editing. |
| `set_rc` | `game_id`, `contents` | Save rc file contents. |
| `admin_announce` | `text` | *(admin)* Server-wide announcement. |
| `admin_pw_reset` | `username` | *(admin)* Force a password-reset token. |
| `admin_pw_reset_clear` | `username` | *(admin)* Clear a reset token. |

Any other `msg` value: if a game process is attached, forwarded verbatim as
raw input to the process socket (this is how in-game commands like `key`,
`input`, `ui_state_sync`, `menu_action`, etc. — defined by the DCSS client
JS, not the webserver — actually reach the game); otherwise logged as an
unrecognized message (except `key`/`ui_state_sync`, which are silently
ignored pre-game-start/post-game-end as a known benign race).

## 3. Server → client messages (non-exhaustive but covers every call site in
`ws_handler.py`/`process_handler.py`/`status.py`)

| `msg` | Fields | When |
|---|---|---|
| `ping` | — | Liveness probe. |
| `close` | `reason` (HTML) | Server is force-closing this socket (shutdown, fatal init error). |
| `login_success` | `username`, `admin` | Successful login. |
| `login_fail` | `reason?` | Failed login. |
| `login_cookie` | `cookie`, `expires` | Response to `set_login_cookie`. |
| `set_account_hold` | — | Tells the client this account is hold-restricted. |
| `logout` | `reason` | Forced logout (ban detected on an existing session). |
| `register_fail` | `reason` | Registration failed. |
| `lobby_clear` / `lobby_entry` / `lobby_complete` / `lobby_remove` | see `CrawlProcessHandlerBase.lobby_entry()` | Lobby game list sync. `lobby_entry` fields: `id`, `username`, `spectator_count`, `idle_time`, `game_id`, plus any of `xl`,`char`,`place`,`turn`,`dur`,`god`,`title` present in `where`, plus `milestone` if any. `lobby_remove` fields: `id`,`reason`,`message`,`dump`. |
| `html` | `id`, `content` | Injects rendered server-side HTML fragment (banner, etc.) by DOM id. |
| `set_game_links` | `content` (HTML) | Per-user "play" buttons/save-slot state. |
| `game_started` | — | Local game process has been created for this socket. |
| `game_ended` | `reason`, `message`, `dump` | Game process exited. |
| `game_client` | `version`, `content` (HTML) | Injects the versioned game UI shell. |
| `go_lobby` | — | Client should navigate to lobby view. |
| `go_admin` | — | Client should open admin panel. |
| `watching_started` | `username` | Spectate began. |
| `update_spectators` | `count`, `names` | Spectator/player list for the sidebar. |
| `chat` | `content` (HTML), `meta?` | Chat line or system notification. |
| `server_announcement` | `text` | Broadcast announcement (chat view). |
| `stale_processes` | `timeout`, `game` | A previous crash-lock is being purged; countdown shown. |
| `force_terminate?` | — | Ask the client whether to force-kill a stuck stale process. |
| `hide_dialog` | — | Dismiss the stale-process dialog. |
| `rcfile_contents` | `game_id`, `contents` | Response to `get_rc`. |
| `admin_log` | `text` | Admin-only console line (stats, announcement ack). |
| `admin_pw_reset_done` | `email_body?`, `username?`, `email?`, `error?` | Admin reset-token result. |
| `auth_error` | `reason` | Generic auth-gate failure shown above the login box. |
| `start_change_email` | `email` | Current email, pre-filling the form. |
| `change_email_done` | `email` | Email change applied. |
| `change_email_fail` | `reason` | Email change rejected. |
| `start_change_password` | — | Ack for the password-change form. |
| `change_password_done` | — | Password changed. |
| `change_password_fail` | `reason` | Password change rejected. |
| `forgot_password_done` | — | Reset email queued (or silently no-op if unregistered — doesn't leak existence). |
| `forgot_password_fail` | `reason` | Reset request rejected (e.g. feature disabled). |
| `reset_password_fail` | `reason` | Reset-token consumption failed. |
| `reload_url` | — | Full page reload (post reset-password success). |
| `spectator_joined` | — | Sent **on the game-process socket**, not to the browser: tells DCSS a new watcher exists (server-side control message, one of the `*`-prefixed-on-the-way-out family, though this one is sent *unprefixed* by the webserver to the process — asymmetric with the process→server `*` convention). |
| `options` | `watcher: true`, `options` (raw JSON blob from `-print-webtiles-options`) | Per-spectator JSON option sync, generated via a non-blocking subprocess call, not the running game. |
| `note` | `content` | Chat line forwarded into the DCSS process's own message/log window. |
| `toggle_chat` / `super_hide_chat` | — | Chat visibility toggles. |
| `admin_announce` (as a server_announcement in the process) | — | see `handle_announcement`. |

## 4. Crawl-process control messages (over the Unix datagram socket,
`*`-prefixed on the way in from DCSS to the webserver)

| `msg` (after stripping leading `*`) | Fields | Handling |
|---|---|---|
| `client_path` | `path`, `version?` | First-time bootstrap: which on-disk client asset directory + version hash to serve/register with `GameDataHandler`; triggers rendering and sending `game_client` to every current watcher. |
| `flush_messages` | — | Switch this game's message delivery from "send immediately" to "queue and batch" mode (`queue_messages = true`), and flush anything queued. |
| `dump` | `type` (`"command"` or other), `filename` | Morgue file was written; either send a `dump` URL immediately (`type == "command"`, i.e. player explicitly requested a dump) or remember it as `exit_dump_url` for the eventual `game_ended` message. |
| `exit_reason` | `type`, `message?` | Structured game-over reason/message, used verbatim in the eventual `game_ended`. |
| `milestone` | milestone fields inline | Equivalent to a parsed `.milestone`/`.where` file line, but pushed proactively; once seen, file-based `.where` polling is skipped for this process (`receiving_direct_milestones = true`). |

Sent from webserver → process (also over the datagram socket, plain JSON,
no `*` prefix):
- `{"msg":"attach","primary":<bool>}` — handshake, first message only.
- Raw forwarded client messages (`input`, `key`, etc. — passed through
  as-is after JSON-decoding only far enough to special-case `input` and
  `force_terminate`/`stop_stale_process_purge`, see below).
- `{"msg":"input","data":[<charcodes>...],"text":"<string>"}` — keystroke
  input; the two representations (`data` char codes, `text` string) are
  concatenated by the webserver into one `data` string before forwarding
  raw bytes — **this concatenation happens server-side**, i.e. the *outgoing*
  message to DCSS is not the same shape as what the browser sent (browser
  sends `data`/`text` split; process receives them joined). Actually: on
  reread, `handle_input` builds a *local* `data` string from `obj["data"]`
  + `obj["text"]` for its own bookkeeping/activity-timestamp purposes, but
  what's forwarded to the crawl process socket is `self.conn.send_message(utf8(msg))`,
  i.e. the **original untouched message text** from the browser. Only two
  `msg` values are intercepted and *not* forwarded: `force_terminate`
  (webserver-only) and `stop_stale_process_purge` (webserver-only). All
  other messages, including `input`, are forwarded byte-for-byte.
- `{"msg":"note","content":"<user>: <chat text>"}` — chat line replayed into
  the game log.
- `{"msg":"server_announcement","content":"<text>"}`.
- `spectator_joined` (bare, unprefixed) on new watcher attach.

## 5. Process argv (`CrawlProcessHandlerBase._base_call`,
`CrawlProcessHandler._start_process`)

```
$crawl_binary
[...$pre_options]
-name <username>
-rc <rcfile_path>/<username>.rc
-macro <macro_path>/<username>.macro
-morgue <morgue_path>
[-no-player-bones]                  # iff account_restricted()
[...$options]                       # game-config "options" list, templated
[-dir <dir_path>]                   # iff dir_path configured
-webtiles-socket <socket_path>/<username>:<timestamp>.sock
-await-connection
```
`timestamp` format: `%Y-%m-%d.%H:%M:%S` (also the `.ttyrec` file's base
name). `DGLLessCrawlProcessHandler` (non-dgl "local" mode) instead uses a
bare `["./crawl"]` argv with fixed relative paths.

## 6. Lock file format (`inprogress_path/<username>:<timestamp>.ttyrec`-
adjacent lock, written by `gen_inprogress_lock`)

Plain text, `flock`'d exclusively, 3 lines:
```
<pid>
<terminal lines>
<terminal columns>
```

## 7. Config-driven templating placeholders

Applied to most `GameConfig` string/list-of-string fields (not
`socket_path`) via `dgl_format_str`:
- `%n` → username (only if a username is available in context).
- `%v` → `version` config value, verbatim.
- `%V` → `version` capitalized.
- `%r` → `version` with a leading `"0."` stripped (else unchanged).

## 8. HTTP JSON shapes

- `GET /status/version/` →
  `{"webtiles": "<version>", "tornado": "<ver>", "python": "<ver>", "python_supported": <bool>}`.
  Rust: `tornado`/`python` fields become `axum`/`rustc` (documented as an
  intentional, client-invisible informational difference — no client is
  known to branch on these two specific fields; see `MIGRATION.md`).
- `GET /status/lobby/` → JSON array of
  `{"name","game_id","idle_time","viewers","watch_url"?,"v","vlong","tiles","race","cls","char","xl","title","place","god","turn","dur","milestone"}`
  (missing `where` keys default to `""`, preserved exactly).
