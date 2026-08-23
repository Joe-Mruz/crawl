# webtiles-rs

A Rust (Axum + Tokio) rewrite of the DCSS WebTiles server, protocol-compatible
with the Python implementation at `../webserver`. See:

- [`ARCHITECTURE.md`](ARCHITECTURE.md) - component-by-component mapping to
  the Python implementation, with citations.
- [`PROTOCOL.md`](PROTOCOL.md) - the wire protocol reference.
- [`MIGRATION.md`](MIGRATION.md) - what's implemented, what isn't, and the
  path to production readiness.

## Building

```sh
cd crawl-ref/source/webserver-rs
cargo build --release
```

Requires a Rust toolchain (edition 2021). No system libraries beyond a
C compiler/linker are required (SQLite is vendored via `rusqlite`'s
`bundled` feature; password hashing is pure Rust).

## Running

The server needs the same on-disk layout as the Python server: a
`config.yml`/`games.d/` directory, a `passwd.db3`/`user_settings.db3`,
template files, and static assets. By default it looks for a `webserver/`
directory next to the `webserver-rs` checkout it was built from
(resolved relative to the binary's own location, not the current working
directory - matching Python's `os.path.dirname(os.path.abspath(__file__))`
default), so it can usually be run from anywhere with no flags:

```sh
./crawl-ref/source/webserver-rs/target/release/webtiles-rs --port 8080
```

Use `--server-path` to point at a different directory explicitly:

```sh
./webtiles-rs --server-path /path/to/webserver --port 8080
```

### Command-line options

| Flag | Meaning |
|---|---|
| `--server-path <dir>` | Directory containing `config.yml`/`games.d/`. Defaults to `webserver/` next to the `webserver-rs` checkout (see above); the server logs the resolved path and game count at startup, and warns if no games were found. |
| `-p, --port <port>` | Bind an HTTP port, disabling SSL (matches `webtiles/server.py`'s `-p`). |
| `--ssl-port <port>` | Bind an SSL port (SSL/TLS is not yet implemented - see `MIGRATION.md`). |
| `--logfile <path>` | Reserved for parity with the Python CLI; logging is currently controlled by `RUST_LOG` (see below). |
| `--daemon` / `--no-daemon` | Reserved; daemonization is not yet implemented. |
| `--no-pidfile` | Reserved; pidfile handling is not yet implemented. |
| `--live-debug` | Debug mode: disables `watch_socket_dirs`, daemonizing, and the pidfile. |

### Logging

Uses [`tracing`](https://docs.rs/tracing). Control verbosity with the
standard `RUST_LOG` environment variable, e.g.:

```sh
RUST_LOG=info ./webtiles-rs --port 8080
RUST_LOG=webtiles_rs=debug,tower_http=debug ./webtiles-rs ...
```

## Configuration

Configuration is loaded exactly like the Python server: built-in defaults,
then `<server-path>/config.yml`, then `<server-path>/games.d/*.yml` (unless
`games` was already set in `config.yml`). See `ARCHITECTURE.md` §6 for the
full layering rules and `PROTOCOL.md` §7 for game-config string templating
(`%n`/`%v`/`%V`/`%r`).

`config.py` itself (an executable Python file) is **not** read by this
server - only `config.yml` and `games.d/*.yml`. If your deployment
currently configures things only via arbitrary Python logic in
`config.py`, express the equivalent as YAML.

Key defaults (see `src/config.rs` for the complete, authoritative list):

| Key | Default |
|---|---|
| `dgl_mode` | `true` |
| `bind_port` | `8080` |
| `password_db` | `./webserver/passwd.db3` |
| `max_connections` | `100` |
| `connection_timeout_secs` | `600` |
| `max_idle_time_secs` | `18000` |
| `allow_anon_spectate` | `true` |
| `max_chat_length` | `1000` |

## Testing

```sh
cargo test
```

Most tests run in a normal sandbox. A few require creating real Unix
domain sockets, spawning real subprocesses, or binding real TCP
listeners:

- `tests/process_smoke.rs` - spawns real processes through a PTY.
- `tests/real_crawl_handshake.rs` - spawns the actual compiled `crawl`
  binary (skips itself if `../crawl` doesn't exist) and performs the real
  webtiles protocol handshake over a real `AF_UNIX`/`SOCK_DGRAM` socket.
- `tests/http_integration.rs` - runs the real Axum app on a real bound
  socket and exercises it over real HTTP/WebSocket connections.
- `src/game/socket.rs`'s `attach_handshake_round_trips_over_real_unix_sockets`
  unit test - same, at the unit level.

If your environment sandboxes socket/process syscalls, these specific
tests need to run unsandboxed; everything else (config parsing, the
protocol codec, auth, the user database, session/game-registry logic,
crash-reason parsing, template rendering) runs anywhere.

## Project layout

See `ARCHITECTURE.md` §8 for the annotated directory tree.
