# Cross-Namespace Agent Terminals: Browser Wall vs Native Wall (2026-11-26)

## 1. How the Browser Wall Gets Agent Terminals onto the Canvas

### Two `g3d-termd` namespaces
- **`wall` (primary shell namespace)**: `glyph3d-cli serve` (`cli/server.go`, `cli/attach_unix.go`) creates and re-adopts the browser wall's own interactive shells in `--termd-ns wall` (IDs `term-1`, `term-2`, …).
- **`agents` (gomuxai agent namespace)**: `gomuxai` (`agent` CLI) spawns AI coding agents (`claude`, `agy`, `aider`, custom) in `g3d-termd` namespace `agents` under session ID `agent-<name>` (unless an agent record explicitly overrides `namespace` or `session`).

### Discovery and listing (`GomuxWatcher` + `WallBus`)
- The Go relay does not blindly dump every terminal in `agents` onto the canvas. Instead, `GomuxWatcher` (`cli/gomux.go:187-230`) runs `agent export --json --follow --all -q` and cross-checks local sessions against `termdList(ns)` (`attachProbe`, `cli/gomux.go:116-148`) to set `attached: true|false` (whether `agent-<name>` currently exists in `g3d-termd`).
- Snapshots (`GomuxSnapshot { gen, host, dirs, agents }`) stream over WebSocket notifications (`gomux/changed`) into `WallBus` (`wall/WallBus.js`).
- Live agents are listed in:
  1. **Sidebar (`wall/Sidebar.jsx`)** — under an **Agents** section grouped by host (local host first, then remote hosts alphabetically) and sorted within each host by status urgency (`waiting` -> `error` -> `processing` -> `idle` -> `unknown` -> `ended`, then name).
  2. **Command Palette (`wall/Palette.jsx`, `⌘K`)** — searchable `attach: <target>` rows showing status and active tool/detail.
  3. **`wall.bus` (`wall/wallCommands.js:409-443`)** — CLI/script listing.
- An agent's terminal lands on the canvas when requested via `wall.attach <name[@host]>` (Sidebar click, Palette selection, or CLI), or automatically on reload in `TerminalWall._recover()` (`wall/TerminalWall.js:502-521`) when `WallStore.attachedEntries()` remembers an open `gx-<name>` whose local agent is still live and attachable.

### Attaching to the second namespace: role, flow, input, resize, and close
- **Local agents (`name` or `name@<this-host>`)**:
  - `TerminalWall.attach()` (`wall/TerminalWall.js:1399-1442`) sends `{ relay: "gomux.attach", name }`.
  - `handleGomuxAttach` (`cli/gomux.go:472-555`) looks up the agent, resolves `ns = agent.Namespace` (default `"agents"`) and `termId = agent.Session` (e.g. `"agent-foo"`), verifies `termId` is present in `termdList(ns)`, and spawns a dedicated `glyph3d-cli attach` adapter dialing the `"agents"` daemon directly:
    - **`--no-create` (`dialTermd`)**: connects to the existing `"agents"` daemon without ever starting a daemon or creating a shell.
    - **Canvas ID `gx-<sanitized_name>` (`gomuxScreenId`)**: `gx-` prefix (`[A-Za-z0-9._-]`, max 64 chars) prevents any ID collision with primary `wall` terminals (`term-1`, `t1`, …).
    - **`role: "own"` (`cli/attach_unix.go:853-864`)**: takes size ownership so tiling, splitting, grip-resizing, and fullscreen on the wall resize the agent's PTY (`SIGWINCH`). If a human concurrently attaches in another terminal with `g3d --ns agents attach --take agent-foo`, `g3d-termd` (`crates/g3d-termd/src/owner.rs:1414-1427`) demotes the wall's subscription to `role: "attach"` (`EVENT role`) and emits `EVENT size`; the wall yields size ownership and follows the external size.
    - **`flow: "lossy"` (`cli/gomux.go:545`, `cli/attach_unix.go:861-863`)**: overrides `role: "own"`'s default `flow: "block"`. If the viewer stalls or is backgrounded, `g3d-termd` drops intermediate `OUTPUT` bytes past `--lossy-queue` and resyncs via `EVENT lagged` + `REPAINT` instead of pausing PTY reads (`flow: "blocked"`) and freezing the autonomous agent.
    - **Who may type and resize**: the user on the wall has full interactive input (`key`, `paste`, `mouse`, `view` scrollback) at all times, and `resize` whenever the subscription holds `role: "own"`.
    - **`--detach-on-kill` (`detachOnKill: true`, `cli/gomux.go:544`, `cli/attach_unix.go:180-186, 561`)**: closing the pane on the wall (`wall.close gx-foo` / `×` button) drops the `termd` subscription and unmounts `gx-foo` from the wall **without calling `termd.kill`**, so closing an agent's view on the wall never kills the running agent process.
- **Remote agents (`name@<other-host>`)**:
  - Spawns a normal shell in the primary `"wall"` namespace running `WallBus.attachArgv(target)` (`wall/WallBus.js:179-211`): loops `agent attach "$1"` (which hops over `ssh -t`), resets leaked mouse/paste/alt-screen modes (`\033[?1000l…\033[?1049l`) on non-zero exit, and waits for `Enter` (reconnect) or `q` (close).

### Naming and what the agent bus adds
- `WallStore.setAttached("gx-foo", target)` binds the canvas terminal ID to its bus target (`wall/TerminalWall.js:1430`).
- **`nameOf(id)` (`wall/TerminalWall.js:800-804`)**: `store.name(id) || store.attached(id) || shortTitle(windowTitle) || id` — an unnamed attached terminal displays `"foo"` (or `"foo@host"`), never raw `"gx-foo"` or `"agent-foo"`.
- **`titleOf(id)` (`wall/TerminalWall.js:806-810`)**: appends ` · <busStatus>` (`foo · waiting`, `foo · processing`, `foo · idle`, `foo · error`, `foo · ended`) onto both the 3D `TitleBar` and far-zoom `MapLabels`.
- **Rich agent metadata**:
  - Status colour dots and rank sorting in Sidebar and Palette (`waiting` amber `#f2b45a` first so blocked permission prompts surface at the top).
  - Current tool/prompt detail (`agent.detail`, `agent.tool`) in Sidebar/Palette subtitles.
  - Automatic transcript pairing in `WallAgentViews._termMeta()` (`wall/WallAgentViews.js:706-718`): `gx-<name>` resolves `busAgent.sessionId` directly so opening its sister Agent View (`wall.agent gx-foo open`) streams its `gomuxai` `EVENTS.md` transcript beside the live terminal without guessing by `cwd`.

---

## 2. Native Equivalent (`crates/term-session` + `native/src/wall/`)

Behind `--wall-extra-ns <NAME>` (repeatable or comma-separated, plus `wall_extra_ns = [...]` in `launch_config.toml`; **empty / off by default**):

1. **Primary vs extra namespace connections in `Sessions`**:
   - Primary namespace (`--wall-ns`, default `rs-wall`): `SessionHub::open_with` (starts `g3d-termd` if absent, owns `Wall::set_connected` and `Sessions::spawn`).
   - Extra namespaces (`--wall-extra-ns`, e.g. `agents`): passive `SessionHub::connect_with` + `status_follow(Some(500))`. Never starts a daemon on its own; if an extra namespace is not running yet (or stops and later restarts), an independent background watcher connects when its socket appears and adopts its terminals without ever marking the primary wall offline.
2. **Collision-free canvas IDs and labels**:
   - In namespace `agents`, terminal `agent-<name>` maps to **`gx-<sanitized_name>`** (`wall_core::bus::gomux_screen_id(name)`) with initial display title `<name>`, matching `WallStore` and `wall.attach` conventions in `wall-core`.
   - Any other extra-namespace terminal `(ns, raw_id)` maps to **`<ns>/<raw_id>`** (e.g. `probe2/worker1`, `probe2/agent-alpha`), preventing any collision with primary `t1..tN` or across multiple extra namespaces.
3. **Subscription role, flow, and detach-on-close**:
   - Extra-namespace terminals subscribe with **`Role::Own`** + **`Flow::Lossy`** (`SessionHub::attach_with_options`), giving full typing, paste, mouse, scrollback, and native resize while ensuring a stalled/occluded window never blocks an agent's PTY.
   - **`Sessions::kill(id)` (`wall.close`) on an extra-namespace terminal detaches instead of killing**: drops the subscription (`AsyncCmd::Unsubscribe`), suppresses auto-readopt for that terminal until explicitly re-adopted or recreated, and emits `News::Ended { id }` on the next `pump()` so the pane leaves the canvas cleanly while the process in `agents` keeps running unharmed.
   - `Sessions::spawn` (`wall.spawn`) always creates new shells in the primary namespace (`rs-wall`).
