# Agents on the Native Wall — Trying It Live (2026-11-26)

## 1. Launch Command

Run the wall on its default namespace (`rs-wall`) while passively attaching the `agents` daemon alongside it:

```sh
./target/release/glyph3d-native --wall-extra-ns agents
# or:
cargo glyph run -- --wall-extra-ns agents
```

`--wall-extra-ns agents` is off by default; passing it connects passively to `termd-agents.sock` (`Role::Own`, lossy flow, matching the browser wall's attach behaviour) without touching `rs-wall`'s own shell terminals.

---

## 2. What You Should See

- **`gx-<name>` agent terminals on the canvas**: Every live `agent-<name>` terminal in `termd-agents` mounts on the wall as canvas id `gx-<name>`, titled `<name>`.
- **Live status in title bars & map labels**: Once `agent export --json --follow --all` delivers its fleet snapshot, each agent terminal's title bar (and its zoomed-out map label) renders `<name> · processing`, `<name> · waiting`, or `<name> · idle` with its status colour.
- **Opening an Agent View (`⌘A` or title-bar `≡`)**:
  - Focus an agent terminal and press **`⌘A`** (`Ctrl+Alt+A` on Linux), **or** hover its title bar and click the leftmost **`≡`** button (also available under right-click -> `agent view` -> `right` / `left` / `top` / `bottom`).
  - A world-anchored companion card opens beside the terminal (shoving any loose neighbour out of the way and giving that room back when closed or moved to another side).
  - Drag the card's header to move it between `right`, `left`, `top`, and `bottom`. Zoom out past the readable floor and the card collapses to a compact `● <name>` badge; click the badge to ease back in.
  - Inside the card: category filter chips (`All`, `Edit`, `Write`, `Bash`, `Read`, `Search`, `Say`, `Think`, …), expandable tool/turn rows, `Load earlier` pagination, row `jump` (scrolls the terminal's primary scrollback to where that event was written), and an unread `+N` / `Follow live` pill when scrolled away from the tail.
- **Sidebar (`⌘B`) & Command Palette (`⌘K`)**:
  - Both list all live gomuxai agents grouped by host (this host first, `waiting` first).
  - Picking an already-mounted agent reveals and frames its terminal.
  - Closing a `gx-<name>` terminal (`⌘X` hold or `×`) **detaches it locally without killing the running agent** in `termd-agents` (and suppresses `status_follow` from popping it right back). Picking that closed local agent later from the Sidebar or Command Palette reattaches `gx-<name>` directly via `Sessions::reattach` (no wrapper shell). Picking a remote `name@host` agent spawns an `agent attach` wrapper terminal.
- **Zero idle cost when unused**: When no terminal on the wall is an agent and no Agent View, Sidebar, or Palette is open, `WallAgents` spawns zero `agent` subprocesses and zero poll timers.

---

## 3. What Was Tested Live vs Stub-Only

- **Tested against real `~/.local/bin/agent` on `laptop-2` (read-only)**:
  - `FleetFollower` (`agent export --json --follow --all`) and `SessionPoller` (`agent sessions --json`), plus rendering the live `laptop-2` fleet in Sidebar and Command Palette.
- **Tested only with a stub `agent` CLI and scratch namespaces (`probe` + `probe2` playing `agents`, never live `agents` or `rs-wall`)**:
  - Connecting `--wall-extra-ns agents` to the real `termd-agents.sock`, detaching a real running `gx-<name>` terminal, and reattaching it from Sidebar / Palette (`Sessions::reattach`).
  - Live `agent transcript <session> --follow` streaming, 256 KiB tail hydration, `Load earlier` pagination, orphan `update` adoption, and write-time scrollback jumps against a real Claude / Antigravity / Kimi session while it is actively producing turns.
  - Attaching to a remote `name@otherhost` agent over SSH (`attach_argv`).

---

## 4. If a Card Says "No Agent Session Found" (or Unavailable)

Automatic pairing (`resolve_terminal_session`) tries, in order:
1. **Bus `session_id`**: `gx-<name>` or `wall.attach` terminals matched to the gomuxai fleet's `session_id`.
2. **Workgroup name**: terminal id `gx-<name>` or custom terminal name matching a session's `workgroup.name`.
3. **Working directory**: terminal `pwd` / `cwd` matching a single session's `cwd` (if two or more sessions share that directory, auto-pairing refuses to guess and reports the ambiguity).

If a card reports no matching session:
1. **Pick or pin the session directly**: use the session selector in the Agent View card header, or pin it via `wallctl`:
   ```sh
   ./target/release/wallctl wall.agent.session <terminal-id> <session-id>
   # return to automatic pairing:
   ./target/release/wallctl wall.agent.session <terminal-id> auto
   ```
2. **Verify gomuxai sees the session**: run `~/.local/bin/agent sessions --json` in a shell. Nothing in `glyph3d-native` reads harness transcripts directly — if `agent sessions` does not list it, check `launch_config.toml` (`claude_projects_dir`, `agy_brain_dir`, `kimi_sessions_dir`; see `launch_config.example.toml`).
3. **Check `agent` binary discovery**: if you launch `Glyph3D Wall.app` from Finder/Dock where `~/.local/bin` is not on `PATH`, set `agent_binary = "~/.local/bin/agent"` in `launch_config.toml` (`WallAgents` also checks `~/.local/bin/agent` automatically when `agent` is absent from `PATH`).
4. **Plain shell terminals running an agent manually**: if multiple sessions share the same repo directory, either pick the session in the card header or rename the terminal (`⌘K` -> `rename` or right-click -> `rename…`) to the agent's workgroup name.
