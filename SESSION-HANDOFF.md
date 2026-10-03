# SESSION-HANDOFF — `glyph3d-native` Spatial Agent Stacks & Focus Locking

**Date**: 2026-10-02  
**Handoff From**: Antigravity Pair-Programming Session  
**Branch with Completed Work**: `worktree-workspace-random-experiments` (HEAD @ `92d27c1`)  
**Target Repository Checkout**: `/Users/lugo/localdev/viz-native/glyph3d-native` (`main` / `spatial-alignment`)  

---

## 1. What This Repository Is

`glyph3d-native` is a 100% pure Rust 3D GPU code and spatial workspace visualizer capable of loading and laying out massive codebases (e.g. 1,306 files, 97 MB source, 95.2 million glyph instances) in sub-second time (~0.57s) and rendering at 120 FPS on Apple Silicon Metal and Vulkan.

### Core Non-Negotiable Invariants:
1. **Output Neutrality**: The renderer's output is the contract. Every refactor, component addition, or architectural change must be provably output-neutral against the 9 golden pixel baselines (`pixel-ab`) and the pick oracle (`pick-oracle`).
2. **Zero Compiler Warnings**: `cargo clippy --release -p glyph3d-native` and `cargo doc --no-deps` must pass with ZERO warnings.
3. **Test Floor Ratchet**: `test_floor` in `build.toml [settings]` is a ratchet. When new tests are added, the floor must be raised in the same commit. Currently at **158** (157 unit tests + WGSL shader validation).
4. **Pure Rust**: Zero external C-ABI/Mojo dependencies. All layout runs via `HyperLayout` (`native/src/layout_hyper.rs`) with Rayon parallel processing and unified-memory shared buffers.

---

## 2. Executive Journey: What Was Done in This Session

In this session, we transformed the spatial layout engine from an ad-hoc arrangement into a clean, modular, headless **Bevy ECS Scene Graph** and established the foundation for **Agent Stacks** and **3D Workdesks**.

Here is the exact progression of what was designed, implemented, and verified:

### A. Performance Debugging & Elimination of Degenerate Loops
- **Carrel/Entity Drag Regression**: Moving an entity or carrel previously dropped frame rates precipitously. We diagnosed that `sync_to_group_rows` was querying all 1,306+ files in the ECS world on every mouse move and issuing thousands of redundant GPU buffer writes. We fixed this by introducing ECS change detection (`Changed<Transform>`) to only dirty-mark moved entities, and implemented batched GPU uploads (`write_group_rows`).
- **Per-Frame Mesh GPU Uploads**: `MeshPipeline::prepare` was writing quads and cubes to GPU memory at 60/120Hz on every frame, even when completely static. We introduced `SceneMeshDraws::dirty` tracking so buffer uploads only happen when meshes or transforms mutate.
- **$O(1)$ Drag Anchor**: Replaced a recursive subtree AABB bounds calculation on every cursor move with an $O(1)$ world translation lookup.

### B. Modularization & Code Shape Refactor
We cleanly modularized three oversized files into dedicated submodules:
- `native/src/glyph_scene/` $\rightarrow$ `setup.rs`, `interaction.rs`, `style.rs`, `pick.rs`, `render.rs`, `buffers.rs`, `pipelines.rs`.
- `native/src/spatial_scene/` $\rightarrow$ `spawner.rs`, `query.rs`, `sync.rs`, `alignment.rs`, `deck.rs`, `turn_card.rs`, `workdesk.rs`.
- `native/src/layout_stack/` $\rightarrow$ `algorithm.rs`, `controller.rs`.

### C. Directional Containers (`SpatialAlignment`)
- Ported and generalized spatial layout principles from `glyph3d-js/packages/glyph3d-core/src/collections/layouts/StackContainer.js`.
- Implemented `SpatialAlignment` component (`native/src/spatial_scene/alignment.rs`):
  - Directions: `RowX`, `ColumnY` (up/down), `DepthZ`.
  - Wrapping constraints: `WrapConstraint::None`, `WrapConstraint::Count(usize)`, and `WrapConstraint::Extent(f32)`.
  - AABB-relative placement, spacing, margins, and multi-track wrapping (e.g. Splay grids).

### D. Decks, Rolodex Paging & Agent Turn Cards
- Ported multi-sheet and deck semantics from `Book.js` and `AgentBooks.js`:
  - `Deck` component (`native/src/spatial_scene/deck.rs`) supporting two display modes:
    - `DeckMode::Deck`: Compact 3D Rolodex carousel where the active card sits face-on at $Z=0$, receding into $-Z$.
    - `DeckMode::Splay`: Unfurled 2D grid overview centered on $X=0$.
  - **Circular Rolodex Slot Law**:
    $$\text{slot} = (i - \text{head}).\text{rem\_euclid}(n)$$
    Cards never drift off-axis into empty space; they cycle continuously in order through the carousel track.
  - **`AgentTurnCard` (`native/src/spatial_scene/turn_card.rs`)**:
    - Modeled as a 2-page book spread with a central spine gap:
      - **Left Page (Verso / Mind)**: Turn index, user prompt, assistant thinking, tool call badges.
      - **Right Page (Recto / Material Impact)**: Tool execution results, command output, file diffs.
- **3D Workdesk (`native/src/spatial_scene/workdesk.rs`)**:
  - `Workdesk` entity holding file revisions.
  - `FileRevisionStack`: Multiple edits of the same file cascade along $-Z$ (`z_step = -0.5`) with action tinting:
    - Read: Cyan `[0.2, 0.6, 0.9, 0.8]`
    - Edit: Amber `[1.0, 0.7, 0.2, 0.8]`
    - Write: Emerald Green `[0.2, 0.9, 0.4, 0.8]`

---

## 3. Git Commit History in This Tree

All work is committed on `worktree-workspace-random-experiments`:
- `92d27c1`: `feat(tooling): encode 2026 Rust development rules, hooks, and justfile workflows`
- `4ae60d9`: `fix(spatial): Rolodex carousel slot law, carrel node uniqueness, and rich turn card content`
- `494b9e6`: `feat(spatial): implement Deck, AgentTurnCard, and Workdesk components`
- `923b930`: `feat(spatial): implement SpatialAlignment component and Splay grid wrapping`
- `0b86b47`: `refactor: modularize glyph_scene, spatial_scene, layout_stack`
- `68b9075`: `perf(render): eliminate per-frame mesh GPU re-uploads and O(1) zone translation drag anchor`
- `2b68e1b`: `perf(spatial): eliminate degenerate carrel/entity drag loop via ECS change tracking and batched GPU writes`
- `7f329b3`: `merge: pull in perf/render-optimizations (indexed quads, Greeking bypass, hierarchical block culling)`
- `99f949a`: `feat(spatial): migrate to headless Bevy ECS scene & dedicated 3D MeshPipeline`

---

## 4. The Incoming Agent's Mission: Exactly What to Do Next

When you start in the main checkout (`/Users/lugo/localdev/viz-native/glyph3d-native`), follow this concrete roadmap:

### Step 0: Sync Commits
Ensure the commits from `worktree-workspace-random-experiments` are brought into your active branch:
```sh
git merge worktree-workspace-random-experiments
# Or rebase / fast-forward if clean
cargo check --workspace
cargo test --workspace
```

### Step 1: Camera Focus Locking (`CameraMode::FocusLock`) & Input Isolation
- **The Problem**: In `native/src/glyph_scene/camera.rs`, `KeyD` was mapped to strafe right (`FLY_RIGHT`), and `KeyF` was fly down (`FLY_DOWN`). In `GlyphScene::on_key`, pressing `KeyD` simultaneously flew the camera right and reset the deck demo.
- **The Fix**:
  1. Add `CameraMode::FocusLock` to `GlyphScene`.
  2. When a turn card is clicked (or pressing `KeyF` / clicking HUD), compute the card's world AABB.
  3. Smoothly ease camera eye to `(center.x, center.y, center.z + fit_distance)` with `yaw = 0.0, pitch = 0.0` (pure face-on view).
  4. While in `FocusLock`, bypass `FlyCamera::on_key` so WASD movement is suppressed.
  5. Arrow keys, `[` / `]`, or `N` / `P` exclusively call `deck_prev()` and `deck_next()`.
  6. `Escape` (or clicking away) smoothly transitions back to `CameraMode::Fly`.

### Step 2: Antigravity JSONL Transcript Ingestion
- **Disk Path**:
  Antigravity sessions live at `<appDataDir>/brain/<conversation-id>/.system_generated/logs/transcript.jsonl`.
- **Module `native/src/agent_transcript.rs`**:
  - Deserialize lines with serde: `step_index`, `type` (`USER_INPUT`, `PLANNER_RESPONSE`, `GENERIC`), `created_at`, `content`, `thinking`, `tool_calls`.
  - Group steps into `AgentTurn` records.
  - Spawn real `AgentTurnCard`s in the Bevy ECS world instead of simulated mock cards.
  - Left Page: User prompt, assistant thinking, badges for invoked tools.
  - Right Page: Tool outputs, stdout/stderr, diff snippets.
- **CLI Options**:
  - Add `--transcript <path>` and `--agent-live` to `native/src/cli.rs`.

### Step 3: 3D Workdesk File Stacking
- Extract file paths from tool calls:
  - `view_file` $\rightarrow$ `AbsolutePath` (Action: `Read`)
  - `replace_file_content` $\rightarrow$ `TargetFile` (Action: `Edit`)
  - `write_to_file` $\rightarrow$ `TargetFile` (Action: `Write`)
- Populate the `Workdesk` entity with the active turn's touched files.
- Stack revisions of the same file along $-Z$ (`z_step = -0.5`) with action tints (Cyan, Amber, Green).

### Step 4: Interface Chrome & Deck HUD
- In `native/src/windowed/state.rs` (`egui` rendering pass), add a dedicated **Agent Workspace HUD**:
  - Buttons: `[⏮ First]` `[◀ Prev]` `Turn X of Y` `[Next ▶]` `[⏭ Last]`.
  - Toggle: `[Deck (Rolodex) ⇄ Splay (Grid)]`.
  - Focus Lock Toggle: `[🎯 Focus Spread]` / `[🔓 Free Fly]`.
  - Touched files drawer with jump-to-workdesk action.

---

## 5. Verification Commands

Always run these commands before concluding any task:
```sh
cargo check --workspace
cargo test --workspace
cargo clippy --release -p glyph3d-native
cargo run -p glyph -- test
```
All must exit 0 with ZERO warnings and all golden gates GREEN.

Good luck! The path is paved, the architecture is clean, and the vision is clear.
