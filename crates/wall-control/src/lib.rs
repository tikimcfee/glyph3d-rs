//! Local Unix-socket control plane (`Server`, `Client`, and `wallctl`) for driving the
//! native terminal wall's command router without an HTTP/WebSocket relay.
//!
//! ## Socket path & credential rules
//! Follows the exact directory and peer-check rules of `crates/termd-client/src/paths.rs`:
//! 1. **Directory**: `$G3D_TERMD_DIR` when set and non-empty, else `/run/user/<uid>/g3d`
//!    (when `/run/user/<uid>` exists and is a real `0700` directory owned by `<uid>`),
//!    else `/tmp/g3d-<uid>`. Created with mode `0700` if absent, and verified to be a
//!    real (non-symlink) directory owned by `<uid>` with `mode & 0o077 == 0`.
//! 2. **File name**: `wall-<namespace>.sock` (default namespace: [`DEFAULT_NAMESPACE`],
//!    `"rs-wall"`), created with mode `0600`, capped at [`MAX_SOCKET_PATH_BYTES`] (`100` B).
//! 3. **Same-UID peer check**: Both [`Server`] (on `accept`) and [`Client`] (on `connect`)
//!    query kernel peer credentials (`getpeereid` on macOS/BSD, `SO_PEERCRED` on Linux)
//!    and immediately drop/refuse peers whose effective UID differs from [`paths::current_uid`].
//! 4. **Stale takeover vs live refusal**: When `wall-<namespace>.sock` already exists on
//!    disk, [`Server::bind`] probes it with `connect()`. If a live server answers, binding
//!    fails with [`Error::AlreadyRunning`] without unlinking the socket; if the kernel
//!    reports `ECONNREFUSED` (leftover file from a dead process), the stale socket is
//!    removed and replaced cleanly.
//!
//! ## Wire protocol
//! Plain UTF-8 text over a persistent `AF_UNIX` `SOCK_STREAM` connection, usable directly
//! with `nc -U`:
//!
//! - **Request (client → server)**: one single-line command terminated by `\n`
//!   (optional `\r\n` is also accepted; max [`MAX_REQUEST_BYTES`] = `1 MiB`):
//!   ```text
//!   wall.split right\n
//!   ```
//! - **Reply (server → client)**: an ASCII decimal byte-length header line (`<byte_len>\n`),
//!   followed by `<byte_len>` raw UTF-8 bytes of the router's reply text, followed by a
//!   trailing `\n` frame terminator (max [`MAX_REPLY_BYTES`] = `16 MiB`):
//!   ```text
//!   24\n
//!   OK: split t1 right -> t2\n
//!   ```
//!   Multi-line replies (such as `help` boxes or `wall.list` tables, including blank lines
//!   or leading dots) round-trip byte-for-byte without escaping:
//!   ```text
//!   49\n
//!   ID  NAME\n
//!   ──  ────\n
//!   t1  zsh\n
//!   \n
//!   (1 terminal)\n
//!   ```
//!   Refusals carry `CommandRouter`'s `"ERR: ..."` prefix ([`Reply::is_err`]):
//!   ```text
//!   40\n
//!   ERR: unknown command 'wall.nope'. Try 'help'.\n
//!   ```
//!   Interactive `nc -U /tmp/g3d-$(id -u)/wall-rs-wall.sock` sessions can type multiple
//!   commands back-to-back on one connection and read `<byte_len>` + reply after each line.

pub mod client;
pub mod paths;
pub mod server;
pub mod wire;

pub use client::{send_one, Client};
pub use paths::{Target, DEFAULT_NAMESPACE, MAX_SOCKET_PATH_BYTES};
pub use server::{Request, Responder, Server, ServerBuilder, WakeFn};
pub use wire::{format_command_line, Error, Reply, MAX_REPLY_BYTES, MAX_REQUEST_BYTES};
