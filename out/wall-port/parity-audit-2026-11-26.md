# JS-to-Rust Wall Parity Audit (2026-11-26)

- **JS baseline**: `/Users/lugo/localdev/viz-web/glyph3d-js/wall/` (all 44 `.js`/`.jsx` files) + wall-used modules in `packages/glyph3d-core/src/` @ `7addab95`
- **Rust target**: `glyph3d-rs` (`wall` @ `f5f5c35` + `wall-root` follow-ups & link rules)
- **Scope**: Every user-visible or triggerable behaviour — verbs, chords, gestures, menu items, panel controls, settings, notices, animations, persistence rules, rendering/VT details, and edge cases.
- **At a glance**:
  - **Verbs**: 53/54 JS `wall.*` verbs ported (`wall.token` deliberately disabled on native) + 3 Rust additions (`wall.focus`, `wall.palette`, `wall.agent.follow`) + 7 `terminal.*` + 2 router built-ins — all 56 native `wall.*` verbs are live-wired end-to-end (`wall.bus`, `wall.attach` with local `--wall-extra-ns agents` `gx-<name>` reattach preference, and the five stream verbs `wall.agent.{expand,collapse,show,earlier,jump}` routed through `WallAgents::run_verb_with_screens`).
  - **Chords & plain keys**: 31/31 `KEYMAP` chords + 5/5 plain keys ported and live-wired.
  - **Config knobs**: 47/47 JS knobs ported + 5 native additions (`fontSize`, `lineHeight`, `panelOpacity`, `greekPx`, `cursorBlinkMs`) = 52 total. 49 are live-wired end-to-end (including `boldIsBright` and all 4 `agentView*` knobs: `agentViewSide`, `agentViewWidth`, `agentViewHeight`, `agentViewGap`); 3 browser-only knobs (`renderLoop`, `traceInput`, `menuHint`) stay in `WallConfig`/`WallStore` for browser import round-tripping and are hidden from the native Settings panel.
  - **Smoke coverage**: `tools/wall-smoke.sh` (26 live checks) + `tools/wall-smoke-text.sh` (6 live checks) + `tools/wall-smoke-panels.sh` (12 live checks) + `tools/wall-smoke-agents.sh` (13 live checks) + 288 `wall-core` tests + 137 `glyph3d-native --lib wall::` unit/GPU tests.

---

## 1. Missing — Ranked by How Much Ivan Would Notice

1. ~~**Live gomuxai agent bus & Agent View companion cards are not plugged into `app.rs` / `mount.rs` yet**~~ — **Landed & live**
   - *What now exists*: `WallAgents` (`native/src/wall/agents.rs`) is hooked into `native/src/wall/app.rs` with `WakeGate`-coalesced wakes and zero `agent` invocations while idle; `FleetFollower`, `SessionPoller`, and `LineFollower` (`crates/agent-feed`) populate `wall.bus`, Sidebar, Command Palette, and 3-span `<name> · <status>` title bars / map labels; `AgentViewPanel` (`native/src/wall/panels/agent_view.rs`) draws world-anchored cards on all 4 sides (`R`/`L`/`T`/`B`), collapses to a clickable far-zoom badge below `BADGE_FLOOR_PX`, and syncs `stick_bottom`/unread badges; `WallAgents::run_verb_with_screens` handles `wall.agent.{expand,collapse,show,earlier,jump}` + write-time primary scrollback anchors (`note_terminal_scrolled`); `WallCore` wires `AgentViews::footprint_changed` (`shove_aside` / `give_room_back`) on open, close, side change, and `agentView{Width,Height,Gap}` resize, and steps `free_flow_slot` spawns past `terminal + open card`; `--wall-extra-ns agents` mounts live `agent-<name>` terminals as `gx-<name>`, pairs them automatically, detaches locally on `wall.close`, and reattaches via `Sessions::reattach` (`WallEffect::ReattachTerminal`) on `wall.attach`.
   - *Verified by*: `tools/wall-smoke-agents.sh` (13 live checks in ~22 s across scratch `probe` + `probe2`), `wall_scenarios` (`attach_agent_prefers_local_reattach_...`, `loose_agent_view_shoves_restores_side_switches_resizes_and_spawn_steps_past_card`), `term-session` `part2_extra_namespaces`, and `wall::agents` / `wall::panels::agent_view` unit & offscreen GPU PNG tests.

2. ~~**No visual close-hold progress ring on keyboard `⌘X` / `Ctrl+Alt+X` hold**~~ — **Closed**: `mount.rs` pins `bar_buttons` to the terminal carrying `TerminalFrame::close_hold_progress`, painting the clockwise `#e5534b` ring on its title bar `×` button (or a top-right fallback badge in solo fullscreen / when bars are hidden) without running a duplicate close timer (`bar_buttons::external_hold_progress_...`, `mount::keyboard_close_hold_progress_drives_ring_through_mount`, + live scratch screenshot).

3. **Live `termd` status pushes & clickable dismiss `×` on the red outage banner** — **Mostly closed** (core live UX done):
   - **Closed**: `term-session::Sessions::pump` now emits coalesced `News::Status(serde_json::Value)` from `DaemonEvent::Status`; `app.rs` feeds it live into `wall.on_termd_status` (repainting immediately when Status is open or when daemon state/terminal count/error count changes, while keeping closed-Status idle at 0 frames); `mount.rs` enables `status::show_notice` (`show_notice: true`) with clickable `×` dismiss (`wall.status.dismiss`) while `app.rs` suppresses the duplicate canvas banner (`draw_status_notice: false` in `describe.rs`, keeping vertical spacing for transient notices).
   - *Minor remaining hooks*: `Wall::on_termd_holder_waiting`, `on_termd_brain_failed`, `on_termd_socket_unreachable`, and `TermdStatus::set_bundled_version` are still unused until `termd-client` surfaces distinct holder-waiting / brain-failed / bundled-version signals.

4. ~~**Floating selection action bar (`group (N)` + `×`) is missing**~~ — **Closed**: `native/src/wall/panels/selection_bar.rs` mounted in `mount.rs` paints the floating `group (N)` + `×` bar beneath the selected terminals' bounding box (hidden during lasso drags and fullscreen, emitting `wall.group` / `wall.select none`, matching `SelectionOverlay.js:39-50, 135-155`; unit-tested + live scratch click check).

5. ~~**`boldIsBright` setting has no effect on live terminals**~~ — **Closed**: `Sessions::set_bold_is_bright` propagates through `SessionHub`, `SessionInbox`, and each worker's `glyph_vt::Vt::set_theme` with immediate re-fill + repaint (and inherits across new spawns/reconnects); wired in `app.rs` on connect, browser import, and `ConfigKnobChanged` (`session_live::set_bold_is_bright_updates_live_and_new_sessions` + live pixel check).

6. ~~**IME preedit text is not drawn inline on the terminal canvas**~~ — **Closed**: `App` now stores the active composition string (`ime_preedit` / `script_ime_preedit`, preserving Wayland cursor-rect commit safety) and draws an underlined inline composition pill at the focused terminal's cursor cell via `chrome::push_ime_preedit`, clearing immediately on commit/disable/blur (verified with `chrome` unit test and live `app.ime preedit "你好"` screenshot).

7. ~~**Closed terminal GPU buffers are never freed from `WallRenderer` until exit**~~ — **Closed**: `App::pump` now releases closed terminal GPU buffers via `RenderIds::remove_from` (`renderer.remove_terminal`) on `News::Ended` and `RenderIds::retain_live` (`renderer.retain_terminals`) on `take_reconnected`, using monotonic non-colliding renderer ids (`render_id_tests::render_ids_free_closed_and_reconnected_terminals_in_renderer` + live `app.where` `cached_terminals` verification).

8. ~~**Right-click on map labels & fullscreen background, and fullscreen pinch guard**~~ — **Closed**: `Wall::context_menu` now resolves solo fullscreen to the fullscreen terminal, group fullscreen empty padding to the group, and zoomed-out map label pills (`hit_map_label`) to `MenuTarget::Terminal { part: Title }` (`WallMenu.js:61-75`); `Wall::pinch` returns early while fullscreen is active (`wall_scenarios::context_menu_hits_map_labels_and_fullscreen_padding_and_pinch_respects_fullscreen` + live scratch check).

9. ~~**Other dead or split-source settings (`renderLoop`, `traceInput`, `menuHint`, `clickRepeatMs`, static `[wall]` knobs)**~~ — **Closed**: `panelOpacity`, `greekPx`, and `cursorBlinkMs` are now live `WallConfig`/`KNOBS` settings synced into `WallRenderer::style_mut()` (with `greekPx` surviving resize/DPI changes via `WindowLifecycle::set_base_greek_px`); `app.rs` reads `wall.config().click_repeat_ms` for title-bar/canvas double-click instead of hardcoded `DOUBLE_CLICK_MS`; `renderLoop`, `traceInput`, and `menuHint` are hidden from the native Settings panel while remaining in `WallConfig`/`WallStore` so browser imports round-trip cleanly.

10. **Minor polish & unhandled window/session hooks**
    - ~~`WindowEvent::CursorLeft` is unhandled~~ — **Closed**: `Wall::pointer_leave()` clears passive terminal, group-header, map-label, and hyperlink hover highlights (and restores `CursorIcon::Default` without interrupting an active drag), wired from `WindowEvent::CursorLeft` in `app.rs` (`wall_scenarios::pointer_leave_clears_hover_and_link_without_canceling_active_drag`).
    - `sessions.take_bell()` (no bell flash/sound) and `sessions.pwd()` (OSC 7) are unused by `app.rs` (matches JS for bell; `pwd` is owned by the Agent View integration).
    - `VtEvent::ClipboardWrite` (OSC 52 from programs) is parsed by `glyph-vt` (`crates/glyph-vt/src/lib.rs:43`) but not forwarded through `term-session` to `app.rs` — confirmed JS `TerminalEmulator.js` and `TerminalWall.js` also do not handle or forward OSC 52.
    - ~~`OSC 8` hyperlink spans~~ — **Closed**: `term-session::sync_screen_into` copies `Screen::hyperlinks`, `Screen::link_spans`, and `cursor_color` (bumping `generation`), and `wall-core`'s `impl ScreenView for Screen` forwards `hyperlink_at` / `hyperlink_span_at` to `Screen::hyperlink_at` / `hyperlink_range_at`; verified live with `printf` OSC 8 + `⌘`-hover screenshot underline.
    - ~~No live smoke script exercises egui panels~~ — **Closed** (landed on `wall` as `tools/wall-smoke-panels.sh`, 12 checks).

### Deliberate Native Differences (Not Gaps)
- **No Go relay / `wall.token` / relay chip**: Native connects directly to `g3d-termd`'s Unix socket (`crates/termd-client`), so `wall.token` returns an explicit refusal (`wall.rs:3651`), `relay_port` is `None` (`mount.rs:741`), and the topbar omits the `● relay` chip (replacing it with a namespace badge + explicit `status` button).
- **No browser native context menu / `menuHint` footer / Firefox `noMenuBar` hack**: Native owns the whole window, so `context_menu.menu_hint` is `false` (`mount.rs:525`) and no browser-menu fallback exists.
- **Platform-strict paste chord**: `is_paste_chord` accepts `⌘V` on macOS and `Ctrl+Shift+V` on Linux/Other (`chord.rs:210`), avoiding stealing `Ctrl+Shift+V` on macOS or `Super+V` on Linux (JS `keyEncoding.js` accepted both).
- **Free-form system `fontFamily` + live `fontSize` & `lineHeight` knobs + panel-preserving re-wrap**: JS only offered 3 bundled families (`monaspace-neon | jetbrains-mono | fira-code`) and no `fontSize`/`lineHeight` knobs; native loads any installed monospace font with live size/leading and keeps world panel footprint constant (`set_geometry_preserving_panels`, also rescaling saved fullscreen canvas grids).
- **External URL hover underline & open (`⌘-click` / `Ctrl-click`)**: Absent in JS wall; added in native (`links.rs`, `WallEffect::OpenUrl`, `http`/`https`/`mailto` only).
- **Scrolled-back indicator pill (`↑N`) + scrollbar thumb**: Added in native (`describe.rs:178`, `chrome.rs:1284`); JS had no per-terminal scrollback indicator.
- **WGSL shader hot-reload (`GLYPH_WALL_SHADER_DIR`), perf probe (`WallPerf` / `app.perf`), and paper-first async startup (`f5f5c35`)**: Native-only engineering and startup optimizations (immediate frame-1 paper paint with off-critical-path `g3d-termd` connect, fallback fonts, emoji archive, and shapes/cells pipelines).

---

## 2. Behaviour-by-Behaviour Parity Table (Most-Used First)

### 2.1 Core Terminal Lifecycle, Typing, IME & Clipboard

| Behaviour | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| Connect to `g3d-termd`, adopt existing terminals & reconcile store | `TerminalWall.js`, `WallStore.js` | `wall.rs` `set_connected`, `set_live_terminals`, `terminal_appeared(adopted: true)` | `app.rs` startup + `pump` (`Sessions::connect`, `take_reconnected`, `take_external_appeared`) | `wall_scenarios::state_survives_restart_through_store`; `wall-smoke.sh` boot | **Ported & live** |
| Spawn new terminal (flow placement / at world `(x, y)` / focus & frame) | `TerminalWall.js` `spawn`, `wallLayout.js` `placeNext` | `model.rs` `spawn_terminal`, `layout.rs::place_next`, `commands.rs` `wall.spawn` | `app.rs:860` `WallEffect::SpawnTerminal` -> `sessions.spawn` | `wall_scenarios::spawn_three_terminals_flow_into_place`; `wall-smoke.sh` #1 | **Ported & live** |
| Close terminal (kill in `termd` & forget store state) | `TerminalWall.js` `close`, `WallStore.js` `forget` | `model.rs` `close_terminal`, `store.rs::forget`, `commands.rs` `wall.close` | `app.rs` `WallEffect::KillTerminal` -> `sessions.kill`; `News::Ended` & reconnect free GPU buffers (`remove_terminal` / `retain_terminals`) | `wall_scenarios::hide_show_close_and_hold_close_chord`; `app::render_id_tests` | **Ported & live** |
| Forward keys (Press / Repeat / Release) to focused terminal with Kitty modes | `WallKeys.js`, `keyEncoding.js` `keyEventFor` | `keymap.rs` `decide_key`, `terminal_key_for`, `PressedKeys`; `wall.rs:4985` | `app.rs:874, 1449-1528`, `keys.rs` `winit_key_event` | `wall_scenarios::keys_and_verbs_drive_wall_and_forward_to_focused_terminal`; `wall-smoke.sh` #3, #15, #17, #18 | **Ported & live** |
| Route key-release to the terminal that received its press (even if focus moved) | `WallKeys.js` `_down` map | `keymap.rs::PressedKeys` (L1314) | `app.rs:1449` -> `wall.key` | `wall_scenarios::wall_blur_clears_pressed_keys_cancels_close_hold_and_cancels_active_drag` | **Ported & live** |
| `Option as Alt` (`auto` / `true` / `false`) on macOS & other platforms | `keyEncoding.js` `OPTION_AS_ALT`, `WallKeys.js` | `config.rs::OptionAsAlt`, `keymap.rs::terminal_key_for` | `app.rs:654-679, 836, 1519`, `keys.rs:118-152` (winit macOS `OptionAsAlt`) | `wall-smoke.sh` #16 (exact PTY hex bytes for `auto`/`true`/`false`) | **Ported & live** |
| Dead keys & IME composition (`Preedit` inline preview + suppression + UTF-8 `Commit`) | `keyEncoding.js` `NOTHING_YET`, browser IME | `keymap.rs::is_nothing_yet_key` (L836) | `app.rs` (`set_ime_cursor_area`, `ime_preedit` -> `chrome::push_ime_preedit`) | `wall-smoke.sh` #15 (dead `Option+E`, preedit `´`, commit `é` & `你好`) + live `app.ime preedit "你好"` shot | **Ported & live** |
| Window blur / focus loss (clear pressed keys, cancel close-hold & drag) | `WallKeys.js` / `WallPointer.js` blur | `wall.rs:4805` `Wall::blur()` (`PressedKeys::clear`, `cancel_close_hold`, `pointer_cancel`, clear `hovered_link`) | `app.rs:1581-1598` `App::focused(false)` calls `wall.blur()` + resets window mods/IME/buttons | `wall_scenarios::wall_blur_clears_pressed_keys_cancels_close_hold_and_cancels_active_drag`; `wall-smoke.sh` #20 | **Ported & live** |
| Copy selected text (`⌘C` / `Ctrl+Shift+C`, `wall.copy`) — falls through to shell when no selection | `WallKeys.js:140`, `chord.js` `isCopyChord` | `chord.rs:186`, `keymap.rs:1256`, `commands.rs` `wall.copy` | `app.rs:800-820` `WallEffect::WriteClipboard` (Wayland/egui, `arboard`, or memory) | `wall_scenarios::text_selection_drag_double_triple_click_and_copy_paste`; `wall-smoke.sh` #9; `wall-smoke-text.sh` #1, #3 | **Ported & live** |
| Paste system clipboard (`⌘V` / `Ctrl+Shift+V`, `wall.paste`) with bracketed paste | `WallKeys.js:172`, `keyEncoding.js` `isPasteChord` | `chord.rs:210`, `wall.rs:5116`, `WallEffect::ReadClipboard` / `PasteTerminal` | `app.rs:783-798, 822-829, 875` -> `sessions.paste` | `wall_scenarios::text_selection_drag_double_triple_click_and_copy_paste`; `wall-smoke.sh` #18 | **Ported & live** (deliberately platform-strict chord) |
| Middle-click paste from wall selection clipboard (`wall.paste <id> selection`) | `WallPointer.js`, `TerminalWall.js` `copyOnSelect` | `pointer.rs` `Pan { paste_into }` / `FsPaste`; `model.rs:2917` | `app.rs:875` -> `sessions.paste` | `pointer::middle_click_on_body_pastes_selection_clipboard_and_middle_drag_pans` | **Ported & live** (no X11/Wayland primary selection export, matches browser limitation) |
| Terminal selection lifetime across VT output, scroll, rewrite & resize | `terminalSelection.js`, `TerminalGrid.js` | `selection.rs::sync_after_fill`, `wall.rs::terminal_screen_filled` | `crates/term-session` worker `FillDelta` accumulation -> `News::Filled` -> `app.rs:1032` | `wall_scenarios::selection_lifetime_across_vt_fill_scroll_rewrite_and_resize`; `wall-smoke-text.sh` #1–#6 | **Ported & live** |
| Typing clears terminal selection (`selectionClearOnTyping`) | `WallKeys.js:169` | `keymap.rs:1306`, `wall.rs:3205, 5159` | `app.rs:1449` -> `wall.key` | `keymap::selection_clear_on_typing_off_...`; `wall-smoke-text.sh` #6 | **Ported & live** |
| Live terminal title (`OSC 0`/`2`) & last-line preview for Palette | `TerminalEmulator.js` `onTitle`, `TerminalWall.js` | `wall.rs::terminal_title`, `terminal_preview`, `terminal_screen_filled` | `term-session` `News::Title` + `News::Filled` -> `app.rs:1032-1047` | `wall_scenarios::screen_fill_updates_preview_and_adopted_title_only_when_changed` | **Ported & live** |
| Terminal bell (`BEL`) & working directory (`OSC 7` `pwd`) | `TerminalEmulator.js` (no UI bell in JS) | — | `term-session::Session::take_bell` / `pwd` exist; **unused** in `app.rs` | `session_live` BEL test | **Unused** (matches JS for bell; `pwd` needed by `AgentViews` session matching) |
| Program clipboard writes (`OSC 52`) | Not wired in JS `TerminalEmulator.js` | — | `glyph-vt` emits `VtEvent::ClipboardWrite`; not forwarded by `term-session` | `glyph-vt` unit tests | **Not wired** (same as JS) |

### 2.2 Pointer Gestures, Mouse Reporting, Wheel/Pinch & Links

| Behaviour | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| Click terminal body: focus & raise to front (clear selection) | `WallPointer.js` | `pointer.rs` `Text`, `model.rs` `focus` | `app.rs:1787-1836` | `wall_scenarios::click_focuses_and_raises`; `wall-smoke.sh` #2 | **Ported & live** |
| Click empty canvas: blur focus & clear wall selection | `WallPointer.js` | `pointer.rs` `Pan` click | `app.rs:1787-1836` | `wall_scenarios::click_focuses_and_raises`; `wall-smoke.sh` #14 | **Ported & live** |
| Drag empty canvas (left or middle): pan camera | `WallPointer.js` | `pointer.rs` `Pan` -> `camera_move_to` | `app.rs:1787-1836` | `pointer::middle_click_on_body_pastes_selection_clipboard_and_middle_drag_pans` | **Ported & live** |
| Drag terminal body: cell text selection; double-click = word; triple-click = line | `WallPointer.js`, `terminalSelection.js` | `pointer.rs::start_text` (L2327), `selection.rs` | `app.rs:1198-1210` (`CellSelection`), `1787` | `pointer::presses_within_click_repeat_ms_...`; `wall-smoke.sh` #9; `wall-smoke-text.sh` #1 | **Ported & live** |
| Rectangular block text selection (`⌘⌥-drag` Mac / `Ctrl+Alt-drag` Other) | `WallPointer.js`, `chord.js` `blockSelects` | `chord.rs:295`, `pointer.rs` `Text { block: true }`, `selection.rs` | `app.rs:1198-1210` | `pointer::block_modifiers_select_rectangular_block_and_alt_drag_still_moves_terminal` | **Ported & live** |
| Drag title bar or `Alt`-drag terminal: move with grid snap + live `Ctrl` snap-invert | `WallPointer.js`, `wallSnap.js` | `pointer.rs` `Press`, `on_key` (L2151), `snap.rs::snap_move` | `app.rs:1787-1836`, `1905` (`ModifiersChanged`) | `wall_scenarios::title_bar_drag_moves_with_snapping_and_ctrl_inverts`; `pointer::snap_*`; `wall-smoke.sh` #4 | **Ported & live** |
| Drag terminal border / corner: resize in whole cells (`min 20×5`) with grid snap | `WallPointer.js`, `wallLayout.js` `resizeFromDrag`, `wallSnap.js` | `pointer.rs` `Resize`, `layout.rs::resize_from_drag`, `snap.rs::snap_resize` | `app.rs:873` (`sessions.resize`) | `wall_scenarios::border_drag_resizes_in_whole_cells_and_requests_resize`; `pointer::border_drag_*`; `wall-smoke.sh` #5 | **Ported & live** |
| Double-click title bar (frame terminal), group header (frame group), empty canvas (fit all), body (no-op) | `WallPointer.js` `_dblClick` | `pointer.rs::on_double_click` (L2123) | `app.rs:1825` calls `wall.pointer_double_click` | `wall_scenarios::double_click_frames_pane_focus_...`; `pointer::double_click_on_body_...`; `wall-smoke.sh` #11 | **Minor diff**: `app.rs:66` hardcodes `DOUBLE_CLICK_MS = 500.0` instead of reading `config.click_repeat_ms` |
| `Shift+click` terminal: toggle wall selection; `Shift+drag` canvas/title: lasso select; `Shift+drag` body: force text select | `WallPointer.js`, `wallLayout.js` `lassoTouches` | `pointer.rs` `Shift` / `Lasso`, `layout.rs::lasso_touches` | `app.rs:1787`, `describe.rs:201-227` | `wall_scenarios::lasso_selects_and_shift_click_toggles`; `pointer::shift_drag_*`; `wall-smoke.sh` #10, #14 | **Ported & live** |
| DECSET mouse reporting (`x10`/`normal`/`button`/`any`) on focused body + `Shift` bypass | `WallMouse.js`, `WallPointer.js` | `mouse.rs`, `pointer.rs::ProgramMouse` (L780) | `app.rs:876` (`sessions.mouse`), `1053` (`News::Modes`) | `wall_scenarios::mouse_reporting_program_...`; `pointer` #15–#23 | **Ported & live** |
| Drag group header (move group, snap top-left); click header (raise & select members) | `WallPointer.js`, `WallGroups.js` | `pointer.rs` `Group`, `groups.rs` | `app.rs:1787`, `describe.rs:135` | `wall_scenarios::chord_g_groups_tiled_and_header_drag_moves_group`; `pointer::group_header_drag_*` | **Ported & live** |
| Drag tiled member (reorder with drop preview, never snaps); drag split pane title (swap/dock preview) | `WallPointer.js`, `WallGroups.js`, `splitTree.js` | `pointer.rs` `Press` (tile) / `Pane` (split), `groups.rs::tile_drop` / `pane_drop` | `describe.rs:230` (`DropPreviewDesc`) | `wall_scenarios::pane_swap_and_dock_by_title_bar_drag`; `pointer::tiled_member_drag_never_snaps`, #24 | **Ported & live** |
| Drag split divider (canvas & Group Fullscreen) and outer split border | `WallPointer.js`, `WallGroups.js` | `pointer.rs` `Divider` / `SplitResize`, `split.rs` | `app.rs:1787` | `wall_scenarios::divider_drag_resizes_split_panes`; `pointer` #24 | **Ported & live** |
| Click / drag / right-click zoomed-out map label pill (click focuses & frames; drag moves; right-click opens terminal menu) | `MapLabels.js`, `WallPointer.js` `pressLabel`, `WallMenu.js:74` | `wall.rs` (`hit_map_label` -> `press_label` & `context_menu`) | `app.rs` (`wall.pointer_down`, `wall.context_menu`), `describe.rs` | `wall_scenarios::zoom_out_hands_off_to_map_labels_and_press_label_frames`, `context_menu_hits_map_labels_...`; `pointer` #24 | **Ported & live** |
| Wheel: pan empty canvas; zoom at pointer with `zoomKeys`; scroll focused terminal (trackpad 1:1 + notch glide + quench gate + mid-stream mod guard) | `WallPointer.js`, `terminalScroll.js`, `WallCamera.js` | `pointer.rs::on_wheel`, `scroll.rs` (`WheelStreamGate`, `TerminalScroller`), `camera.rs` | `app.rs:878-879, 1432-1447` | `wall_scenarios::wheel_*`; `pointer` #6, #7; `wall-smoke.sh` #6, #7, #8 | **Ported & live** |
| Trackpad pinch zoom (`PinchGesture`) | Browser `ctrlKey` wheel pinch | `wall.rs` `Wall::pinch` (fullscreen-guarded) | `app.rs` | `wall_scenarios::context_menu_hits_map_labels_...`; `wall-smoke.sh` #11 | **Ported & live** |
| Pointer cursor icons (13 shapes: `Default`, `Grab`, `Grabbing`, `Text`, `Pointer`, `EwResize`, `NsResize`, `NeswResize`, `NwseResize`, `ColResize`, `RowResize`, `Crosshair`, `Move`) | `WallPointer.js`, `wallLayout.js` | `pointer.rs::CursorIcon`, `wall.rs::sync_effective_cursor` | `app.rs:270-287, 852-856` | `wall_scenarios::link_hover_underline_cursor_and_open_url_on_mac_and_linux` | **Ported & live** |
| Hyperlink hover underline, `Pointer` cursor & external open (`⌘-click` Mac / `Ctrl-click` Other; `http`/`https`/`mailto` only) | *Not in JS wall* | `links.rs` (`ScreenView`, `link_at_cell`, `is_allowed_url_scheme`), `wall.rs`, `model.rs::WallEffect::OpenUrl` | `app.rs:366-385, 830, 1211-1218, 1905` (`open` / `xdg-open`, `CellLinkHover`), `term-session::sync_screen_into` | `wall_scenarios::link_hover_underline_cursor_and_open_url_on_mac_and_linux`, `osc8_screen_view_link_...`, `links::screen_impl_forwards_osc8_hyperlinks_and_soft_wrapped_ranges`, live OSC 8 hover PNG | **Native addition** — plain URLs and `OSC 8` hyperlinks both live |

### 2.3 Keyboard Chords, Focus Navigation & Plain Keys

Chord modifier: `⌘` (`Meta`) on macOS; `Ctrl+Alt` (without `Meta`, excluding `AltGraph`) on Linux/Other (`chord.rs:88`). All 31 `KEYMAP` chords match on physical `code`:

| Behaviour | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| `Chord+]` / `Chord+[` — cycle focus next/prev in reading order (works in panel inputs too) | `wallKeymap.js`, `WallKeys.js:118` | `keymap.rs` `KEYMAP` #1–#2, `decide_key` | `app.rs:291, 1449` | `wall_scenarios::keys_and_verbs_...`; `wall-smoke.sh` #19 | **Ported & live** |
| `Ctrl+,` / `Ctrl+.` — focus history back / forward | `chord.js` `focusHistoryDir`, `WallKeys.js:128` | `chord.rs:244`, `keymap.rs` | `app.rs:1449` | `wall-smoke.sh` #19 | **Ported & live** |
| `Ctrl+Shift+[` / `Ctrl+Shift+]` — cycle focus across groups (works in panel inputs too) | `chord.js` `groupCycleDir`, `WallKeys.js:111, 134` | `chord.rs:270`, `keymap.rs` | `app.rs:291, 1449` | `keymap.rs` unit tests | **Ported & live** |
| `Chord+Enter` — fullscreen focused group/terminal (or fit all); toggle back to canvas | `wallKeymap.js` | `keymap.rs` `KEYMAP` #3 | `app.rs:1449` | `wall_scenarios::fullscreen_single_group_pane_zoom_text_scale_and_exit`; `wall-smoke.sh` #19 | **Ported & live** |
| `Chord+Shift+Enter` — solo-fullscreen focused pane (zoom out of Group Fullscreen & back) | `wallKeymap.js` | `keymap.rs` `KEYMAP` #4 | `app.rs:1449` | `wall_scenarios::fullscreen_single_group_pane_zoom_text_scale_and_exit` | **Ported & live** |
| `Chord+=` / `Chord+-` / `Chord+0` (in fullscreen) — bigger / smaller / reset fullscreen text (re-wraps, remembered) | `wallKeymap.js`, `WallFullscreen.js` | `keymap.rs` `KEYMAP` #5–#7, `fullscreen.rs` | `app.rs:873, 1449` | `wall_scenarios::fullscreen_single_group_pane_zoom_text_scale_and_exit` | **Ported & live** |
| `Chord+\` — spawn new terminal | `wallKeymap.js` | `keymap.rs` `KEYMAP` #8 | `app.rs:1449` | `wall-smoke.sh` #1 | **Ported & live** |
| `Chord+A` (canvas) — toggle focused terminal's Agent View | `wallKeymap.js` | `keymap.rs` `KEYMAP` #9 -> `wall.agent <id> toggle` | `app.rs` -> `WallAgents` + `AgentViewPanel` | `wall-smoke-agents.sh` #2, #3 | **Ported & live** |
| `Chord+-` (canvas) — hide focused terminal (keeps running) | `wallKeymap.js` | `keymap.rs` `KEYMAP` #10 | `app.rs:1449` | `wall_scenarios::hide_show_close_and_hold_close_chord` | **Ported & live** |
| `Chord+X` (hold `closeHoldMs`) — kill focused terminal; release early or blur cancels | `wallKeymap.js`, `WallKeys.js:93`, `BarButtons.js:112` | `keymap.rs` `KEYMAP` #11, `wall.rs` `close_hold_progress` | `app.rs`, `mount.rs`, `bar_buttons.rs` (`external_hold_progress` + fallback badge) | `wall_scenarios::hide_show_close_and_hold_close_chord`, `wall_blur_...`, `mount::keyboard_close_hold_progress_...`; `wall-smoke.sh` #20 + live ring shot | **Ported & live** |
| `Chord+K` — toggle command palette | `wallKeymap.js`, `main.jsx:90` | `keymap.rs` `KEYMAP` #12 -> `KeyDecision::Palette`, `wall.rs:5098` | `app.rs:1449`, `mount.rs:326` | `wall_scenarios::verbs_palette_...`; `wall-smoke.sh` #19 | **Ported & live** |
| `Chord+D` / `Chord+Shift+D` — split right / split down | `wallKeymap.js` | `keymap.rs` `KEYMAP` #13–#14 | `app.rs:1449` | `wall_scenarios::chord_d_splits_...`; `wall-smoke.sh` #12 | **Ported & live** |
| `Chord+←/→/↑/↓` — focus adjacent pane or nearest window | `wallKeymap.js` | `keymap.rs` `KEYMAP` #15–#18 | `app.rs:1449` | `wall_scenarios::double_click_frames_pane_focus_...` | **Ported & live** |
| `Chord+Shift+←/→/↑/↓` — move pane/tile within its group | `wallKeymap.js` | `keymap.rs` `KEYMAP` #19–#22 | `app.rs:1449` | `commands.rs` / `groups.rs` unit tests | **Ported & live** |
| `Chord+G` / `Chord+Shift+G` — group selection (or focused) / ungroup focused group | `wallKeymap.js` | `keymap.rs` `KEYMAP` #23–#24 | `app.rs:1449` | `wall_scenarios::chord_g_groups_tiled_...`; `wall-smoke.sh` #10 | **Ported & live** |
| `Chord+0` (canvas) — fit every shown terminal in view | `wallKeymap.js` | `keymap.rs` `KEYMAP` #25 | `app.rs:1449` | `wall_scenarios::fit_and_frame_...` | **Ported & live** |
| `Chord+'` / `Chord+Shift+'` — toggle snap / toggle drafting grid | `wallKeymap.js` | `keymap.rs` `KEYMAP` #26–#27 | `app.rs:1449`, `describe.rs:311` | `keymap.rs` unit tests | **Ported & live** |
| `Chord+B` / `Chord+S` / `Chord+,` / `Chord+/` — toggle Sidebar, Status, Settings, Help | `wallKeymap.js` | `keymap.rs` `KEYMAP` #28–#31 | `app.rs:1449`, `mount.rs` | `wall-smoke.sh` #19 | **Ported & live** |
| `Esc` (unfocused) — clear wall selection; `Esc` (menu/palette open) — close menu/palette; `Esc` (focused terminal) — sent to program | `WallKeys.js:160`, `ContextMenu.jsx`, `Palette.jsx` | `keymap.rs:1289`, `wall.rs:4995` | `app.rs:1469-1485` | `wall-smoke.sh` #14, #18, #19 | **Ported & live** |
| `Menu` / `Shift+F10` — open context menu on focused terminal (else canvas centre) | `WallKeys.js:145`, `WallMenu.js:116` | `keymap.rs:1172`, `wall.rs:5090` | `app.rs:1449`, `mount.rs:514` | `wall-smoke.sh` #19 | **Ported & live** |
| `Cmd+Q` / `Ctrl+Alt+Q` quit; `Chord+F1` debug HUD | Browser close tab / devtools | — | `app.rs:1453-1467` | Manual | **Native addition** |

### 2.4 Groups, Splits & Fullscreen

| Behaviour | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| Three group layouts: `tile` (configurable columns & gap), `freeform`, `split` (binary tree with ratios) | `WallGroups.js`, `splitTree.js` | `groups.rs`, `split.rs`, `model.rs` | `describe.rs`, `app.rs` | `wall_scenarios` #9–#12; `groups.rs` / `split.rs` unit tests | **Ported & live** |
| Split pane title bars toggle (`splitBars` / `wall.group.bars`) & `splitBarRows` height | `WallGroups.js`, `wallConfig.js` | `groups.rs`, `model.rs`, `wall.rs` | `describe.rs:153` | `wall_scenarios` #10–#12 | **Ported & live** |
| Solo fullscreen: resize terminal to viewport, remember canvas grid & camera, refit on window resize, restore on exit | `WallFullscreen.js` | `fullscreen.rs` (`enter_single`, `refit`, `exit`, `rescale_saved_canvas_for_geometry`) | `app.rs:873, 1112`, `describe.rs:244` | `wall_scenarios` #13, #14 (`fullscreen_saved_canvas_grid_rescales_on_font_geometry_change`); `wall-smoke.sh` #19 | **Ported & live** |
| Group fullscreen (tile, freeform, split) + pane zoom-out (`wall.fullscreen.pane`) & return to group | `WallFullscreen.js` | `fullscreen.rs` (`enter_group`, `enter_pane`, `exit`) | `app.rs`, `describe.rs:244` | `wall_scenarios` #13, #29 | **Ported & live** |
| Fullscreen text scaling (`fullscreenTextStep`, `fullscreenMinLinePx`, `fullscreenPad`), remembered per terminal or `group:<gid>` | `WallFullscreen.js`, `WallStore.js` `fullscreenText` | `fullscreen.rs`, `store.rs::fullscreen_text` | `app.rs:873` | `wall_scenarios` #13 | **Ported & live** |
| Dock-aware fit & frame (shrinks and shifts clear of left/right docked panels) | `dockLayout.js`, `TerminalWall.js` | `dock.rs`, `wall.rs` (`visible_canvas_rect`) | `mount.rs`, `app.rs` | `wall_scenarios::fit_and_frame_shrink_and_shift_clear_of_docked_panels` | **Ported & live** |

### 2.5 Canvas Chrome, Visuals, Rendering & Animations

| Behaviour | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| Drafting-paper grid (minor/major discrete levels, `f64` camera-modulo origin, Retina `linePx`, sRGB colors) | `WallPaper.js`, `wallSnap.js` `gridLevels` | `snap.rs::grid_levels`, `wall.rs` `WallFrame::paper` | `describe.rs:311`, `paper.rs`, `wall_paper.wgsl` | `wall::describe` tests, `paper.rs` tests | **Ported & live** |
| Terminal panels, borders (focused green `#6ee7a0`, hovered unfocused slate, idle slate) | `TerminalGrid.js` | `wall.rs` `TerminalFrame::border` | `wall_cells.wgsl:288-345`, `app.rs:1195` | `renderer.rs` pixel tests | **Ported & live** (`panel_opacity` static in `defaults.toml`) |
| Title bars (height-capped, auto-shrinking text, handoff to map labels, reserve room for bar buttons) | `TitleBar.js`, `WallNames.js` | `names.rs`, `wall.rs` `TitleBarFrame` + `set_char_widths` | `describe.rs:153`, `chrome.rs:373`, `app.rs:758` | `wall_scenarios` #27, #31; `wall::describe` tests | **Ported & live** |
| 3-span agent status in title bar & map label (`<name> · <status>` colored by status) | `TitleBar.js`, `MapLabels.js`, `WallBus.js` | `wall.rs` `attached_agent` & `agent_status` on `TitleBarFrame` / `MapLabelFrame` (+ `gx-<name>` fallback) | `describe.rs:153, 258`, `chrome.rs:373, 949`, `agents.rs` `set_bus_snapshot` | `wall_scenarios` #27, #38; `wall-smoke-agents.sh` #2, #13 | **Ported & live** |
| Group frames (padded backdrop + elevated header with idle/focused/hovered tint) | `GroupFrame.js` | `wall.rs` `GroupFrame` (`hovered`, `focused`) | `describe.rs:135`, `chrome.rs:649` | `wall_scenarios` #28; `describe.rs` test | **Ported & live** |
| Multi-terminal selection outlines (`3px` outside `#8ab4f8`), floating `"group (N)"` + `×` bar, lasso polygon, drop preview (solid blue / dashed coral) | `SelectionOverlay.js` | `wall.rs` `selected`, `lasso`, `drop_preview` | `describe.rs:201-241`, `chrome.rs:766-878`, `panels/selection_bar.rs`, `mount.rs` | `wall_scenarios` #8, #12; `selection_bar` unit test + live click check | **Ported & live** |
| Fullscreen veil with exact disjoint cutouts (single, split, tile, freeform) & opacity (`0.90`) | `WallFullscreen.js` `_cutoutBoxes` | `wall.rs` `FullscreenBackdrop { cutouts, opacity }` | `describe.rs:244`, `chrome.rs:1434` (sweep-line) | `wall_scenarios` #29; `describe.rs` tests | **Ported & live** |
| Zoomed-out map labels (`12px` screen-space pills + leader lines + anchor dots, collision-avoiding placement) | `MapLabels.js`, `labelPlacement.js`, `WallNames.js` | `labels.rs::place_labels`, `names.rs`, `wall.rs` `MapLabelFrame` (`hovered`, `char_widths`) | `describe.rs:258`, `chrome.rs:949` | `wall_scenarios` #7, #31, #32 | **Ported & live** |
| Close-hold progress indicator (`×` ring over `closeHoldMs`) | `holdPress.js`, `BarButtons.js`, `Sidebar.jsx`, `ContextMenu.jsx` | `wall.rs` `close_hold_progress` | Pointer hold: `bar_buttons.rs`, `sidebar.rs`, `context_menu.rs`; Key hold (`⌘X`): `mount.rs` -> `bar_buttons.rs` (`external_hold_progress` + fullscreen fallback badge) | `bar_buttons.rs`, `sidebar.rs`, `mount.rs` unit tests + live ring shot | **Ported & live** |
| Scrolled-back pill (`↑N`) & right-edge proportional scrollbar thumb | *Not in JS wall* | — | `describe.rs:178`, `chrome.rs:1284`, `app.rs:1221` | `chrome.rs` tests | **Native addition** |
| Slug quadratic-Bezier glyphs + 4 font faces + cell sprites + SGR decorations (single/double/curly/dotted/dashed underline, SGR 58 color, strike, overline, dim) | `TerminalGrid.js`, `glyphMaterial.js`, `fonts/families.js` | `config.rs::LINE_STYLE_KNOBS` | `renderer.rs`, `glyphs.rs`, `wall_cells.wgsl:639-783` | `renderer.rs` pixel tests | **Ported & live** |
| Wide chars (CJK / emoji 2-col span, selection/cursor/link snapping, right-edge clamp) & color emoji (`ZWJ`/flag/skin-tone/VS16) | `TerminalEmulator.js`, `TerminalGrid.js` | `links.rs`, `selection.rs` | `cell.rs:179`, `glyphs.rs:85`, `wall_cells.wgsl:417, 650` | `renderer.rs` & `cell.rs` tests | **Ported & live** |
| Cursor shapes (`Block`, `Bar`, `Underline`), focused block ink inversion, unfocused hollow outline, blink, `OSC 12` color | `TerminalGrid.js` | — | `renderer.rs:595-795, 1430, 1541`, `wall_cells.wgsl:796-865`, `app.rs:1243` | `renderer.rs` pixel tests | **Ported & live** (`cursorBlinkMs` static in `defaults.toml`) |
| Greeking (horizontal bars when cell height `< greekPx`) | `TerminalGrid.js`, `glyphMaterial.js` | — | `wall_cells.wgsl:648, 680`, `renderer.rs:59` (`greek_px = 8.0`) | `renderer.rs` tests | **Ported & live** (`greekPx` static in `defaults.toml`) |
| Camera animations: exponential framing ease (`EASE_RATE = 14`), log-zoom wheel glide (`settleRate = 24`), discrete terminal scroll glide (`GLIDE_SETTLE_RATE = 22`) | `WallCamera.js`, `terminalScroll.js` | `camera.rs`, `scroll.rs`, `wall.rs::tick` | `app.rs` `is_animating` -> `ControlFlow::Poll`/`WaitUntil` | `camera.rs`, `scroll.rs`, `wall_scenarios` | **Ported & live** |

### 2.6 Topbar & Panels (Including Every Context Menu Item)

| Panel / Control | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| **Topbar**: `+ terminal`, `fit all`, `find`, `list`, `status`, `settings`, `?`, `TermdChip`, hides in fullscreen, respects dock insets | `main.jsx:102-124` | `dock.rs`, `status.rs::chip_look` | `panels/topbar.rs`, `mount.rs:306` | `topbar.rs` unit tests; `wall::mount` tests | **Ported & live** (deliberately replaces `● relay` chip with namespace badge + `status` button) |
| **Title-bar hover buttons**: `≡` (Agent View), `–` (hide), `×` (hold-to-close ring); hides below `barButtonsMinPx` or mid-drag | `BarButtons.js` | `config.rs` (`barButtonsMinPx`, `closeHoldMs`) | `panels/bar_buttons.rs`, `mount.rs:253` | `bar_buttons.rs` unit tests; `wall-smoke-agents.sh` #3 | **Ported & live** (`hold_key` unused for `⌘X`, §1 #2) |
| **Sidebar**: Left/Right/Float + drag head; groups (collapse, frame, tile `−`/`+` cols, tile/free toggle, ungroup); terminals (indented, focused, hidden, reveal, `≡`, leave, hide/unhide, hold `×`); agents-by-host section | `Sidebar.jsx`, `Panel.jsx` | `sidebar.rs`, `store.rs::sidebar`, `bus.rs` | `panels/sidebar.rs`, `mount.rs:344`, `agents.rs` | `sidebar.rs` unit tests; `wall-smoke-agents.sh` #11 | **Ported & live** |
| **Command Palette**: `Find` / `Rename` (terminal & group) / `Token` modes; fuzzy rank; groups, terminals (+ preview), agents, context verbs; `↑`/`↓`/`Enter`/`Esc`/`⌘K` | `Palette.jsx` | `palette.rs`, `wall.rs::wall.palette`, `bus.rs` | `panels/palette.rs`, `mount.rs:326`, `agents.rs` | `palette.rs` unit tests; `wall-smoke-agents.sh` #11 | **Ported & live** (`Token` deliberately hidden via `relay_port: None`) |
| **Status Window & `TermdChip`**: Left/Right/Float + drag; daemon version/engine/ns/uptime/holder/brain/env/upgrade note; 6-col terminals table (click row -> `wall.reveal`); leftovers; capped errors; dismissible outage notice | `StatusWindow.jsx`, `Panel.jsx` | `status.rs`, `store.rs::status` | `panels/docked/status.rs`, `mount.rs:384` | `status.rs` unit tests; `wall-smoke.sh` #19 (toggle) | **Partial** — no live `DaemonEvent::Status` pushes, no `set_bundled_version`, `show_notice: false` disables clickable `×` dismiss (§1 #3) |
| **Settings Panel**: Left/Right/Float + drag; live search with amber hit highlights; collapsible sections; Bool/Enum/Color (swatch)/Text/Number controls; per-knob `↺` reset; inline refusal errors; keyboard nav | `SettingsPanel.jsx`, `wallSettings.js`, `Panel.jsx` | `settings.rs`, `config.rs`, `store.rs::settings` | `panels/docked/settings.rs`, `mount.rs:413` | `settings.rs` unit tests; `wall-smoke.sh` #19 (toggle) | **Ported & live** (4 knobs dead live, see §2.8) |
| **Help Panel**: Left/Right/Float + drag; live search; keys, gestures, and full verb reference from router | `HelpPanel.jsx`, `wallKeymap.js`, `Panel.jsx` | `keymap.rs::help_sections` / `search_help`, `store.rs::help` | `panels/menus/help.rs`, `mount.rs:495` | `help.rs` unit tests; `wall-smoke.sh` #19 (toggle) | **Ported & live** |
| **Context Menu — Policy & Chrome**: `Ours` / `Native` / `Program` policy; macOS `Ctrl+Left`; viewport flip (`place_menu`); keyboard nav (`↑↓←→ Home End Enter Space Tab Esc`); `Action`, `Hold` ring, inline `Rename`, `Choice` chips, inline custom `cols×rows` resize + error | `contextMenu.js`, `ContextMenu.jsx`, `WallMenu.js` | `menu.rs` (`context_policy`, `menu_items`, `place_menu`, `check_size`, `parse_sizes`) | `panels/menus/context_menu.rs`, `mount.rs:514`, `app.rs:1798` | `menu.rs` & `context_menu.rs` unit tests; `wall-smoke.sh` #13, #14, #19 | **Ported & live** (`menuHint` footer deliberately omitted; map-label / fs-bg right-click bug §1 #8) |
| **Context Menu — Terminal items (all 20)**: `copy` (if selected), `paste`, `exit fullscreen`/`frame`, `fullscreen pane` (group fs), `focus` (if unfocused), `rename…`, `agent view` (`off`/`right`/`left`/`top`/`bottom`), `select toggle`, `leave group` OR (`new group`/`group with selection (N)` + `add to` chips), `split right`, `split down`, `size` (`menuSizes` + `custom…`), `redraw`, `Option as Alt` (`auto`/`on`/`off`), `hide`, `close` (hold) | `contextMenu.js` `terminalItems` | `menu.rs::terminal_items` (L428–704) | `panels/menus/context_menu.rs` | `menu.rs` & `context_menu.rs` tests | **Ported & live** (all 20 items match 1:1) |
| **Context Menu — Group items (all 9)**: `exit fullscreen`/`frame`, `rename…`, `layout` (`tile`/`freeform`/`split`), `title bars` (`shown`/`hidden`, split), `columns` (`−`/`+`, tile), `select its terminals`, `hide all`, `ungroup` | `contextMenu.js` `groupItems` | `menu.rs::group_items` (L706–858) | `panels/menus/context_menu.rs` | `menu.rs` & `context_menu.rs` tests | **Ported & live** (all 9 items match 1:1) |
| **Context Menu — Canvas items (all 9)**: `new terminal here`, `fit all`, `select all`, `snap to grid` (`on`/`off`), `grid` (`show`/`hide`), `sidebar`, `status`, `settings` | `contextMenu.js` `canvasItems` | `menu.rs::canvas_items` (L860–977) | `panels/menus/context_menu.rs` | `menu.rs` & `context_menu.rs` tests | **Ported & live** (all 9 items match 1:1) |
| **Agent View Overlay**: world-anchored card beside terminal (`R`/`L`/`T`/`B`), far-zoom badge, drag-to-reposition ghost, session selector, category filter chips + tree drawer, event rows + expand/collapse + full body + load earlier + scrollback jump, follow-tail / unread badge, canvas shove & restore | `AgentViewOverlay.jsx`, `WallAgentViews.js`, `wallAgentView.js` | `agent_view.rs`, `agent_views.rs`, `wall.rs` (`AgentFootprintBridge`) | `panels/agent_view.rs`, `agents.rs`, `app.rs` | `wall-smoke-agents.sh` #1–#13; `wall_scenarios` #39; `agent_view` / `agents` unit & GPU PNG tests | **Ported & live** |
| **FitText** (shrink-to-fit text down to `minPx`, wrap below floor, never ellipsis) | `FitText.jsx` | — | Canvas `chrome.rs` shrinks labels; egui panels use `panels` helpers | `chrome.rs` & panel tests | **Ported** |

### 2.7 All Verbs (`wallCommands.js` — 54 JS + 3 Rust Additions + 7 `terminal.*` + 2 Built-ins)

All verbs are registered in `crates/wall-core/src/commands.rs:947-2511` (`register_wall_commands`), `wall.rs:3920-4124` (`register_terminal_commands`), and `router.rs:789-800` (`call`, `help`), and reached live via `wallctl` (`crates/wall-control`) + `app.rs::serve_control` + panels + `Wall::key`:

| Verb(s) | JS source | `wall-core` | Window / Live status |
|---|---|---|---|
| `wall.list`, `wall.hide`, `wall.show`, `wall.reveal`, `wall.spawn`, `wall.close`, `wall.rename`, `wall.fit` | `wallCommands.js` | `commands.rs` | **Ported & live** (tested in `wall_scenarios` & `wall-smoke.sh`) |
| `wall.focus` *(Rust addition)* | — | `commands.rs` | **Ported & live** (focus without framing; `wall_scenarios` #23) |
| `wall.fullscreen`, `wall.fullscreen.pane`, `wall.fullscreen.text` | `wallCommands.js` | `commands.rs`, `fullscreen.rs` | **Ported & live** (`wall_scenarios` #13, #14; `wall-smoke.sh` #19) |
| `wall.select`, `wall.select.add`, `wall.select.toggle` | `wallCommands.js` | `commands.rs` | **Ported & live** (`wall_scenarios` #8; `wall-smoke.sh` #10, #14) |
| `wall.copy`, `wall.paste` | `wallCommands.js` | `commands.rs`, `wall.rs` | **Ported & live** (`wall_scenarios` #16; `wall-smoke.sh` #9, #18; `wall-smoke-text.sh`) |
| `wall.group`, `wall.group.add`, `wall.group.remove`, `wall.ungroup`, `wall.group.layout`, `wall.group.columns`, `wall.group.bars`, `wall.group.gap`, `wall.group.rename`, `wall.group.move`, `wall.group.frame` | `wallCommands.js` | `commands.rs`, `groups.rs` | **Ported & live** (`wall_scenarios` #9–#12; `wall-smoke.sh` #10, #12) |
| `wall.split`, `wall.pane`, `wall.pane.move`, `wall.focus.history` | `wallCommands.js` | `commands.rs`, `split.rs`, `model.rs` | **Ported & live** (`wall_scenarios` #10, #21; `wall-smoke.sh` #12, #19) |
| `wall.sidebar`, `wall.sidebar.collapse`, `wall.status`, `wall.status.dismiss`, `wall.menu`, `wall.settings`, `wall.settings.collapse`, `wall.help` | `wallCommands.js` | `commands.rs`, `store.rs` | **Ported & live** (`wall-smoke.sh` #13, #14, #19; panel unit tests) |
| `wall.palette` *(Rust addition)* | — (DOM state in `main.jsx`) | `commands.rs` | **Ported & live** (`wall_scenarios` #23; `wall-smoke.sh` #19) |
| `wall.config`, `wall.config.find`, `wall.snap`, `wall.grid` | `wallCommands.js` | `commands.rs`, `config.rs`, `settings.rs` | **Ported & live** (4 knobs dead live, see §2.8) |
| `wall.bus`, `wall.attach` | `wallCommands.js`, `WallBus.js` | `commands.rs`, `bus.rs`, `wall.rs` | **Ported & live** (`FleetFollower` -> `set_bus_snapshot`; `ReattachTerminal` prefers local `gx-<name>` when `--wall-extra-ns agents` is connected; `wall-smoke-agents.sh` #11, #13) |
| `wall.agent`, `wall.agent.side`, `wall.agent.filter` | `wallCommands.js` | `commands.rs`, `wall.rs`, `agent_views.rs` | **Ported & live** (persists store, runs `AgentViews::footprint_changed` shove/restore, opens/filters streams; `wall-smoke-agents.sh` #2–#6, #12) |
| `wall.agent.follow` *(Rust addition)* | — | `commands.rs`, `wall.rs`, `agents.rs` | **Ported & live** (syncs `stick_bottom` & unread badge with `AgentViews` & `AgentViewPanel`; `wall-smoke-agents.sh` #9, #10) |
| `wall.agent.expand`, `wall.agent.collapse`, `wall.agent.show`, `wall.agent.earlier`, `wall.agent.jump` | `wallCommands.js` | `commands.rs`, `agent_views.rs` | **Ported & live** (routed via `WallAgents::run_verb_with_screens` with write-time primary scrollback anchors; `wall-smoke-agents.sh` #7–#10) |
| `wall.token` | `wallCommands.js` | `commands.rs`, `wall.rs:3651` | **Deliberately disabled** — returns `Err` (no Go relay on native) |
| `terminal.resize`, `terminal.scroll`, `terminal.move`, `terminal.focus`, `terminal.spawn`, `terminal.kill`, `terminal.refresh` | `TerminalWall.js` / core router | `wall.rs:3920-4124` | **Ported & live** (`wall_scenarios` #26; `wall-smoke.sh`, `wall-smoke-text.sh`) |
| `call`, `help` (router built-ins) | core router | `router.rs:789-800` | **Ported & live** (`wallctl help`) |

### 2.8 All Config Knobs & Settings (47 JS + 2 Native + 3 Static `[wall]`)

All 49 `WallConfig` knobs are defined in `crates/wall-core/src/config.rs` and `settings.rs` (bijection tested by `wall_config_and_knobs_bijection_and_line_style_knobs`):

| Section | Knobs | `wall-core` | Window (`app.rs` / `renderer.rs` / `mount.rs`) | Status |
|---|---|---|---|---|
| **Canvas (10)** | `background`, `gridShow`, `gridMinor`, `gridMajorEvery`, `gridMinPx`, `gridLinePx`, `gridMinorColor`, `gridMajorColor`, `snap` | `config.rs`, `snap.rs`, `wall.rs` | `describe.rs:311`, `paper.rs`, `renderer.rs:1999` | **Ported & live** |
| **Canvas (dead)** | `renderLoop` (`demand` \| `always`) | `config.rs:392` | **Unread** — `app.rs:1951` always runs on-demand | **Dead knob** (§1 #9) |
| **Terminals (2)** | `closeHoldMs`, `barButtonsMinPx` | `config.rs`, `wall.rs` | `mount.rs:295, 369, 524` | **Ported & live** (no visual ring on `⌘X`, §1 #2) |
| **Agent view (4)** | `agentViewSide`, `agentViewWidth`, `agentViewHeight`, `agentViewGap` | `config.rs`, `wall.rs` (`apply_config_side_effects`), `agent_views.rs` | `agents.rs` (`WallHostAdapter`), `panels/agent_view.rs` | **Ported & live** (`wall_scenarios` #39; `wall-smoke-agents.sh` #12) |
| **Fullscreen (3)** | `fullscreenMinLinePx`, `fullscreenPad`, `fullscreenTextStep` | `config.rs`, `fullscreen.rs` | `app.rs` (via `ResizeTerminal` & camera) | **Ported & live** |
| **Text (8 live)** | `fontFamily` *(widened to free-form)*, `fontSize` *(native)*, `lineHeight` *(native)*, `boldIsBright`, `lineThickness`, `underlineRaise`, `curlyAmplitude`, `dimAlpha` | `config.rs`, `settings.rs`, `model.rs` | `app.rs` (`reload_font`, `set_geometry_preserving_panels`, `set_char_widths`, `sessions.set_bold_is_bright`), `renderer.rs` | **Ported & live** |
| **Groups (7)** | `groupLayout`, `groupColumns`, `groupGap`, `groupPad`, `splitGap`, `splitBarRows`, `splitBars` | `config.rs`, `groups.rs`, `split.rs` | `describe.rs`, `app.rs` | **Ported & live** |
| **Status (2)** | `restartNoticeMs`, `errorsShown` | `config.rs`, `status.rs` | `wall.rs`, `mount.rs:340, 396` | **Ported & live** |
| **Input (3 live)** | `optionAsAlt`, `zoomKeys`, `scrollMultiplier` | `config.rs`, `chord.rs`, `keymap.rs`, `pointer.rs` | `app.rs:654, 836, 1519`, `keys.rs`, `mount.rs:633, 792` | **Ported & live** |
| **Input (1 dead)** | `traceInput` | `config.rs:364` | **Unread** — no `[input]` capture logger | **Dead knob** (§1 #9) |
| **Selection (7)** | `selectionColor`, `selectionForeground`, `selectionWordChars`, `copyOnSelect`, `clickRepeatMs`, `selectionClearOnTyping`, `noticeMs` | `config.rs`, `model.rs`, `pointer.rs`, `wall.rs` | `app.rs:1198-1210`, `renderer.rs:1589` | **Ported & live** (`clickRepeatMs` hardcoded to `500 ms` in `app.rs:66` for dblclick; `copyOnSelect=true` internal-only, same as JS) |
| **Menu (1 live)** | `menuSizes` | `config.rs`, `menu.rs:619` | `mount.rs:793`, `context_menu.rs` | **Ported & live** |
| **Menu (1 dead)** | `menuHint` | `config.rs:410` | `mount.rs:525` (`false`), ignored in `context_menu.rs:301` | **Deliberately omitted** (still shown in Settings UI) |
| **Static `[wall]` (3)** | `panelOpacity`, `greekPx`, `cursorBlinkMs` | Not in `WallConfig` | `config/defaults.toml:401, 410, 411` -> `WallStyle` in `renderer.rs` | **Startup-only** (`launch_config.toml`, not live Settings) |

### 2.9 Persistence, Reconciliation & Reload Rules

| Rule | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| All 13 persisted fields (`positions`, `names`, `attached`, `hidden`, `groups`, `sidebar`, `status`, `config`, `settings`, `help`, `fullscreen`, `fullscreenText`, `agentViews`) | `WallStore.js` (`localStorage`) | `store.rs::StoreDocumentRef` (L446) | `app.rs` opens disk-backed `Wall::open` (`wall-<scope>.json`) | `wall_scenarios` #20, #22 (`disk_backed_wall_open_persists_across_instances`) | **Ported & live** (atomic `.tmp` + `rename` replaces `localStorage`) |
| Single-group invariant (moving member leaves old group, collapses split, deletes empty group & sidebar collapse entry) | `WallStore.js` `setGroup` | `store.rs:855-902` | Via `Wall` | `store.rs` unit tests, `wall_scenarios` #20 | **Ported & live** |
| `forget(id)` & `prune(live)` clean terminal refs across all maps while preserving `config` & panel placements | `WallStore.js` `forget`, `prune` | `store.rs:714-819`, `model.rs:1506` | `app.rs` `set_live_terminals` & `terminal_ended` | `store.rs` unit tests, `wall_scenarios` #20 | **Ported & live** |
| Invalid/stale stored `config` knobs rejected & dropped on load without crashing | `WallStore.js`, `wallConfig.js` | `config.rs::apply_dialled`, `model.rs:469` | Via `Wall::open` | `config.rs` unit tests | **Ported & live** |
| Fullscreen reload adoption (resumes persisted single/group fullscreen on reconnect + posts one-time exit-chord notice) | `WallFullscreen.js` `adopt` | `fullscreen.rs:1007-1120`, `wall.rs:2040` | `app.rs` `terminal_appeared(adopted: true)` | `fullscreen.rs` unit tests | **Ported & live** |
| Hiding a split-group pane removes it from the split so siblings reclaim space; hiding a tiled member keeps it in group | `WallGroups.js`, `TerminalWall.js` | `model.rs:1538-1545` | Via `wall.hide` / `wall.show` | `wall_scenarios` #15, `model.rs` tests | **Ported & live** |

### 2.10 Notices, Daemon Trouble States & Edge Cases

| Behaviour | JS source | Ported in `wall-core` | Reached by window | Verified live / test | Status / Difference |
|---|---|---|---|---|---|
| Transient amber wall notices (`Wall::notify` / `notify_at`, auto-clear after `noticeMs`, clock refreshed on idle window) | `main.jsx:127`, `TerminalWall.js` `notify` | `wall.rs` `notify`, `notify_at`, `set_now_ms`, `WallFrame::notice` | `describe.rs:286`, `chrome.rs:1142`, `app.rs` (missing font, clipboard, fullscreen reload) | `wall_scenarios::notify_refreshes_clock_after_idle_window_so_notice_survives_first_tick` | **Ported & live** |
| Daemon outage notice lifecycle: `Disconnected` -> amber restarting (`restartNoticeMs` grace) -> red lost (5 s escalation, sticky `LostLatch`) -> clear on reconnect | `StatusWindow.jsx` `TermdNotice`, `TerminalWall.js` | `status.rs::TermdStatus`, `wall.rs` `on_termd_*`, `WallFrame::status_notice` | `app.rs:1060-1099` (`Disconnected`, `Reconnected`, `SocketAbsent`) -> `describe.rs:286` | `wall_scenarios` #30; `describe.rs` test | **Partial** — `holder_waiting`, `brain_failed`, `socket_unreachable`, `bundled_version` never called; no clickable `×` on canvas notice (§1 #3) |
| Shader hot-reload error notice (`GLYPH_WALL_SHADER_DIR`) | *Not in JS wall* | — | `renderer.rs:1296`, `app.rs:1224` | `renderer.rs` tests | **Native addition** |
| Pointer leaving window (`CursorLeft`) | Browser `pointerleave` clears hover | `Wall::pointer_move` updates hover | **Unhandled** in `app.rs:1851-1942` | — | **Missing** (§1 #10) |
| Drag-and-drop file onto terminal (`DroppedFile`) | Browser default | — | **Unhandled** in `app.rs` | — | **Missing** (same as JS) |

---

## 3. Complete JS File-by-File Coverage Index

### A. `/Users/lugo/localdev/viz-web/glyph3d-js/wall/` (All 44 `.js`/`.jsx` Files)

| # | JS file | Rust counterpart(s) | Status |
|---|---|---|---|
| 1 | `AgentViewOverlay.jsx` | `native/src/wall/panels/agent_view.rs`, `app.rs` | Complete & live |
| 2 | `BarButtons.js` | `native/src/wall/panels/bar_buttons.rs`, `mount.rs:253` | Live (pointer); `hold_key` unwired for `⌘X` |
| 3 | `chord.js` | `crates/wall-core/src/chord.rs` | Complete (+ `is_link_open_mods`) |
| 4 | `contextMenu.js` | `crates/wall-core/src/menu.rs` | Complete |
| 5 | `ContextMenu.jsx` | `native/src/wall/panels/menus/context_menu.rs` | Complete (`menuHint` deliberately omitted) |
| 6 | `dockLayout.js` | `crates/wall-core/src/dock.rs` | Complete |
| 7 | `FitText.jsx` | `native/src/wall/chrome.rs` (shrink-to-fit) + egui panel helpers | Complete |
| 8 | `GroupFrame.js` | `native/src/wall/describe.rs:135`, `chrome.rs:649` | Complete |
| 9 | `HelpPanel.jsx` | `crates/wall-core/src/keymap.rs` (`help_sections`), `native/src/wall/panels/menus/help.rs` | Complete |
| 10 | `holdPress.js` | `panels/bar_buttons.rs`, `sidebar.rs`, `context_menu.rs`; `wall.rs` `close_hold_progress` | Pointer holds complete; keyboard `⌘X` ring missing |
| 11 | `inputTrace.js` | `config.rs::trace_input` (knob only) | **Not ported** (`traceInput` dead) |
| 12 | `labelPlacement.js` | `crates/wall-core/src/labels.rs` | Complete |
| 13 | `main.jsx` | `native/src/wall/app.rs`, `mount.rs`, `panels/topbar.rs` | Complete (no relay/token; `renderLoop` dead; `AgentViewOverlay` unmounted) |
| 14 | `MapLabels.js` | `native/src/wall/describe.rs:258`, `chrome.rs:949` | Complete (bus status dormant; right-click misses pill) |
| 15 | `Palette.jsx` | `crates/wall-core/src/palette.rs`, `native/src/wall/panels/palette.rs` | Complete (agents empty without bus; token omitted) |
| 16 | `Panel.jsx` | `native/src/wall/panels/docked.rs` | Complete |
| 17 | `SelectionOverlay.js` | `native/src/wall/describe.rs:201-241`, `chrome.rs:766-878` | Outlines/lasso/drop complete; **floating `"group (N)"` + `×` bar missing** |
| 18 | `SettingsPanel.jsx` | `native/src/wall/panels/docked/settings.rs` | Complete (4 knobs dead live) |
| 19 | `Sidebar.jsx` | `crates/wall-core/src/sidebar.rs`, `native/src/wall/panels/sidebar.rs` | Complete (agents empty without bus) |
| 20 | `splitTree.js` | `crates/wall-core/src/split.rs` | Complete |
| 21 | `StatusWindow.jsx` | `crates/wall-core/src/status.rs`, `native/src/wall/panels/docked/status.rs` | Partial (no live status pushes; clickable notice `×` disabled) |
| 22 | `terminalScroll.js` | `crates/wall-core/src/scroll.rs` | Complete |
| 23 | `TerminalWall.js` | `crates/wall-core/src/{model,wall}.rs`, `native/src/wall/app.rs` | Complete except `WallAgents` hook-up, `boldIsBright`, `remove_terminal` |
| 24 | `TitleBar.js` | `native/src/wall/describe.rs:153`, `chrome.rs:373` | Complete (bus status dormant) |
| 25 | `vite.config.js` | Cargo workspace (`Cargo.toml`, `build.toml`) | N/A (browser build tool) |
| 26 | `wallAgentView.js` | `crates/wall-core/src/agent_view.rs` | Complete |
| 27 | `WallAgentViews.js` | `crates/wall-core/src/agent_views.rs`, `native/src/wall/agents.rs`, `wall.rs` | Complete & live |
| 28 | `WallBus.js` | `crates/wall-core/src/bus.rs`, `native/src/wall/agents.rs`, `app.rs` | Complete & live |
| 29 | `WallCamera.js` | `crates/wall-core/src/camera.rs`, `native/src/wall/camera.rs` | Complete |
| 30 | `wallCommands.js` | `crates/wall-core/src/commands.rs`, `native/src/wall/agents.rs` | Complete (53/54 + 3 additions live; `wall.token` disabled) |
| 31 | `wallConfig.js` | `crates/wall-core/src/config.rs` | Complete (47 + 2 additions) |
| 32 | `WallFullscreen.js` | `crates/wall-core/src/fullscreen.rs` | Complete (+ geometry rescale of saved canvas grid) |
| 33 | `WallGroups.js` | `crates/wall-core/src/groups.rs` | Complete |
| 34 | `wallKeymap.js` | `crates/wall-core/src/keymap.rs` | Complete (31/31 chords, 5/5 plain keys) |
| 35 | `WallKeys.js` | `crates/wall-core/src/keymap.rs`, `wall.rs`, `native/src/wall/app.rs` | Complete (`noMenuBar` deliberately N/A) |
| 36 | `wallLayout.js` | `crates/wall-core/src/layout.rs` | Complete |
| 37 | `WallMenu.js` | `crates/wall-core/src/wall.rs::context_menu`, `native/src/wall/app.rs` | Complete except map-label & fullscreen-bg right-click targets |
| 38 | `WallMouse.js` | `crates/wall-core/src/mouse.rs`, `pointer.rs::ProgramMouse` | Complete |
| 39 | `WallNames.js` | `crates/wall-core/src/names.rs` | Complete (+ measured `CharWidths`) |
| 40 | `WallPaper.js` | `native/src/wall/paper.rs`, `wall_paper.wgsl` | Complete |
| 41 | `WallPointer.js` | `crates/wall-core/src/pointer.rs` | Complete (+ `links.rs` URL open) |
| 42 | `wallSettings.js` | `crates/wall-core/src/settings.rs` | Complete |
| 43 | `wallSnap.js` | `crates/wall-core/src/snap.rs` | Complete |
| 44 | `WallStore.js` | `crates/wall-core/src/store.rs` | Complete (atomic JSON file storage) |

### B. `packages/glyph3d-core` Modules Used by the Wall

| # | Core JS module | Rust counterpart(s) | Status |
|---|---|---|---|
| 1 | `collections/TerminalGrid.js` | `native/src/wall/{renderer,cell,glyphs}.rs`, `wall_cells.wgsl` | Complete (wide chars, emoji, cursor, decorations, greeking, selection, hover-link; `panelOpacity`/`greekPx`/`cursorBlinkMs` static in `defaults.toml`; `remove_terminal` unused) |
| 2 | `collections/TerminalEmulator.js` | `crates/glyph-vt`, `crates/term-cells`, `crates/term-session` | Complete except `boldIsBright` propagation from `WallConfig` & `OSC 8` spans on `term-cells::Screen` (in flight on `wall-cells`) |
| 3 | `collections/terminalSelection.js` | `crates/wall-core/src/selection.rs` | Complete (cell/word/line/block + VT fill lifetime) |
| 4 | `services/interaction/keyEncoding.js` | `crates/wall-core/src/keymap.rs`, `native/src/wall/keys.rs` | Complete (platform-strict paste chord; dead keys & IME commit live; inline preedit not drawn) |
| 5 | `rendering/glyphMaterial.js` | `native/src/wall/renderer.rs`, `wall_cells.wgsl` | Complete (`lineThickness`, `underlineRaise`, `curlyAmplitude`, `dimAlpha` live) |
| 6 | `fonts/families.js` | `native/src/wall/glyphs.rs`, `app.rs::sync_font_and_text_knobs` | Upgraded — free-form system monospace fonts + live `fontSize` & `lineHeight` |
