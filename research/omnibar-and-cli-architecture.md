# Architecture: External CLI, Ingress, and the 2D Omnibar

This document records the architectural analysis of `glyph3d-js`'s communication bus, external CLI ingress, and internal 2D Omnibar (Command Palette), mapping how to adapt these patterns natively into `glyph3d-rs`.

---

## 1. The `-js` Communication Architecture

In `glyph3d-js`, all commands—whether issued by a human in the browser UI or by an automated CLI / agent—flow through a single unified dispatch point: `CommandRouter`.

```
                       ┌─────────────────────────┐
                       │  External CLI / Agents  │
                       │   (glyph3d-cli, hook)   │
                       └────────────┬────────────┘
                                    │ (WebSocket / IPC)
                                    ▼
┌──────────────────┐       ┌─────────────────┐       ┌──────────────────────┐
│  Internal 2D UI  │──────▶│  CommandRouter  │◀──────│  Relay Server (Go)   │
│ (Omnibar ⌘K/Ctrl-K)      │ (Unified Dispatch)      │ (1 Display, N Ctrls) │
└──────────────────┘       └────────┬────────┘       └──────────────────────┘
                                    │
                                    ▼
                       ┌─────────────────────────┐
                       │   Live Scene / Layout   │
                       │  (Idempotent apply())   │
                       └─────────────────────────┘
```

### A. The Relay & External Connection Model (`cli/relay.go`, `cli/main.go`, `WebSocketBridge.js`)
- **Topology**:
  - **1 Display**: The active visualization window connects via WebSocket and registers as `"display"`.
  - **$N$ Controllers**: External CLI processes (`glyph3d-cli`), test harnesses, and agent hooks connect as `"controller"`.
  - **$N$ Sensor Sources**: Upstream-only streams (e.g. webcam, hand-tracking) pushing frame envelopes.
- **Protocol**:
  - The CLI sends `repo.load <path>`, `camera.fitall`, or `select <id>`.
  - The relay routes to the display: `{"from": "ctrl-1", "cmd": "..."}`.
  - The display's `WebSocketBridge` receives the message, invokes `CommandRouter.execute(cmd, { sender })`, and returns:
    ```json
    { "to": "ctrl-1", "response": result.text, "data": result.data }
    ```

### B. The Omnibar (`app/client/CommandBar.jsx`)
- **Summoned Modal**:
  - Toggled with `⌘K` / `Ctrl-K` or the UI ButtonBar.
  - Sits on top of the 2D DOM layer with defensive keystroke isolation (`stopPropagation` + focus locking), preventing 3D camera flight or scene verb hotkeys from firing while typing.
- **Two Seamless Modes (Zero-Toggle)**:
  1. **Search Mode (Default)**: Typing queries a unified fuzzy index of nouns (files, sheets, layouts) and verbs (registered commands).
     - Subtitles display the exact verb syntax (e.g. `repo.load /path`), actively teaching the user the CLI command vocabulary.
  2. **Command Mode**: Typing a known verb followed by a space (e.g. `repo.load `) switches into a live REPL with syntax hints and history navigation (`↑`/`↓`).
- **Direct Dispatch**:
  - The Omnibar contains zero business logic—it simply executes `router.execute(line)`, identical to an incoming command from `glyph3d-cli`.

### C. State Intent vs Output (`STATE_ARCHITECTURE.md`)
- **Store Intent, Not Outputs**:
  - `field.source`: can be `{ type: 'repo', ref }` or `null` (clean canvas).
  - `repo.clear`: tears down the scene into a clean slate without exiting the app.
  - `apply()`: idempotent push from state onto the live GPU scene objects.

---

## 2. Adapting to Native `glyph3d-rs`

### A. Empty World State (`SceneChoice::Empty`)
- Replace the mandatory fallback to `main.rs` with an optional `--empty` canvas or empty default state.
- The canvas provides a grounded coordinate space (environment grid, horizon, axes) with 0 initial files.

### B. Grounded Worldspace & Environment
- As documented in `SceneEnvironment.jsx`, a void is disorienting:
  - **Raised Value Range**: Viewport background at mid-grey (~`#343a45`), with grid lines lighter than the background for readability.
  - **Analytic Infinite Grid**: Vertex/fragment worldspace grid shader with `fwidth()` anti-aliased lines, major/minor divisions (e.g. 200 / 2000 units), colored origin axes (Red X, Blue Z), and distance fade.
  - **Skydome / Ground Plane**: Camera-following subtle gradient skydome or ground plane.

### C. Native `CommandRouter`
- Define a unified `CommandRouter` in Rust:
  - Namespace verbs: `repo.load`, `repo.clear`, `camera.fitall`, `camera.reset`, `layout.mode`, etc.
  - Return `{ text: String, data: Option<serde_json::Value> }`.
  - Dispatches commands whether originated from egui Omnibar or IPC socket.

### D. 2D Omnibar in `egui-ui`
- Floating modal dialog summoned via `Ctrl-K` / `⌘K`.
- Focused text input with autocomplete suggestions and history.
- Input isolation integrated into `WindowState` event handling.

### E. Background IPC Listener
- A lightweight thread listening on a local UNIX domain socket or localhost port for external tool integration.
