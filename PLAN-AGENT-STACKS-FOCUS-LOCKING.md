# Architecture & Implementation Plan: Agent Stacks, Focus Locking & Workdesk Integration

**Date**: 2026-10-02  
**Target Repository**: `glyph3d-native` (`native/`)  
**Context**: Headless Bevy ECS Spatial Scene Graph + WGPU Renderer  

---

## 1. Executive Vision

We are turning `glyph3d-native` into an interactive, spatial environment for observing, navigating, and inspecting autonomous agent workflows. In future iterations, developers will use a release build of `glyph3d-native` to monitor and interact with active agent sessions in real time while continuing to iterate on the codebase.

The current system has laid the foundational 3D ECS container architecture (`SpatialAlignment`, `Deck`, `AgentTurnCard`, `Workdesk`). This plan outlines the concrete execution steps to complete the bridge from disk session data to an interactive, focus-locked 3D experience.

---

## 2. Foundational Architecture Completed

The following systems are implemented, tested, and passing across all verification gates on branch `worktree-workspace-random-experiments` (`92d27c1`):

1. **ECS Scene Graph**:
   - `native/src/spatial_scene.rs`: Headless Bevy ECS `World` setup.
   - `native/src/spatial_scene/spawner.rs`: Entity spawning helpers (`spawn_agent_carrel_demo`, zones, cards, decks).
   - `native/src/spatial_scene/query.rs`: Raycasting, AABB computation, world bounds traversal.
   - `native/src/spatial_scene/sync.rs`: GPU group row sync with change detection.
2. **Directional Containers (`native/src/spatial_scene/alignment.rs`)**:
   - `SpatialAlignment`: `RowX`, `ColumnY`, `DepthZ` with `WrapConstraint::Count(usize)` and `WrapConstraint::Extent(f32)`.
   - AABB-relative placement, spacing, margins, and directional growth.
3. **Decks & Multi-Sheet Navigation (`native/src/spatial_scene/deck.rs`)**:
   - `Deck`: Supports `DeckMode::Deck` (Rolodex carousel) and `DeckMode::Splay` (2D overview grid).
   - Circular Rolodex slot law: `slot = (i - head).rem_euclid(n)`.
   - Smooth cascade offsets: `z_pos = -slot * z_step`, `y_pos = slot * y_cascade`.
4. **2-Page Book Spreads (`native/src/spatial_scene/turn_card.rs`)**:
   - `AgentTurnCard`: 2-page book spread with spine gap.
   - Left Page = Mind (prompt, reasoning, tool badges).
   - Right Page = Impact (tool output, file diffs, command stdout).
5. **3D Workdesk & File Stacks (`native/src/spatial_scene/workdesk.rs`)**:
   - `Workdesk`: Entity container managing file revisions.
   - `FileRevisionStack`: Stacks revisions along $-Z$ with action tints (Read=Cyan, Edit=Amber, Write=Green).
6. **Performance & Invariants**:
   - Dirty-tracking on mesh GPU uploads (`SceneMeshDraws::dirty` flag prevents redundant per-frame buffer writes).
   - $O(1)$ plane drag anchor computation.
   - 157 unit tests passing (`test_floor = 158`), zero warnings, 100% byte-equal pixel baselines across all 9 golden views.

---

## 3. The 4 Next Features to Implement

### Feature 1: Camera Focus Locking (`FocusMode`) & Input Isolation

#### The Problem
In `native/src/glyph_scene/camera.rs`, `FlyCamera::on_key` binds:
- `KeyD` $\rightarrow$ `FLY_RIGHT`
- `KeyF` $\rightarrow$ `FLY_DOWN`
- `KeyW` / `KeyS` $\rightarrow$ `FLY_FWD` / `FLY_BACK`
- `KeyA` $\rightarrow$ `FLY_LEFT`

In `native/src/glyph_scene.rs:328-335`:
```rust
fn on_key(&mut self, ctx: &GpuContext, key: winit::keyboard::KeyCode, pressed: bool) {
    if matches!(self.camera_mode, CameraMode::Fly) {
        self.fly.on_key(key, pressed);
    }
    if pressed {
        self.verb_key(ctx, key);
    }
}
```
When `KeyD` was pressed to spawn/reset the deck demo, the camera also flew right. When navigating turn cards with WASD or arrow keys, camera drift fights page turning.

#### The Solution
1. **Extend `CameraMode`**:
   ```rust
   #[derive(Clone, Debug)]
   pub enum CameraMode {
       Front { zoom: f32 },
       Orbit,
       Fly,
       FocusLock {
           target_entity: bevy_ecs::entity::Entity,
           target_eye: glam::Vec3,
           target_yaw: f32,
           target_pitch: f32,
           lerp_speed: f32,
       },
   }
   ```
2. **Focus Activation**:
   - Left-clicking on any card in an `AgentTurnCard` deck or pressing `F` (or clicking a HUD button) calculates the card spread's world AABB.
   - Optimal framing distance:
     $$d = \frac{\max(\text{width}, \text{height})}{2 \cdot \tan(\text{fov} / 2)} \times 1.15$$
   - Set `target_eye = (center.x, center.y, center.z + d)`, `target_yaw = 0.0`, `target_pitch = 0.0` (face-on).
3. **Input Isolation in Focus Lock**:
   - In `GlyphScene::on_key`, if in `FocusLock`:
     - Do NOT call `fly.on_key`.
     - Arrow keys or `[` / `]` or `N` / `P` invoke `ctrl.deck_prev()` / `ctrl.deck_next()`.
     - `V` toggles `DeckMode::Deck` $\leftrightarrow$ `DeckMode::Splay`.
     - `Escape` (or clicking background) smoothly transitions back to `CameraMode::Fly`.
4. **Smooth Easing**:
   - In `GlyphScene::render` / `tick`, lerp `self.fly.eye`, `yaw`, and `pitch` towards target pose:
     $$\text{eye} \leftarrow \text{lerp}(\text{eye}, \text{target\_eye}, 1.0 - e^{-\lambda \Delta t})$$

---

## 2. Antigravity JSONL Transcript Ingestion

#### Transcript Schema on Disk
Antigravity logs session events directly to:
`<appDataDir>/brain/<conversation-id>/.system_generated/logs/transcript.jsonl` (and `transcript_full.jsonl`).

Each line is a JSON record:
- `step_index: usize`
- `type: "USER_INPUT" | "PLANNER_RESPONSE" | "GENERIC"`
- `source: "USER_EXPLICIT" | "MODEL" | "SYSTEM"`
- `created_at: String` (ISO 8601)
- `content: String` (User request or tool command output)
- `thinking: Option<String>` (Assistant reasoning)
- `tool_calls: Option<Vec<ToolCall>>` where `ToolCall` has `name` and `args`

#### Parser Architecture (`native/src/agent_transcript.rs`)
```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RawTranscriptLine {
    pub step_index: usize,
    #[serde(rename = "type")]
    pub step_type: String,
    pub source: Option<String>,
    pub created_at: Option<String>,
    pub content: Option<String>,
    pub thinking: Option<String>,
    pub tool_calls: Option<Vec<RawToolCall>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RawToolCall {
    pub name: String,
    pub args: serde_json::Value,
}

pub struct AgentTurn {
    pub turn_index: usize,
    pub timestamp: String,
    pub user_prompt: String,
    pub thinking: Option<String>,
    pub tool_calls: Vec<ProcessedToolCall>,
    pub touched_files: Vec<TouchedFileRecord>,
}
```

#### Ingestion Rules:
- A new `USER_INPUT` line begins an `AgentTurn`.
- Subsequent `PLANNER_RESPONSE` lines contribute `thinking` and `tool_calls`.
- `GENERIC` lines following a tool call capture stdout, stderr, or tool return payloads.
- Convert `AgentTurn`s into live `AgentTurnCard` entities in the Bevy ECS world.

#### CLI Ingestion Door:
- Add CLI arguments in `native/src/cli.rs`:
  - `--transcript <path>`: Load and visualize an offline JSONL session.
  - `--agent-live`: Auto-detect the newest session in `~/.gemini/antigravity/brain/` and live-tail it.

---

## 3. 3D Workdesk File Integration

#### Tool Argument Mapping
Extract file targets from tool calls:
- `view_file` $\rightarrow$ `AbsolutePath` (Action: `Read`)
- `replace_file_content` $\rightarrow$ `TargetFile` (Action: `Edit`)
- `write_to_file` $\rightarrow$ `TargetFile` (Action: `Write`)

#### Spatial Workdesk Linking
- Maintain a `Workdesk` entity alongside the `Deck`.
- As the user flips through turns (or as live turns stream in):
  - Populate the `Workdesk` with cards representing the active turn's touched files.
  - Successive edits to the same file stack along $-Z$ (`z_step = -0.5`).
  - Action Tinting:
    - **Read**: Cyan `[0.2, 0.6, 0.9, 0.8]`
    - **Edit**: Amber `[1.0, 0.7, 0.2, 0.8]`
    - **Write**: Green `[0.2, 0.9, 0.4, 0.8]`
- Selecting a file on the Workdesk highlights the corresponding turn card in the deck.

---

## 4. Interface Chrome & Deck HUD

In `native/src/windowed/state.rs` (`egui` rendering pass), add a dedicated **Agent Workspace HUD**:
- **Turn Carousel Bar**:
  - `[⏮ First]` `[◀ Prev]` `Turn 14 of 42` `[Next ▶]` `[⏭ Last]`
  - Slider scrubbing through turns: `0..=turn_count - 1`.
- **Mode Toggle**:
  - `[Deck (Rolodex) ⇄ Splay (Grid)]`
- **Focus Toggle**:
  - `[🎯 Focus Spread (F)]` / `[🔓 Free Fly (Esc)]`
- **Session Stats**:
  - Touched files count, total tools executed, prompt excerpt.

---

## 5. Verification Checklist for Every Step

Every change must strictly satisfy:
1. `cargo check --workspace`
2. `cargo test --workspace` (assert 0 failures; ratchet `test_floor` in `build.toml` if tests are added)
3. `cargo clippy --release -p glyph3d-native` (assert 0 warnings)
4. `cargo run -p glyph -- test` (assert ALL GATES GREEN including pixel-ab and pick-oracle).
