# Work package A: `crates/termd-client`

You are building one piece of a larger port. Read `out/WALL-PLAN-2026-11-26.md` first
(the goal, the pieces, and the rules everyone works under), then this.

## What this piece is

The wall is a native app that shows live terminals. The terminals live in a daemon,
`g3d-termd`, which owns each terminal's PTY and process and speaks a public protocol
over a Unix socket. Today a browser reaches it through a Go relay. The native app will
talk to it directly, and this crate is how: a small client library with a reader
thread, so an app's event loop is never blocked on a socket.

It is the first thing the app needs, so favour a narrow, correct client that works
today over a complete one.

## What to read

- `~/localdev/viz-web/glyph3d-js/docs/termd/PROTOCOL.md` — the whole
  contract (version 1.5). If the daemon and that page disagree, the daemon has a bug:
  write it down for the lead.
- `~/localdev/viz-web/glyph3d-js/docs/termd/MACOS.md` — socket path limits
  on macOS.
- The reference client, same repo: `crates/g3d-wire/src/{frame,msg,lib}.rs` (codec and
  message types) and `crates/g3d/src/{client,start,paths}.rs` (a blocking client,
  socket paths, starting the daemon). You may copy and adapt from these (same owner,
  MIT); the result must be edition 2021 (those are 2024 and use let-chains) and depend
  on nothing but std, serde and serde_json. That client blocks on every reply, which
  is exactly what this crate must not do to its caller's other threads.

## What to build

The API in the plan's section A. The shape matters more than the exact names; if a
signature there is wrong for the protocol, build what is right and tell the lead.

- One connection, one reader thread. The reader decodes frames and routes them:
  `REPLY` to whichever caller is waiting on that `req`, everything else onto the
  events channel in arrival order, then calls `wake`.
- Requests can be made from any thread and block only their caller, with a timeout.
- `connect_or_start`: resolve the socket (namespace rule, or an explicit path; honour
  `G3D_TERMD_DIR`), and if nothing listens, start `g3d-termd --ns <ns> --daemon`
  (found beside the current executable, in `~/.local/bin`, or on `PATH`) and retry
  for a few seconds.
- The ops the wall needs now: `list`, `create`, `subscribe` (raw; roles watch, attach,
  own), `unsubscribe`, `take`, `resize`, `repaint`, `view`, `key`, `paste` (chunked
  with `more` when it would pass a frame), `mouse`, raw `INPUT`, `kill`. `status_follow`
  if it comes cheaply.
- Restarts: on `ERROR restart` or a dropped connection, reconnect, subscribe again to
  what was subscribed, ask each for a repaint, and tell the host with one event.
- `lagged`: ask for a repaint yourself, and still pass the event on.

## How you will know it works

Integration tests against the real daemon, installed at `~/.local/bin/g3d-termd`.
Each test starts its own: `G3D_TERMD_DIR=/tmp/g3d-a-<pid>` (the path must stay under
100 bytes: never `$TMPDIR`), a namespace of its own, and it stops that daemon and
removes the directory when done, also when the test fails. If the binary is absent the
tests say so and skip; they do not pass silently for another reason.

**Never connect to namespaces `wall`, `agents` or `default`, and never run without
`G3D_TERMD_DIR` set to your scratch directory.** Ivan's live terminals are in the
default directory.

Cover at least: handshake and `list`; `create` a program that prints, subscribe as
owner, see one `Repaint` then its bytes as `Output`; a `key` event reaches a program
that echoes it; `resize` produces a `Size` event; `paste`; `kill` ends in `Closed`;
two threads making requests while output streams. Also an example,
`examples/termd_tail.rs`: start or connect to a namespace given on the command line,
create a shell, print what it outputs, send stdin lines as pastes. The lead will use
it by hand.

## Handing back

Work in this worktree on its branch; commit as you go. Your final reply: what works
(with the test output), what you left out, anything in the plan's API you changed and
why, and every wart you found in termd itself (what happened, how to reproduce) for
the plan's "termd findings" list.
