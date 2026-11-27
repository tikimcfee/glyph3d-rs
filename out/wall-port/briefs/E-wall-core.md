# Work package E: a port into `crates/wall-core`

You are porting one small part of a browser app's logic to Rust. Read
`out/WALL-PLAN-2026-11-26.md` first for what the whole thing is and the rules everyone
works under, then this. The message that sent you here names YOUR modules.

## What this is

The wall is an infinite canvas of terminals. Its browser version keeps its rules
(where a terminal lands, how a group tiles, what a chord does, what a right-click
offers) in plain JavaScript modules with no DOM in them, each with a headless test
file. Those modules are the specification of how the wall behaves, and the person who
uses it every day expects the native app to behave the same. You are carrying a few of
them across, tests and all.

- The JS, read-only: `~/localdev/viz-web/glyph3d-js/wall/` and its tests in
  `~/localdev/viz-web/glyph3d-js/tools/`. `docs/architecture/wall.md` there
  describes the behaviour in prose.
- The Rust: `crates/wall-core/src/<module>.rs` in this worktree. `lib.rs` already
  declares every module; `geom.rs` (`Rect`, `Point`: world units, **y up**, min and max
  corners, exactly the JS `{ minX, minY, maxX, maxY }`) and `input.rs` (`Mods`,
  `Platform`, `KeyEvent`, `Button`) are shared and already written. Use them.

## How to port

- **Behaviour first, shape second.** Each exported JS function becomes a Rust function
  that returns the same answers for the same inputs. Write Rust a Rust reader would
  expect (enums for the string unions, `Option` for null, structs for the object
  literals, `Result` with a message where the JS throws), but do not redesign the
  logic or "fix" it. If something in the JS looks wrong, port it as it is and tell the
  lead.
- **Numbers are `f64`**, as in JS, so the ported tests compare the same values.
- **Port the tests.** Every assertion in the module's JS test file that concerns your
  module becomes a Rust test (`#[cfg(test)] mod tests` in the same file, or
  `tests/<module>.rs` if it is long), with the same inputs and the same expected
  values. A JS test that needs a DOM, a real terminal or a stand-in wall object is
  outside your module: skip it and list it in your reply. Do not weaken an assertion to
  make it pass; if a ported test fails, your port is wrong or you have found a
  difference worth reporting.
- **Where the JS reaches for the browser** (`navigator.platform`, an event object,
  `localStorage`, `performance.now()`), take it as a parameter instead:
  `input::Platform`, `input::KeyEvent` or `input::Mods`, a path or a trait for storage,
  a time in milliseconds.
- **Head each file** with a `//!` comment naming the JS module it ports and what the
  module decides. Give public items a doc comment that says what they do; carry the
  JS's own comments over where they explain a rule, in your words.
- **Stay in your modules.** Do not edit `geom.rs`, `input.rs`, `lib.rs` or another
  agent's module. If the shared types lack something you need, add it in your own
  module and say so, and the lead will move it. If you need another module that is not
  written yet, write the smallest private helper that unblocks you and say so.
- A dependency on `serde`/`serde_json` is fine where data is saved or loaded; nothing
  else should be needed.

## Done

`cargo test -p wall-core` passes from this worktree with your tests in it, and `cargo
clippy -p wall-core --all-targets` says nothing about your files. Commit on this
worktree's branch as you go, saying what you ran.

Your final reply: each JS export and where it went; how many of the JS test's
assertions you ported and which you skipped and why; anything in the JS you believe is
a bug; anything the shared types should gain.
