# Migration / Deprecation Plan

## Status

The Python implementation at `crawl-ref/source/webserver/` is **not
deprecated or removed**. It remains the production-ready, fully-featured
WebTiles server. `webserver-rs/` is a parallel, in-progress rewrite.

## What is implemented in `webserver-rs`

Validated with 74 automated tests (unit + in-process HTTP/WebSocket
integration tests + a real end-to-end test against the compiled `crawl`
binary):

- Configuration loading (`config.yml`, `games.d/*.yml`, CLI overrides),
  matching the layering rules in `ARCHITECTURE.md` §6.
- User database (`passwd.db3`/`user_settings.db3`), same SQLite schema as
  the Python server - **an existing database file can be reused as-is**.
- Password hashing: Argon2id for new/changed passwords (an intentional
  improvement over the Python implementation's `crypt(3)`-based schemes;
  see `ARCHITECTURE.md` §5), with transparent, pure-Rust (no FFI)
  verification and on-login upgrade of legacy hashes from a migrated
  database.
- Login tokens (cookie/token-based re-login).
- The WebTiles wire protocol: typed client/server messages, the
  `{"msgs":[...]}` batching format, and the raw-deflate compression scheme
  - byte-compatible with the existing JS client (see `PROTOCOL.md`).
- DCSS process management: PTY spawning, ttyrec recording, crash-reason
  parsing, stale-lock file format - validated against the real compiled
  `crawl` binary (spawn, protocol handshake, clean `SIGHUP` shutdown).
- The crawl↔webtiles Unix-datagram protocol socket, including the
  `*`-prefixed control-message subset.
- Per-game session state: watchers with bounded per-connection queues
  (a slow client cannot block others or the game), chat block/kick,
  lobby-entry construction.
- WebSocket connection handling: login, token login, lobby navigation,
  spectating, chat, connection-limit rejection, periodic ping/pong.
- HTTP endpoints: `/`, `/status/version/`, `/status/lobby/`,
  `/gamedata/<version>/<path>`, static asset serving.
- A minimal, purpose-built template renderer covering the actual Tornado
  template syntax used by `client.html`/`banner.html`/`footer.html`.

## What is NOT yet implemented (do not point real users at this yet)

- **Playing a game end-to-end over the websocket.** The `play`/`get_rc`/
  `set_rc` message handlers, forwarding raw input to an attached game
  process's socket, and the full `start_crawl`/stale-lock-purge orchestration
  that ties `game::manager`, `game::process`, and `game::socket` together
  into "a player clicked Play" are not wired up yet. The pieces exist and
  are individually tested; the orchestration layer connecting them is the
  next major chunk of work.
- Registration, password change/reset (including email sending), admin
  commands (`admin_announce`, `admin_pw_reset`, etc.), RC file
  editing over the websocket.
- `watch_socket_dirs` (discovering externally-started games via inotify).
- The CLI admin subcommands (`password`/`ban`/`flag`, matching
  `wtutil.py`).
- Daemonization, pidfile handling, privilege-shedding, chroot, SSL/TLS
  binding, multiple bind-pairs.
- Byte-exact `/status/lobby/` JSON schema (currently reuses the internal
  `LobbyEntry` shape rather than Python's ad-hoc fields - see
  `PROTOCOL.md` §8).
- Save-slot info collection, milestone file tailing, admin-only socket
  stats in the lobby.

## Recommended path to production readiness

1. Wire up `play`/game-process orchestration in `websocket/connection.rs`
   using `game::manager`, `game::process`, and `game::socket` (all
   individually implemented and tested).
2. Port the remaining message handlers (registration, password flows, RC
   editing, admin commands).
3. Run both servers side-by-side against a shared `passwd.db3`/game
   config during a beta period; compare behavior using real DCSS clients.
4. Add the compatibility-test corpus described in the original task brief
   (captured real message traffic from the Python server, replayed
   against both implementations).
5. Only once dgamelaunch-config-style production deployments have run the
   Rust server successfully for a period should the Python implementation
   be formally marked deprecated - and even then, keep it available for at
   least one release cycle as a fallback.

## Compatibility notes for operators eventually switching over

- Point `--server-path` at the same directory as the existing
  `config.py`'s `server_path` (a `config.yml`/`games.d/` there is read
  identically by both servers - `config.py` itself, an executable Python
  file, is not interpreted by the Rust server; any config expressed only
  via arbitrary Python logic in `config.py` needs a YAML equivalent).
- Point `password_db`/`settings_db` at the existing SQLite files; no
  migration script is needed for those.
- `/status/version/` reports `axum`/`rust` fields instead of
  `tornado`/`python` (see `PROTOCOL.md` §8) - update any monitoring that
  keys off those exact field names.
