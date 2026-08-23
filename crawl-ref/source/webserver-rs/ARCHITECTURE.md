# WebTiles Rust Rewrite — Architecture Reference

This document maps the existing Python WebTiles server (`crawl-ref/source/webserver/`)
to the new Rust implementation (`crawl-ref/source/webserver-rs/`). It is the
authoritative "what does the Python code actually do" reference used while
porting; when in doubt, this document (and the Python source it cites) wins
over assumptions.

The Python implementation is **not removed**. It continues to live at
`crawl-ref/source/webserver/` and remains deployable. See `MIGRATION.md` for
the retirement plan.

## 1. Python entry points

| File | Role |
|---|---|
| `webserver/server.py` | Thin bootstrap: loads `config.py` as the config module, sets `server_path`, calls `webtiles.server.run()`. |
| `webserver/webtiles/server.py` | Real entry point. Argument parsing, daemonization, pidfile, signal handling, binds HTTP(S) sockets, builds the Tornado `Application`, drives the asyncio loop. Also hosts the `wtutil.py`-style admin CLI (`password`/`ban`/`flag` subcommands via `run_util()`). |
| `webserver/config.py` | The *default* config module (tracked in git). Server operators override via `config.yml` (YAML merge) and/or `games.d/*.yml`. |
| `webserver/wtutil.py` | CLI wrapper invoking `webtiles.server.run_util()`. |

Rust equivalent: `webtiles-rs` binary crate with `main.rs` doing argument
parsing (`clap`) + config load + server bootstrap. The admin CLI subcommands
are a separate concern from the running server and are ported last (they
operate directly on the user/settings databases).

## 2. HTTP endpoints (`webtiles/server.py: bind_server`)

| Method | Path | Handler | Behavior |
|---|---|---|---|
| GET | `/` | `MainHandler` | Renders `templates/client.html` with `socket_server` (ws/wss URL), `game_version`, `config`, and password-reset-token state (`?ResetToken=`). No auth required — the lobby itself authenticates over the websocket. |
| GET/WS | `/socket` | `ws_handler.CrawlWebSocket` | The single WebSocket endpoint. All game/lobby/chat/auth interaction happens here. |
| GET | `/gamedata/<version-hex>/<path>` | `game_data_handler.GameDataHandler` | Static file serving for game client assets (tiles, JS, sound), namespaced by a per-binary content hash (`version`) so multiple crawl versions/binaries can serve different asset sets concurrently. 404 if `version` is unknown. Honors `game_data_no_cache` config (disables caching headers). |
| GET | `/status/lobby/` | `status.LobbyHandler` | JSON array of currently running, publicly-visible games (used by external dashboards). No auth. |
| GET | `/status/version/` | `status.VersionHandler` | JSON `{webtiles, tornado, python, python_supported}` version info. |
| GET | `/` (redirect app) | `HTTPSRedirectHandler` | Only mounted when `bind_nonsecure == "redirect"`; 301-redirects every path to the HTTPS host/port. |

Static files under `static_path` (config) are also served directly by
Tornado's built-in `StaticFileHandler` via the `static_path` application
setting — *not* listed above as an explicit route, but Tornado auto-adds
`/static/(.*)`. `no_cache` config swaps in `NoCacheHandler` (adds
`Cache-Control: no-cache, no-store, must-revalidate`).

Rust equivalent: Axum router in `http/mod.rs`, handlers in `http/handlers.rs`.
`GameDataHandler`'s per-version static roots become a small in-memory
`HashMap<VersionHash, PathBuf>` behind an `RwLock`, served with `tower-http`'s
`ServeDir` per lookup (see `http/game_data.rs`).

## 3. WebSocket lifecycle (`webtiles/ws_handler.py: CrawlWebSocket`)

### Connection open (`open()`)
1. Assign a monotonic connection id.
2. Reject if `max_connections` exceeded, or server is shutting down — send a
   `connection_closed` message as a **raw literal string** (not JSON! —
   `"connection_closed('...');"`, a legacy non-JSON control message) then
   close.
3. If `dgl_mode`:
   - Optionally auto-login (`config.autologin`).
   - Send the lobby (`send_lobby`: `lobby_clear`, `lobby_entry` per game,
     `lobby_complete`, banner HTML, admin-only socket stats).
4. Else (`dgl_mode == False`, single-game/local mode): immediately start a
   game via `DGLLessCrawlProcessHandler` (no login).
5. Compression negotiation: WebSocket subprotocol `no-compression` disables
   per-message raw-deflate; otherwise raw deflate (no zlib header, i.e.
   `-MAX_WBITS`) is used for every outgoing batch, matching the browser's
   `client.js` inflate implementation. This is **not** the standard
   `permessage-deflate` extension — it's an application-level scheme layered
   on top of binary WebSocket frames.

### Message framing (both directions)
- **Client → server**: one JSON object per WebSocket text/binary frame,
  `{"msg": "<type>", ...fields}`. Dispatched via a `message_handlers` name →
  method map (see the full list in `PROTOCOL.md`). Unknown `msg` while a game
  is running is forwarded verbatim to the child process. Unknown `msg` with
  no running game/watch and not `key`/`ui_state_sync` logs a warning.
- **Server → client**: messages are *batched*. `queue_message` appends
  `{"msg":...}` JSON strings to a per-connection list; `send_message` queues
  then immediately flushes. `flush_messages` wraps the queue as
  `{"msgs":[...]}"` and sends it as a single frame, optionally raw-deflate
  compressed (binary frame) or plain UTF-8 (text frame, only when
  compression disabled). This is why the JS client always expects either
  `{"msgs":[...]}` or a legacy raw `connection_closed(...)` string.
- Idle/liveness: server sends `{"msg":"ping"}` on a timer
  (`connection_timeout`, default 600s) and expects a `{"msg":"pong"}` inside
  the same window, else closes. Separately, in-game idle time closes the
  crawl process after `max_idle_time` (default 5h), and a bare lobby
  connection times out after `max_lobby_idle_time` (default 3h, admins
  exempt).

### Core state per socket
`username`, `user_id`, `user_flags` (ban/hold/admin/wizard/bot bits from
`userdb`), `process` (the game the socket is *playing*, if any), `watched_game`
(the game the socket is *spectating*, if any — mutually exclusive with
`process`), `game_id` (selected game config key), `save_info` cache (per
game-config "save slot" status used for lobby menu greying-out).

### Message handlers (authoritative list; ported 1:1 as an enum — see
`PROTOCOL.md` §Client Messages)
`login`, `token_login`, `set_login_cookie`, `forget_login_cookie`, `play`
(→ `start_crawl`), `pong`, `watch`, `chat_msg`, `register`,
`start_change_email`, `change_email`, `start_change_password`,
`change_password`, `forgot_password`, `reset_password`, `go_lobby`,
`go_admin`, `get_rc`, `set_rc`, `admin_announce`, `admin_pw_reset`,
`admin_pw_reset_clear`.

### Game creation / joining / observing / termination
- **Play** (`start_crawl(game_id)`): validates `game_id` against configured
  games, requires login in `dgl_mode`, invalidates the save-slot cache,
  re-checks ban/hold flags, then constructs a `CrawlProcessHandler` (dgl mode)
  or `DGLLessCrawlProcessHandler` (local mode) and calls `.start()`. Adds the
  socket as the process's first "watcher" (players are just a privileged
  watcher: the one whose `watched_game` is not set).
- **Observe** (`watch(username)`): stops the caller's own game if any, looks
  up a running process by (case-insensitive) username in the global
  `processes` registry, enforces the target's block-list and account
  restrictions, then attaches as a watcher. Sends `watching_started`.
- **Terminate**: `go_lobby()` calls `process.stop()` which sends `SIGHUP` to
  the crawl process (cooperative save/quit), arms a `kill_timeout` (default
  10s) that escalates to `SIGABRT` if the process hasn't exited. Process exit
  triggers `_on_crawl_end`/`handle_process_end`, which notifies all watchers
  (`game_ended` with `reason`/`message`/`dump` URL), removes the game from the
  lobby, and returns players to the lobby.
- **Reconnection**: not a first-class "resume my game" feature over the
  websocket — a lost websocket does *not* keep the crawl process alive
  waiting for the same connection. What *does* survive is: (a) login cookies
  (`set_login_cookie`/`token_login`, TTL `login_token_lifetime` days) so a
  browser reload can silently re-auth, and (b) the crawl process itself,
  which keeps running detached and is later discovered on disk via its
  named Unix socket if `watch_socket_dirs` is enabled (`process_handler.py:
  watch_socket_dirs`/`handle_new_socket`), letting a *different* incoming
  connection re-attach as watcher/player. There's also `-await-connection`
  reconnect semantics up to the DCSS binary itself, not the webserver.
- **Server shutdown**: `stop_everything` stops accepting new HTTP/WS
  connections, then calls `ws_handler.shutdown()` (broadcasts a `close`
  message plus a rendered goodbye HTML then closes every open socket **and**
  stops every running crawl process), and polls until `sockets` is empty
  (30s budget) before falling back to cancelling all asyncio tasks.

## 4. DCSS process management

This is the most operationally important, most protocol-sensitive part of
the system, split across `process_handler.py`, `terminal.py`, and
`connection.py`.

### 4.1 Process spawn (`terminal.py: TerminalRecorder`)
- Uses `pty.fork()` (BSD/Linux pseudo-terminal fork, *not* `subprocess`):
  the child gets a controlling TTY, sets `COLUMNS`/`LINES`/`TERM=linux`,
  `chdir`s to `game_cwd` if given, closes all inherited FDs above stderr,
  then `execvpe`s the crawl binary with the assembled argv
  (`CrawlProcessHandlerBase._base_call`, see `PROTOCOL.md` §Process Argv).
  Stderr is redirected through a dedicated pipe (`errpipe_read`) so crash
  diagnostics can be parsed independently of the tty stream.
- The **PTY master fd** (`child_fd`) is registered on the IOLoop for
  readability. Every chunk read is (a) appended to a `.ttyrec` file with a
  `<sec:4><usec:4><len:4>` little-endian header per chunk (for admin replay
  tooling — `enable_ttyrecs` config), and (b) line-buffered and delivered via
  `output_callback` — this is the **stdout/bootstrap channel**, used only
  until the process attaches over its own Unix socket (see 4.2). The header
  chunk written once at start embeds player/game/server/time metadata as raw
  ANSI (`clrscr` + text), visible if the ttyrec is replayed as a terminal
  session.
- **Stderr** is line-buffered similarly and drives crash-reason heuristics
  (`_on_process_error` in `process_handler.py`, regex/prefix matching on
  DCSS's `dbg-asrt.cc: do_crash_dump` output format — `"ERROR ..."`,
  `"crash report: ..."`, `"We crashed!..."`, `"Writing crash info to..."`).
- Exit handling: `poll()` reaps via `os.waitpid(WNOHANG)`, distinguishes
  signal-death vs. normal exit, closes fds, closes the ttyrec file, invokes
  `end_callback`.
- Signals: `SIGHUP` (cooperative stop — DCSS auto-saves and quits on receipt
  of SIGHUP if it's a webtiles game), `SIGABRT` (forced kill after
  `kill_timeout`).

Rust equivalent: `game::process::TerminalProcess`, spawned via the
`pty-process` crate (tokio-integrated `openpty` + `Command`), exposing
`AsyncRead`/`AsyncWrite` halves of the PTY, plus a dedicated stderr pipe.
`.ttyrec` writing is a dedicated `tokio::fs::File` write path, gated by
config exactly like Python.

### 4.2 Crawl↔WebTiles protocol socket (`connection.py:
WebtilesSocketConnection`)
This is **the actual game protocol** — separate from both the PTY and the
browser WebSocket:
- A `SOCK_DGRAM` (**not** stream!) Unix domain socket.
  - The **DCSS process** creates/owns and listens on the path passed via
    `-webtiles-socket <path> -await-connection` (assembled server-side, see
    `_start_process`), format: `<socket_path>/<username>:<yyyy-mm-dd.HH:MM:SS>.sock`.
  - The **webserver** creates its own throwaway datagram socket (temp path
    under `server_socket_path`), and `sendto()`s messages to the crawl
    process's socket path.
- Handshake: webserver sends `{"msg":"attach","primary":true}` (primary =
  this is the actual player connection, not a re-discovered orphan) as the
  first message immediately after `bind()`.
- Framing: every logical message (which may itself be many JSON bytes, e.g.
  a full map/tile bundle) is a **single datagram terminated by `\n`**. Since
  `SOCK_DGRAM` delivers whole datagrams, this newline check is used only to
  detect a message that got fragmented across multiple OS-level datagrams
  (rare, size-related) and needs buffering/concatenation before parsing —
  it is not a stream delimiter in the TCP sense.
- Messages **from** the crawl process over this socket are, with few
  exceptions, forwarded **verbatim** to the browser WebSocket (this is why
  the webtiles wire protocol is "whatever DCSS's json-options/tiles code
  emits" — the webserver does not generally re-encode game state). A small
  set of message types are intercepted server-side because they are meant
  for the webserver, not the browser (prefixed with `*` in the payload, see
  `_on_socket_message`): `client_path` (bootstrap: which client asset
  version to serve, triggers game_data version registration + rendering
  `game.html`), `flush_messages` (switch from immediate-send to queued/batch
  send mode), `dump` (a morgue file was written; used to build a dump URL),
  `exit_reason` (structured game-over reason for lobby/spectators),
  `milestone` (parsed the same as `.milestone`/`.where` files, but pushed
  proactively instead of via file polling).
- Messages **to** the crawl process (player keystrokes/mouse, spectator
  join notice, chat notes, server announcements) are JSON-encoded by the
  webserver and sent as datagrams the same way.
- A full JSON *map* message (`"msg":"map"` with `"clear":true`) is
  special-cased: on new spectator join, it is only forwarded to *freshly
  joined* watchers (not broadcast to everyone), because it can be 100KB+ and
  is otherwise redundant for already-synced clients.
- Bootstrap ordering quirk: until the first message arrives on this socket,
  the webserver still treats **stdout** (via `TerminalRecorder.output_callback`)
  as the place status JSON might appear (backwards compat with very old
  "wrapper script" deployments that print JSON on stdout instead of using the
  socket); the first socket message unconditionally disables the stdout
  callback.

Rust equivalent: `game::process::GameSocket` wrapping `tokio::net::UnixDatagram`
with a small re-assembly buffer, `game::codec` for the `*`-prefixed
control-message subset (typed), everything else passed through as an
opaque, already-validated-UTF8 `Bytes` payload (avoid unnecessary
JSON parse/re-serialize round trips — see Performance requirements).

### 4.3 Stale-lock / crash recovery (`process_handler.py:
_purge_locks_and_start` et al.)
Before starting a new game for a username, the handler scans
`inprogress_path` for a `<username>:*.ttyrec` lock file. If found, it reads
the PID inside, and:
- signals it with `SIGHUP` (10s grace) then `SIGABRT` if unresponsive,
  notifying the connecting client (`stale_processes` message with a
  countdown) and requiring **no user_action** unless it lingers (then
  `force_terminate?` is sent, and the client is expected to answer via a
  `force_terminate` message).
- if the recorded PID doesn't exist (`ProcessLookupError`) or belongs to
  another user (`PermissionError`), the lock file is deleted directly.

Rust equivalent: `game::manager::purge_stale_lock`, same on-disk protocol
(lock file format: 3 lines — pid, terminal lines, terminal columns).

### 4.4 Discovery of externally-started games
`process_handler.py: watch_socket_dirs`/`handle_new_socket` uses inotify
(`inotify.py: DirectoryWatcher`) on every configured `socket_path` to detect
`.sock` files created/deleted by DCSS processes that were **not** started by
this webserver instance (e.g. started by a wrapper script, or another
webserver process during a hot-reload/restart). On creation, a
`CrawlProcessHandler` is built around the found socket and connected
non-primary (`attach: primary=false`); on deletion, the process is treated
as ended. Rust equivalent: `notify` crate watcher, same semantics, only
enabled when `watch_socket_dirs` is set (default `False`).

## 5. Authentication & accounts (`auth.py`, `userdb.py`)

- **Password storage**: SQLite `dglusers` table (`password_db`), password
  hash stored via POSIX `crypt(3)` (Python's `crypt` module → glibc). Salt
  format selected by `crypt_algorithm` config: `"broken"` (legacy DES,
  salt = the password itself — a deliberate backward-compat wart, **not**
  something to "fix" without an explicit opt-in since it would invalidate
  every existing password on old servers), or a glibc crypt id (`"6"` for
  SHA-512, etc.) with a random salt of `crypt_salt_length` chars, or (a
  local patch already present in this checkout) a hardcoded SHA-512 fallback
  when `crypt_algorithm` is falsy. Passwords are truncated to
  `max_passwd_length` (default 20) *before* hashing — a deliberate legacy
  compatibility behavior, must be preserved exactly.
- **Login**: `userdb.user_passwd_match` looks up by case-insensitive
  username, checks `dgl_is_banned` first (banned accounts always fail,
  independent of password), then `crypt.crypt(input, stored_hash) ==
  stored_hash`.
- **Login cookies / token login**: `auth.py` keeps an in-memory
  `{(token, username): expires}` map (128-bit random token via
  `random.SystemRandom`), purged hourly. `set_login_cookie` sets an actual
  HTTP cookie for `MainHandler` requests, or hands the token directly to
  websocket-only clients (`login_cookie` message) since websocket handlers
  can't set cookies themselves in this flow.
- **Password reset**: `recovery_tokens` SQLite table, token lifetime
  `recovery_token_lifetime` hours, emailed via `util.send_email` (SMTP,
  best-effort/optional — most self-hosted servers leave email unconfigured
  and reset is admin-assisted via the CLI instead).
- **Account flags** (`dglusers.flags` bitmask, shared bit layout with
  `dgamelaunch`): `ADMIN=1`, `LOGIN_LOCK=2` (ban), `PASSWD_LOCK=4`,
  `EMAIL_LOCK=8` (both also implied by "hold"), `ACCOUNT_HOLD=16` (webtiles-
  specific soft restriction — allowed to log in/play with reduced
  privileges, e.g. `-no-player-bones`, but excluded from the public lobby),
  `WIZARD=32`, `BOT=64`.
- **Registration**: `nick_regex` config validates usernames; `bans.py`
  additionally checks a configurable ban list (`banned_players.yml`/`.txt`)
  by literal name or regex.

Rust equivalent: `auth.rs` for the in-memory token map + password hashing,
`userdb.rs` wrapping `rusqlite` with the identical schema (so existing
`passwd.db3`/`user_settings.db3` files are used as-is — this is a hard
compatibility requirement, not just a protocol requirement).

**Intentional deviation**: password hashing itself is *not* ported as-is.
New/changed passwords are hashed with Argon2id (`argon2` crate, PHC string
format) instead of `crypt(3)`-based schemes — there is no wire-protocol or
file-format reason to keep the legacy scheme, and it is weaker (non-memory-
hard, and the `"broken"` mode literally uses the password as its own salt).
Existing accounts migrated from a Python-server `passwd.db3` still have
`$1$`/`$5$`/`$6$` (glibc MD5/SHA-256/SHA-512 crypt) or bare-salt (DES
crypt, `"broken"` mode) hashes; these are verified using the pure-Rust
`pwhash` crate (no FFI/system `libcrypt` linking) and transparently
upgraded to Argon2 on the next successful login. See `src/auth.rs`.

## 6. Configuration (`config.py` + `webtiles/config.py` + `games.d/*.yml`)

Three layers, later layers override earlier ones for scalar keys (except
`games`, see below):
1. **Built-in defaults** — `webtiles.config.defaults` dict (used whenever a
   key is entirely absent).
2. **`config.py`** (or whatever module path is passed to
   `init_config_from_module`) — plain Python module-level assignments (a
   full Python file: can contain conditionals, loops, imports). This is the
   file server operators are expected to *copy and edit* or leave as-is.
3. **`config.yml`** (optional, same directory as `config.py`) — YAML map,
   merged key-by-key over `config.py`. `games` in `config.yml` **replaces**
   (does not merge with) `games` from `config.py` if both are set (with a
   logged warning). `banned` entries are *appended* rather than replacing.
4. **Command-line arguments** (`--port`, `--ssl-port`, `--logfile`,
   `--daemon`/`--no-daemon`, `--no-pidfile`, `--live-debug`) override
   whatever the above produced (`export_args_to_config`).

Game definitions (`GameConfig`, a `dict`-like with template inheritance) are
loaded either from a `games` dict directly in `config.py`/`config.yml`, or
—when `use_game_yaml` allows it—from every `*.yml` file under `games.d/`
(each declaring `games:`/`templates:` lists; `games.d/base.yml` is the
in-repo example). Each game entry supports `%n` (username), `%v`/`%V`/`%r`
(version) string templating for most path/URL fields (not `socket_path`).
A `default` template applies to any game lacking an explicit `template:`.

Rust equivalent: `config.rs` defines strongly typed
`ServerConfig`/`GameConfig`/`GameTemplate` structs deserialized with `serde`
from the same YAML documents (`config.yml`, `games.d/*.yml`), plus a
`clap`-derived CLI struct for the override layer. `config.py` itself (an
executable Python file) is **not** re-interpreted by the Rust server —
operators migrating to the Rust server must express any config currently
only expressible via arbitrary Python logic as plain YAML; the repo-tracked
`config.py` defaults are transcribed into the Rust `Default` impls /
`config.yml.example`, see `MIGRATION.md`.

## 7. Logging

Python: standard `logging` module, single rotating file handler or stdout,
level from config, with `tornado.access` suppressed to `WARNING` to hide
per-request 200s. Per-connection/per-process log lines are prefixed via a
`LoggerAdapter` (`#<conn-id>`/`P<process-id>`).

Rust equivalent: `tracing` + `tracing-subscriber` (env-filter + optional
rolling file appender via `tracing-appender`), with `connection_id`/
`game_id`/`username` as structured fields (via `tracing::Span`) rather than
string-prefixed messages — strictly more useful while remaining
log-format-compatible enough for admins (timestamps + level + message).

## 8. Directory layout of the Rust crate

```
webserver-rs/
├── ARCHITECTURE.md      (this file)
├── PROTOCOL.md          (wire-format reference, with worked examples)
├── MIGRATION.md         (Python retirement plan)
├── README.md            (build/run/config instructions)
├── Cargo.toml
├── config.yml.example
├── src/
│   ├── main.rs
│   ├── lib.rs
│   ├── config.rs
│   ├── error.rs
│   ├── state.rs
│   ├── auth.rs
│   ├── userdb.rs
│   ├── http/
│   │   ├── mod.rs
│   │   ├── handlers.rs
│   │   └── game_data.rs
│   ├── websocket/
│   │   ├── mod.rs
│   │   ├── connection.rs
│   │   └── compression.rs
│   ├── protocol/
│   │   ├── mod.rs
│   │   ├── client.rs      (typed client->server messages)
│   │   ├── server.rs      (typed server->client messages)
│   │   └── codec.rs       (batching + raw-deflate codec, tested)
│   └── game/
│       ├── mod.rs
│       ├── manager.rs     (game registry, lobby fan-out)
│       ├── session.rs     (per-game state: watchers, chat, blocklist)
│       ├── process.rs     (PTY spawn, ttyrec, exit handling)
│       └── socket.rs      (Unix datagram protocol to the DCSS process)
└── tests/
    ├── codec.rs
    └── http.rs
```
