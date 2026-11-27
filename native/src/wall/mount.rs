//! Mounts all `egui` chrome strips and panels over the live [`Wall`]:
//!
//! - Topbar action strip ([`panels::topbar`], visible whenever not fullscreen)
//! - Hovered terminal title-bar buttons ([`panels::bar_buttons`]: agent / hide / hold-to-close)
//! - Command palette ([`panels::palette`], open state in `WallFrame::palette_open`)
//! - Sidebar ([`panels::sidebar`], open & dock state in `WallFrame::panels.sidebar`)
//! - Settings ([`panels::settings`], open & dock state in `WallFrame::panels.settings_panel`)
//! - `termd` status chip, window & trouble notice ([`panels::status`], open & dock state in `WallFrame::panels.status_window`)
//! - Right-click / menu-key context menu ([`panels::context_menu`], open state in `WallFrame::menu`)
//! - Help reference sheet ([`panels::help`], open & dock state in `WallFrame::panels.help_panel`)
//!
//! [`Wall`] owns the open, placement and size state of all panels; [`Panels`]
//! owns only each widget's transient UI view state (queries, active row,
//! drafts, drag offsets, hold-to-close progress, and cached help sections).
//!
//! A panel's edge can be dragged ([`panels::resize`]). The size reaches the
//! wall on release, as a verb; until then the docked panels and the topbar
//! are laid out with the dragged width ([`Panels::live_dock`]), so what is
//! stacked inside a panel follows its edge as it moves.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use wall_core::dock::{
    DockLayout, DockPanel, DockSide, DockState, PanelKind, PanelSize, dock_layout,
};
use wall_core::geom::Point;
use wall_core::input::Platform;
use wall_core::keymap::{FieldFocus, HelpGroup};
use wall_core::menu::{
    MenuConfig, MenuDimensions, MenuGroup, MenuState, MenuTerminal, TerminalAgentView,
};
use wall_core::model::WallSnapshot;
use wall_core::palette::{PaletteGroup, PaletteSnapshot, PaletteTerminal};
use wall_core::sidebar::{
    SidebarGroup, SidebarInput as CoreSidebarInput, SidebarMode, SidebarTerminal, build_sidebar,
};
use wall_core::status::{TermdStatus, chip_look};
use wall_core::wall::{Wall, WallFrame, help_sections};

use super::panels::{
    self, bar_buttons, context_menu, help, palette, selection_bar, settings, status, topbar,
};

const PALETTE_INPUT_ID: &str = "wall_palette_query_input";
const SETTINGS_SEARCH_ID: &str = "wall_settings_search_input";

/// Owns every mounted panel's transient `egui` view state across frames.
#[derive(Debug, Default)]
pub struct Panels {
    pub topbar: topbar::TopbarState,
    pub bar_buttons: bar_buttons::BarButtonsState,
    pub selection_bar: selection_bar::SelectionBarState,
    pub palette: palette::PaletteState,
    pub sidebar: panels::sidebar::SidebarState,
    pub settings: settings::SettingsState,
    pub status: status::StatusState,
    pub context_menu: context_menu::ContextMenuState,
    pub help: help::HelpPanelState,
    wall_dragging: bool,
    was_palette_open: bool,
    pending_palette_open: Option<bool>,
    palette_search_had_focus: bool,
    last_sidebar_float: Option<Point>,
    settings_was_open: bool,
    settings_search_had_focus: bool,
    status_was_active: bool,
    help_was_open: bool,
    help_wants_keys: bool,
    last_size_error: Option<String>,
    last_key_frame: Option<u64>,
    cached_help: Option<(Platform, String, Vec<HelpGroup>)>,
}

impl Panels {
    /// Construct a fresh [`Panels`] mount with all transient view states at default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inform the mount whether a wall pointer drag (`buttons != 0`) is currently
    /// active so title-bar hover buttons stay hidden mid-gesture (`pointer.busy` in JS).
    pub fn set_wall_dragging(&mut self, dragging: bool) {
        self.wall_dragging = dragging;
    }

    /// Whether any mounted chrome strip, panel, title-bar hold, or `termd` notice
    /// needs an `egui` pass on this frame.
    pub fn any_open(&self, _wall: &Wall, described: &WallFrame) -> bool {
        if described.fullscreen_backdrop.is_none() {
            // The topbar and `termd` chip are always visible on the canvas outside fullscreen.
            return true;
        }
        self.bar_buttons.is_holding()
            || described.terminals.iter().any(|t| t.close_hold_progress.is_some())
            || (!described.selection_outlines.is_empty() && described.lasso.is_none())
            || (!self.wall_dragging && described.title_bars.iter().any(|b| b.hovered))
            || described.palette_open
            || self.palette.open
            || self.was_palette_open
            || described.menu.is_some()
            || described.panels.sidebar.open
            || described.panels.settings_panel.open
            || self.settings_was_open
            || described.panels.status_window.open
            || described.status_notice.is_some()
            || self.status_was_active
            || described.panels.help_panel.open
            || self.help_was_open
    }

    /// Whether an open menu or panel text field currently claims keyboard events
    /// (routing keys to `egui` rather than to `Wall::key` / the focused terminal).
    pub fn wants_keyboard(&self, wall: &Wall) -> bool {
        wall.menu().is_some()
            || wall.palette_open()
            || self.palette.open
            || (wall.store().settings_panel().open
                && (!self.settings_was_open || self.settings.wants_keyboard()))
            || (wall.store().help_panel().open && (!self.help_was_open || self.help_wants_keys))
    }

    /// Surrender keyboard focus from docked/floating Settings and Help search boxes
    /// when the user clicks on the wall canvas or a global nav chord blurs them.
    pub fn blur_docked_inputs(&mut self) {
        self.settings.blur();
        self.help.blur();
        self.help_wants_keys = false;
    }

    /// The dock layout the panels are drawn with: the wall's, with the
    /// width of any docked panel whose edge is being dragged taken from the
    /// drag. The wall's own layout (what a fullscreen terminal fits beside)
    /// moves when the drag ends and the width is stored.
    fn live_dock(&self, described: &WallFrame) -> DockLayout {
        let p = &described.panels;
        let mut widths = p.dock.widths;
        let dragged = [
            (PanelKind::Sidebar, p.sidebar.side, &self.sidebar.resize),
            (PanelKind::Status, p.status_window.side, &self.status.resize),
            (PanelKind::Settings, p.settings_panel.side, &self.settings.resize),
            (PanelKind::Help, p.help_panel.side, &self.help.resize),
        ];
        let mut any = false;
        for (kind, side, resize) in dragged {
            if side == DockSide::Float {
                continue;
            }
            if let Some(width) = resize.live_width() {
                widths.set(kind, f64::from(width));
                any = true;
            }
        }
        if !any {
            return p.dock;
        }
        dock_layout(&DockState {
            sidebar: DockPanel::new(p.sidebar.open, p.sidebar.side),
            status_window: DockPanel::new(p.status_window.open, p.status_window.side),
            settings_panel: Some(DockPanel::new(p.settings_panel.open, p.settings_panel.side)),
            help_panel: Some(DockPanel::new(p.help_panel.open, p.help_panel.side)),
            widths,
        })
    }

    /// Draw the topbar, hovered title-bar buttons, and every open panel into
    /// `ctx`, returning every emitted `wall.*` / `terminal.*` verb line (including
    /// `wall.palette`) to run through `wall.run` after the pass.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        wall: &Wall,
        described: &WallFrame,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let platform = wall.platform();
        let viewport = panels::safe_viewport_rect(ctx);
        let fullscreen = described.fullscreen_backdrop.is_some();
        let dock = self.live_dock(described);

        // Synchronize `PaletteState::open` with `Wall`'s `palette_open`.
        let just_opened_palette = described.palette_open && !self.was_palette_open;
        if just_opened_palette {
            self.palette.open_find();
        } else if !described.palette_open && self.was_palette_open {
            self.palette.close();
        }
        self.was_palette_open = described.palette_open;

        // Inspect raw key events once per `egui` frame:
        // 1. `egui`'s `Focus::begin_pass` consumes `Key::Escape` from `i.events`
        //    when surrendering focus from a `TextEdit`, before `palette::show` or
        //    `settings::show` runs; restore `Key::Escape` when their search box
        //    just surrendered focus so `Escape` clears query then closes.
        // 2. Toggle `Palette` closed on `⌘K` (`Ctrl+Alt+K` elsewhere) while open.
        let frame_nr = ctx.cumulative_frame_nr();
        if self.last_key_frame != Some(frame_nr) {
            self.last_key_frame = Some(frame_nr);
            let palette_surrendered = self.palette_search_had_focus
                && !ctx.memory(|m| m.has_focus(egui::Id::new(PALETTE_INPUT_ID)));
            let settings_surrendered = self.settings_search_had_focus
                && !ctx.memory(|m| m.has_focus(egui::Id::new(SETTINGS_SEARCH_ID)));
            let mut close_palette_chord = false;

            ctx.input_mut(|i| {
                let mut raw_plain_escape = false;
                for ev in &i.raw.events {
                    if let egui::Event::Key {
                        key,
                        physical_key,
                        pressed: true,
                        modifiers,
                        ..
                    } = ev
                    {
                        if *key == egui::Key::Escape
                            && !modifiers.ctrl
                            && !modifiers.alt
                            && !modifiers.mac_cmd
                            && !modifiers.command
                        {
                            raw_plain_escape = true;
                        }
                        let is_k = *key == egui::Key::K || *physical_key == Some(egui::Key::K);
                        if self.palette.open
                            && !just_opened_palette
                            && is_k
                            && is_palette_chord(platform, *modifiers)
                        {
                            close_palette_chord = true;
                        }
                    }
                }

                if close_palette_chord {
                    i.events.retain(|ev| {
                        !matches!(
                            ev,
                            egui::Event::Key {
                                key: egui::Key::K,
                                pressed: true,
                                ..
                            } | egui::Event::Text(_)
                        )
                    });
                } else if raw_plain_escape
                    && ((self.palette.open && palette_surrendered)
                        || (described.panels.settings_panel.open && settings_surrendered))
                    && !i.events.iter().any(|ev| {
                        matches!(
                            ev,
                            egui::Event::Key {
                                key: egui::Key::Escape,
                                pressed: true,
                                ..
                            }
                        )
                    })
                {
                    i.events.push(egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: Some(egui::Key::Escape),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
            });

            if close_palette_chord {
                self.palette.close();
                self.was_palette_open = false;
                self.pending_palette_open = Some(false);
            }
        }

        // While a top-level modal (`context_menu` or `palette`) is open, withhold
        // keyboard/text events from background panels so they cannot eat keys
        // meant for the modal.
        let top_modal_open = described.menu.is_some() || self.palette.open;
        let saved_modal_events = if top_modal_open {
            ctx.input_mut(|i| {
                let mut kept = Vec::with_capacity(i.events.len());
                let mut held = Vec::new();
                for ev in i.events.drain(..) {
                    if matches!(ev, egui::Event::Key { .. } | egui::Event::Text(_)) {
                        held.push(ev);
                    } else {
                        kept.push(ev);
                    }
                }
                i.events = kept;
                held
            })
        } else {
            Vec::new()
        };

        // 0a. Hovered terminal title-bar buttons (`panels::bar_buttons`), plus
        //     keyboard close-hold progress ring (`⌘X` / `Ctrl+Alt+X`).
        let key_hold = described
            .terminals
            .iter()
            .find_map(|t| t.close_hold_progress.map(|p| (t.id.as_str(), p as f32, t.body_rect)));
        let bar_target_id: Option<String> = self
            .bar_buttons
            .hold_target()
            .map(str::to_owned)
            .or_else(|| key_hold.map(|(id, _, _)| id.to_owned()))
            .or_else(|| {
                (!self.wall_dragging)
                    .then(|| {
                        described
                            .title_bars
                            .iter()
                            .find(|b| b.hovered)
                            .map(|b| b.terminal_id.clone())
                    })
                    .flatten()
            });
        let bar_screen_rect = bar_target_id.as_deref().and_then(|tid| {
            let bar = described
                .title_bars
                .iter()
                .find(|b| b.terminal_id == tid)?;
            let sbox = described.camera.to_screen_box(
                bar.rect,
                described.viewport.w,
                described.viewport.h,
            );
            Some(egui::Rect::from_min_max(
                egui::pos2(sbox.left as f32, sbox.top as f32),
                egui::pos2(sbox.right as f32, sbox.bottom as f32),
            ))
        });
        let (external_hold_progress, fallback_body_rect) = match key_hold {
            Some((hid, progress, body)) if bar_target_id.as_deref() == Some(hid) => {
                let sbox = described.camera.to_screen_box(
                    body,
                    described.viewport.w,
                    described.viewport.h,
                );
                (
                    Some(progress),
                    Some(egui::Rect::from_min_max(
                        egui::pos2(sbox.left as f32, sbox.top as f32),
                        egui::pos2(sbox.right as f32, sbox.bottom as f32),
                    )),
                )
            }
            _ => (None, None),
        };
        let is_agent_open = bar_target_id
            .as_deref()
            .is_some_and(|tid| wall.store().agent_view(tid).is_some_and(|av| av.open));
        let now_secs = ctx.input(|i| i.time);
        let bar_out = bar_buttons::show(
            ctx,
            &mut self.bar_buttons,
            &bar_buttons::BarButtonsInput {
                terminal_id: bar_target_id.as_deref(),
                bar_rect: bar_screen_rect,
                is_agent_open,
                close_hold_ms: wall.config().close_hold_ms,
                min_bar_px: wall.config().bar_buttons_min_px as f32,
                now_secs,
                external_hold_progress,
                fallback_body_rect,
            },
            &mut out,
        );
        if let Some(delay) = bar_out.repaint_after {
            ctx.request_repaint_after(delay);
        }

        // 0a2. Floating selection action bar (`group (N)` + `×`, matching `SelectionOverlay.js`).
        let selection_bounds = if described.selection_outlines.is_empty() || fullscreen {
            None
        } else {
            let mut acc: Option<egui::Rect> = None;
            for sel in &described.selection_outlines {
                let sbox = described.camera.to_screen_box(
                    sel.rect,
                    described.viewport.w,
                    described.viewport.h,
                );
                let r = egui::Rect::from_min_max(
                    egui::pos2(sbox.left as f32, sbox.top as f32),
                    egui::pos2(sbox.right as f32, sbox.bottom as f32),
                );
                acc = Some(match acc {
                    Some(prev) => prev.union(r),
                    None => r,
                });
            }
            acc
        };
        selection_bar::show(
            ctx,
            &mut self.selection_bar,
            &selection_bar::SelectionBarInput {
                count: described.selection_outlines.len(),
                selection_bounds,
                lassoing: described.lasso.is_some(),
                platform,
            },
            &mut out,
        );

        // 0b. Topbar strip (`panels::topbar`), reserving space on the right for `TermdChip`.
        let ns = wall.termd().namespace();
        let wall_name = if ns.is_empty() { "wall" } else { ns };
        let termd_chip_width = if fullscreen {
            0.0
        } else {
            measure_termd_chip_width(ctx, wall.termd())
        };
        let topbar_out = topbar::show(
            ctx,
            &mut self.topbar,
            &topbar::TopbarInput {
                wall_name,
                platform,
                fullscreen,
                inset: dock.inset,
                termd_chip_width,
                notice_width: 0.0,
                palette_open: described.palette_open || self.palette.open,
                sidebar_open: described.panels.sidebar.open,
                status_open: described.panels.status_window.open,
                settings_open: described.panels.settings_panel.open,
                help_open: described.panels.help_panel.open,
                palette_verb: topbar::DEFAULT_PALETTE_VERB,
            },
            &mut out,
        );
        let chip_pos = topbar_out.termd_chip_rect.map(|r| {
            egui::pos2(
                r.min.x,
                r.min.y + ((r.height() - 22.0) * 0.5).max(0.0),
            )
        });

        let now_ms = epoch_now_ms();
        let restart_ms = wall.config().restart_notice_ms.max(0.0).round() as u64;
        let status_window_or_notice =
            described.panels.status_window.open || described.status_notice.is_some();

        let needs_snapshot = self.palette.open
            || described.menu.is_some()
            || described.panels.sidebar.open
            || described.panels.status_window.open;
        let snapshot = needs_snapshot.then(|| wall.snapshot());

        // 1. Sidebar (`wall.sidebar`).
        self.sidebar.open = described.panels.sidebar.open;
        if described.panels.sidebar.open {
            let sb = &described.panels.sidebar;
            self.sidebar.side = sb.side;
            self.sidebar.size = sb.size;
            if self.last_sidebar_float != Some(sb.float) {
                self.sidebar.float_pos = egui::pos2(sb.float.x as f32, sb.float.y as f32);
                self.last_sidebar_float = Some(sb.float);
            }
            if let Some(snap) = &snapshot {
                let core_input = build_core_sidebar_input(wall, snap, &sb.collapsed, sb.mode);
                let input = panels::sidebar::SidebarInput {
                    model: build_sidebar(&core_input),
                    dock_rect: panels::sidebar::sidebar_dock_rect(viewport, sb.side, &dock),
                    platform,
                    close_hold_ms: wall.config().close_hold_ms.max(0.0).round() as u64,
                };
                panels::sidebar::show(ctx, &mut self.sidebar, &input, &mut out);
            }
        } else {
            self.last_sidebar_float = None;
        }

        // Spread simultaneous unplaced (`float: None`) right-anchored floating panels
        // (`status`, `settings`, `help`) side-by-side (or cascaded on narrow viewports)
        // so opening two at once never stacks one panel directly under another, while
        // keeping the exact JS `{ right: 12, top: 46 }` spot when opened alone.
        let (default_status_float, default_settings_float, default_help_float) =
            compute_default_float_spots(viewport, described, &dock, &self.status);

        // 2. `termd` status chip, window & trouble notice (`wall.status`).
        if chip_pos.is_some() || status_window_or_notice || self.status_was_active {
            let wall_names: BTreeMap<String, String> = snapshot
                .as_ref()
                .map(|snap| {
                    snap.terminals
                        .iter()
                        .map(|t| (t.id.clone(), t.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let mut st = described.panels.status_window.clone();
            if st.float.is_none() {
                st.float = default_status_float;
            }
            let input = status::StatusInput {
                placement: st.clone(),
                status: wall.termd().clone(),
                rows: None,
                wall_names,
                now_ms,
                restart_notice_ms: restart_ms,
                errors_shown: wall.config().errors_shown as usize,
                dock_rect: dock_rect(viewport, st.side, dock.status, dock.widths.status),
                chip_pos,
                chrome_inset: (dock.inset.left as f32, dock.inset.right as f32),
                // `egui-ui` owns the interactive dismissible outage banner (`×` -> `wall.status.dismiss`);
                // `app.rs` passes `draw_status_notice = false` to `describe.rs` so only one banner paints.
                show_notice: true,
                platform,
            };
            status::show(ctx, &mut self.status, &input, &mut out);
        }
        self.status_was_active = status_window_or_notice;

        // 3. Settings panel (`wall.settings`).
        let settings_open = described.panels.settings_panel.open;
        if settings_open || self.settings_was_open {
            let mut sp = described.panels.settings_panel.clone();
            if sp.float.is_none() {
                sp.float = default_settings_float;
            }
            let input = settings::SettingsInput {
                placement: sp.clone(),
                config: wall.config().clone(),
                dialled: wall.store().config().keys().cloned().collect(),
                dock_rect: dock_rect(viewport, sp.side, dock.settings, dock.widths.settings),
                platform,
                refused: BTreeMap::new(),
            };
            settings::show(ctx, &mut self.settings, &input, &mut out);
        }
        self.settings_was_open = settings_open;

        // 4. Help panel (`wall.help`).
        let help_open = described.panels.help_panel.open;
        if help_open || self.help_was_open {
            let hp = &described.panels.help_panel;
            let sections = if help_open {
                self.help_sections_for(platform, wall)
            } else {
                Vec::new()
            };
            let input = help::HelpPanelInput {
                open: help_open,
                side: hp.side,
                float_pos: hp.float.or(default_help_float),
                dock_rect: help::HelpPanelInput::dock_rect_for(hp.side, &dock, viewport),
                platform,
                sections,
                size: hp.size,
            };
            self.help_wants_keys = help::show(ctx, &mut self.help, &input, &mut out);
        } else {
            self.help_wants_keys = false;
        }
        self.help_was_open = help_open;

        // Restore keyboard/text events for the topmost active modal (`palette` or `context_menu`).
        if !saved_modal_events.is_empty() {
            ctx.input_mut(|i| i.events.extend(saved_modal_events));
        }

        // 5. Command palette (`WallFrame::palette_open`).
        if self.palette.open {
            // If `context_menu` is also open above `palette`, hold keys for `context_menu`.
            let menu_above_palette = described.menu.is_some();
            let held_for_menu = if menu_above_palette {
                ctx.input_mut(|i| {
                    let mut kept = Vec::with_capacity(i.events.len());
                    let mut held = Vec::new();
                    for ev in i.events.drain(..) {
                        if matches!(ev, egui::Event::Key { .. } | egui::Event::Text(_)) {
                            held.push(ev);
                        } else {
                            kept.push(ev);
                        }
                    }
                    i.events = kept;
                    held
                })
            } else {
                Vec::new()
            };

            if let Some(snap) = &snapshot {
                let input = palette::PaletteInput {
                    snapshot: build_palette_snapshot(wall, snap, described, platform),
                    size: described.panels.palette,
                };
                palette::show(ctx, &mut self.palette, &input, &mut out);
            }
            if !self.palette.open {
                self.was_palette_open = false;
                self.pending_palette_open = Some(false);
            }

            if !held_for_menu.is_empty() {
                ctx.input_mut(|i| i.events.extend(held_for_menu));
            }
        }

        // 6. Right-click / menu-key context menu (`WallFrame::menu`, topmost).
        if let (Some(open_menu), Some(snap)) = (&described.menu, &snapshot) {
            let menu_state = build_menu_state(wall, snap, described);
            let mut open_ctx = context_menu::OpenContextMenu::from_wall(
                open_menu.target.clone(),
                open_menu.at,
                &menu_state,
                &menu_state.config,
                platform,
            );
            open_ctx.size_error.clone_from(&self.last_size_error);
            let input = context_menu::ContextMenuInput {
                open: Some(open_ctx),
                viewport: Some(MenuDimensions::new(
                    described.viewport.w,
                    described.viewport.h,
                )),
                close_hold_ms: wall.config().close_hold_ms,
                menu_hint: false,
                platform,
            };
            let _ = context_menu::show(ctx, &mut self.context_menu, &input, &mut out);
        } else {
            self.last_size_error = None;
            let _ = context_menu::show(
                ctx,
                &mut self.context_menu,
                &context_menu::ContextMenuInput::default(),
                &mut out,
            );
        }

        self.palette_search_had_focus =
            self.palette.open && ctx.memory(|m| m.has_focus(egui::Id::new(PALETTE_INPUT_ID)));
        self.settings_search_had_focus =
            settings_open && ctx.memory(|m| m.has_focus(egui::Id::new(SETTINGS_SEARCH_ID)));

        out
    }

    /// Synchronize [`Wall`] state after the `egui` pass (`palette_open` from
    /// widget self-close plus `field_focus`). Returns `true` if [`Wall`]
    /// mutated and effects should be applied.
    pub fn sync_after_pass(&mut self, wall: &mut Wall, egui_wants_keyboard: bool) -> bool {
        let mut mutated = false;
        if let Some(next) = self.pending_palette_open.take() {
            if wall.palette_open() != next {
                wall.set_palette_open(next);
                mutated = true;
            }
        }
        let field_focus = if wall.menu().is_some() || wall.palette_open() || self.palette.open {
            FieldFocus::Input
        } else if egui_wants_keyboard || self.wants_keyboard(wall) {
            FieldFocus::PanelControl
        } else {
            FieldFocus::None
        };
        wall.set_field_focus(field_focus);
        mutated
    }

    /// Feed router replies back to panel state (e.g. knob validation refusals in
    /// `settings` and `terminal.resize` refusals in `context_menu`).
    pub fn record_verb_reply(&mut self, verb_line: &str, reply_text: &str) {
        let mut parts = verb_line.split_whitespace();
        match parts.next() {
            Some("wall.config") => {
                if let Some(key) = parts.next() {
                    self.settings.record_reply(key, reply_text);
                }
            }
            Some("terminal.resize") => {
                if let Some(err) = reply_text.strip_prefix("ERR:") {
                    self.last_size_error = Some(err.trim().to_owned());
                } else {
                    self.last_size_error = None;
                }
            }
            _ => {}
        }
    }

    /// Close the topmost open panel when plain `Escape` reaches the window
    /// without an active `egui` text field or menu claiming keys.
    pub fn close_open_on_escape(&mut self, wall: &mut Wall) -> bool {
        if wall.menu().is_some() {
            let _ = wall.run("wall.menu close");
            return true;
        }
        if wall.palette_open() || self.palette.open {
            self.palette.close();
            self.was_palette_open = false;
            self.pending_palette_open = None;
            wall.set_palette_open(false);
            return true;
        }
        if wall.store().help_panel().open {
            if !self.help.query.is_empty() {
                self.help.query.clear();
                return true;
            }
            let _ = wall.run("wall.help close");
            return true;
        }
        if wall.store().settings_panel().open {
            if !self.settings.query.is_empty() {
                self.settings.query.clear();
                self.settings.active = 0;
                return true;
            }
            let _ = wall.run("wall.settings close");
            return true;
        }
        if wall.store().status_window().open {
            let _ = wall.run("wall.status close");
            return true;
        }
        if wall.store().sidebar().open {
            let _ = wall.run("wall.sidebar close");
            return true;
        }
        false
    }

    /// Format screen-space panel & chrome geometry lines appended to `app.where`.
    pub fn format_where(&self, wall: &Wall, described: &WallFrame) -> String {
        let mut lines = Vec::new();
        if let Some(bar_rect) = self.topbar.last_bar_rect {
            let btn = |k: &str| fmt_opt_rect(self.topbar.button_rects.get(k).copied());
            lines.push(format!(
                "topbar rect={} spawn={} fit={} find={} sidebar={} status={} settings={} help={} chip={}",
                fmt_rect(bar_rect),
                btn("spawn"),
                btn("fit"),
                btn("find"),
                btn("sidebar"),
                btn("status"),
                btn("settings"),
                btn("help"),
                fmt_opt_rect(self.topbar.last_chip_rect),
            ));
        }

        let bar_target_id: Option<&str> = self.bar_buttons.hold_target().or_else(|| {
            (!self.wall_dragging)
                .then(|| {
                    described
                        .title_bars
                        .iter()
                        .find(|b| b.hovered)
                        .map(|b| b.terminal_id.as_str())
                })
                .flatten()
        });
        if let Some(tid) = bar_target_id {
            if let Some(bar) = described.title_bars.iter().find(|b| b.terminal_id == tid) {
                let sbox =
                    described
                        .camera
                        .to_screen_box(bar.rect, described.viewport.w, described.viewport.h);
                let bar_egui = egui::Rect::from_min_max(
                    egui::pos2(sbox.left as f32, sbox.top as f32),
                    egui::pos2(sbox.right as f32, sbox.bottom as f32),
                );
                if let Some((cluster, [agent, hide, close], _)) =
                    bar_buttons::button_cluster_rects(bar_egui, wall.config().bar_buttons_min_px as f32)
                {
                    lines.push(format!(
                        "bar_buttons terminal={tid} rect={} agent={} hide={} close={} holding={}",
                        fmt_rect(cluster),
                        fmt_rect(agent),
                        fmt_rect(hide),
                        fmt_rect(close),
                        self.bar_buttons.is_holding(),
                    ));
                }
            }
        }

        let pal_open = described.palette_open || self.palette.open;
        // `size` is what is stored for the panel (`dock=<w>`, `float=<w>x<h>`,
        // a `-` for a side at its default) and `grips` its resize strips as
        // drawn, by edge. They end every panel's line but the sidebar's,
        // whose list (`rows`) stays last.
        lines.push(format!(
            "palette open={} query={:?} active={} items={} rect={} size={} grips={}",
            pal_open,
            self.palette.palette.query,
            self.palette.palette.selected,
            self.palette.item_count,
            fmt_opt_rect(self.palette.last_rect),
            fmt_size(&described.panels.palette),
            self.palette.resize.report(),
        ));

        if self.selection_bar.count > 0 {
            lines.push(format!(
                "panel selection_bar count={} rect={} group={} clear={}",
                self.selection_bar.count,
                fmt_opt_rect(self.selection_bar.last_rect),
                fmt_opt_rect(self.selection_bar.group_rect),
                fmt_opt_rect(self.selection_bar.clear_rect),
            ));
        }

        let sb = &described.panels.sidebar;
        let sb_collapsed = if sb.collapsed.is_empty() {
            "none".to_string()
        } else {
            sb.collapsed.join(",")
        };
        let mut sb_folds: Vec<String> = self
            .sidebar
            .group_fold_rects
            .iter()
            .map(|(k, r)| format!("{k}:{}", fmt_rect(*r)))
            .collect();
        sb_folds.sort();
        let mut sb_terms: Vec<String> = self
            .sidebar
            .term_row_rects
            .iter()
            .map(|(k, r)| format!("{k}:{}", fmt_rect(*r)))
            .collect();
        sb_terms.sort();
        let mut sb_agents: Vec<String> = self
            .sidebar
            .agent_row_rects
            .iter()
            .map(|(k, r)| format!("{k}:{}", fmt_rect(*r)))
            .collect();
        sb_agents.sort();
        // `terms` are the terminals' rows and `agents` the rows of agents
        // with no terminal on the wall; `rows` is the whole list as drawn,
        // top to bottom (`panels::sidebar::SidebarState::rows_drawn`).
        lines.push(format!(
            "panel sidebar open={} side={} mode={} collapsed={} rect={} header={} dock_left={} dock_float={} dock_right={} dock_close={} mode_group={} mode_host={} size={} grips={} folds={} terms={} agents={} rows={}",
            sb.open,
            dock_side_name(sb.side),
            sb.mode.as_str(),
            sb_collapsed,
            fmt_opt_rect(self.sidebar.last_rect),
            fmt_opt_rect(self.sidebar.header_drag_rect),
            fmt_opt_rect(self.sidebar.dock_btn_rects[0]),
            fmt_opt_rect(self.sidebar.dock_btn_rects[1]),
            fmt_opt_rect(self.sidebar.dock_btn_rects[2]),
            fmt_opt_rect(self.sidebar.dock_btn_rects[3]),
            fmt_opt_rect(self.sidebar.mode_btn_rects[0]),
            fmt_opt_rect(self.sidebar.mode_btn_rects[1]),
            fmt_size(&sb.size),
            self.sidebar.resize.report(),
            if sb_folds.is_empty() { "none".to_string() } else { sb_folds.join(";") },
            if sb_terms.is_empty() { "none".to_string() } else { sb_terms.join(";") },
            if sb_agents.is_empty() { "none".to_string() } else { sb_agents.join(";") },
            if self.sidebar.rows_drawn.is_empty() {
                "none".to_string()
            } else {
                self.sidebar.rows_drawn.join(";")
            },
        ));

        let st = &described.panels.status_window;
        let st_rows = if self.status.shown_row_ids.is_empty() {
            "none".to_string()
        } else {
            self.status.shown_row_ids.join(",")
        };
        lines.push(format!(
            "panel status open={} side={} rows={} rect={} header={} notice_dismiss={} size={} grips={}",
            st.open,
            dock_side_name(st.side),
            st_rows,
            fmt_opt_rect(self.status.last_rect),
            fmt_opt_rect(self.status.header_drag_rect),
            fmt_opt_rect(self.status.notice_dismiss_rect),
            fmt_size(&st.size),
            self.status.resize.report(),
        ));

        let sp = &described.panels.settings_panel;
        let mut sp_ctrls: Vec<String> = self
            .settings
            .control_rects
            .iter()
            .map(|(k, r)| format!("{k}:{}", fmt_rect(*r)))
            .collect();
        sp_ctrls.sort();
        lines.push(format!(
            "panel settings open={} side={} query={:?} items={} rect={} header={} controls={} size={} grips={}",
            sp.open,
            dock_side_name(sp.side),
            self.settings.query,
            self.settings.visible_count,
            fmt_opt_rect(self.settings.last_rect),
            fmt_opt_rect(self.settings.header_drag_rect),
            if sp_ctrls.is_empty() { "none".to_string() } else { sp_ctrls.join(";") },
            fmt_size(&sp.size),
            self.settings.resize.report(),
        ));

        let hp = &described.panels.help_panel;
        lines.push(format!(
            "panel help open={} side={} query={:?} rows={} rect={} header={} size={} grips={}",
            hp.open,
            dock_side_name(hp.side),
            self.help.query,
            self.help.shown_row_count,
            fmt_opt_rect(self.help.last_rect()),
            fmt_opt_rect(self.help.header_drag_rect),
            fmt_size(&hp.size),
            self.help.resize.report(),
        ));

        let mut menu_items: Vec<String> = self
            .context_menu
            .item_rects
            .iter()
            .map(|(k, r)| format!("{k}:{}", fmt_rect(*r)))
            .collect();
        menu_items.sort();
        lines.push(format!(
            "context_menu open={} rect={} items={}",
            described.menu.is_some(),
            fmt_opt_rect(self.context_menu.last_rect()),
            if menu_items.is_empty() { "none".to_string() } else { menu_items.join(";") },
        ));

        lines.join("\n")
    }

    fn help_sections_for(&mut self, platform: Platform, wall: &Wall) -> Vec<HelpGroup> {
        let zoom_keys = wall.config().zoom_keys.clone();
        if let Some((cached_plat, cached_zoom, sections)) = &self.cached_help {
            if *cached_plat == platform && *cached_zoom == zoom_keys {
                return sections.clone();
            }
        }
        let cmds = wall.list_commands();
        let built = help_sections(platform, &zoom_keys, &cmds)
            .or_else(|_| help_sections(platform, "ctrl alt meta", &cmds))
            .unwrap_or_default();
        self.cached_help = Some((platform, zoom_keys, built.clone()));
        built
    }
}

fn epoch_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn dock_side_name(side: DockSide) -> &'static str {
    match side {
        DockSide::Left => "left",
        DockSide::Right => "right",
        DockSide::Float => "float",
    }
}

fn fmt_rect(r: egui::Rect) -> String {
    format!("{:.2},{:.2},{:.2},{:.2}", r.min.x, r.min.y, r.max.x, r.max.y)
}

fn fmt_opt_rect(r: Option<egui::Rect>) -> String {
    r.map_or_else(|| "none".to_string(), fmt_rect)
}

/// A panel's stored size as `app.where` prints it: `dock=<w>,float=<w>x<h>`,
/// with `-` for whatever is at its default.
fn fmt_size(size: &PanelSize) -> String {
    let px = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v}"));
    format!(
        "dock={},float={}x{}",
        px(size.dock_width),
        px(size.float_width),
        px(size.float_height)
    )
}

/// Default floating origins for simultaneously open unplaced (`float: None`) right-anchored
/// panels (`status`, `settings`, `help`). When only one is open and no panel is docked on the
/// right edge, returns `None` so it uses its exact JS `{ right: 12, top: 46 }` spot; when two
/// or more are open together (or a right dock insets the edge), places them side-by-side from
/// `viewport.right() - inset.right` (cascading diagonally only on narrow viewports).
fn compute_default_float_spots(
    viewport: egui::Rect,
    described: &WallFrame,
    dock: &DockLayout,
    status_state: &status::StatusState,
) -> (Option<Point>, Option<Point>, Option<Point>) {
    let st_unplaced = described.panels.status_window.open
        && described.panels.status_window.side == DockSide::Float
        && described.panels.status_window.float.is_none();
    let sp_unplaced = described.panels.settings_panel.open
        && described.panels.settings_panel.side == DockSide::Float
        && described.panels.settings_panel.float.is_none();
    let hp_unplaced = described.panels.help_panel.open
        && described.panels.help_panel.side == DockSide::Float
        && described.panels.help_panel.float.is_none();

    let count = usize::from(st_unplaced) + usize::from(sp_unplaced) + usize::from(hp_unplaced);
    let inset_right = (dock.inset.right as f32).max(12.0);
    if count == 0 || (count == 1 && inset_right <= 12.5) {
        return (None, None, None);
    }

    // Each at the width it will be drawn: the one set on it, else its own.
    let max_w = (viewport.width() - 32.0).max(140.0);
    let float_w = |kind: PanelKind, size: &PanelSize, own: f32| -> f32 {
        size.float_width
            .map_or(own, |w| w as f32)
            .max(kind.limits().float_min_width as f32)
            .min(max_w)
    };
    let st_w = float_w(
        PanelKind::Status,
        &described.panels.status_window.size,
        status_state.effective_float_width((viewport.width() - 32.0).max(160.0)),
    );
    let sp_w = float_w(
        PanelKind::Settings,
        &described.panels.settings_panel.size,
        PanelKind::Settings.limits().float_default_width as f32,
    );
    let hp_w = float_w(
        PanelKind::Help,
        &described.panels.help_panel.size,
        PanelKind::Help.limits().float_default_width as f32,
    );

    let mut cursor_right = viewport.right() - inset_right;
    let base_y = viewport.top() + 46.0;
    let mut slot = 0usize;
    let mut next_spot = |w: f32| -> Point {
        let candidate_x = cursor_right - w;
        let (x, y) = if slot == 0 || candidate_x >= viewport.left() + 12.0 {
            let x = candidate_x.max(viewport.left() + 8.0);
            cursor_right = x - 12.0;
            (x, base_y)
        } else {
            let stagger = slot as f32 * 36.0;
            let x = (viewport.right() - inset_right - w - stagger).max(viewport.left() + 8.0);
            (x, base_y + stagger)
        };
        slot += 1;
        Point::new(f64::from(x), f64::from(y))
    };

    let st_spot = st_unplaced.then(|| next_spot(st_w));
    let sp_spot = sp_unplaced.then(|| next_spot(sp_w));
    let hp_spot = hp_unplaced.then(|| next_spot(hp_w));
    (st_spot, sp_spot, hp_spot)
}

fn measure_termd_chip_width(ctx: &egui::Context, status: &TermdStatus) -> f32 {
    let look = chip_look(status);
    let label = format!("{} {}", look.mark, look.text);
    ctx.fonts_mut(|f| {
        f.layout_no_wrap(label, egui::FontId::monospace(12.0), egui::Color32::WHITE)
            .size()
            .x
            .ceil()
            + 12.0
    })
}

fn is_palette_chord(platform: Platform, modifiers: egui::Modifiers) -> bool {
    match platform {
        Platform::Mac => {
            (modifiers.mac_cmd || modifiers.command) && !modifiers.ctrl && !modifiers.alt
        }
        Platform::Other => modifiers.ctrl && modifiers.alt && !modifiers.mac_cmd,
    }
}

fn dock_rect(
    viewport: egui::Rect,
    side: DockSide,
    offset: f64,
    width: f64,
) -> Option<egui::Rect> {
    let w = (width as f32).min(viewport.width().max(1.0));
    let off = offset as f32;
    match side {
        DockSide::Left => {
            let left = viewport.left() + off;
            Some(egui::Rect::from_min_max(
                egui::pos2(left, viewport.top()),
                egui::pos2((left + w).min(viewport.right()), viewport.bottom()),
            ))
        }
        DockSide::Right => {
            let right = viewport.right() - off;
            Some(egui::Rect::from_min_max(
                egui::pos2((right - w).max(viewport.left()), viewport.top()),
                egui::pos2(right, viewport.bottom()),
            ))
        }
        DockSide::Float => None,
    }
}

fn build_palette_snapshot(
    wall: &Wall,
    snap: &WallSnapshot,
    described: &WallFrame,
    platform: Platform,
) -> PaletteSnapshot {
    PaletteSnapshot {
        terminals: snap
            .terminals
            .iter()
            .map(|t| PaletteTerminal {
                id: t.id.clone(),
                name: t.name.clone(),
                named: t.named,
                group: t.group.clone(),
                hidden: t.hidden,
                preview: t.preview.clone(),
                attached: t.attached.clone(),
            })
            .collect(),
        groups: snap
            .groups
            .iter()
            .map(|g| PaletteGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                named: g.named,
                layout: g.layout,
                columns: g.columns as usize,
                members: g.members.clone(),
            })
            .collect(),
        focused_id: snap.focused_id.clone(),
        selection: snap.selection.clone(),
        sidebar_open: described.panels.sidebar.open,
        settings_open: described.panels.settings_panel.open,
        snap: snap.config.snap,
        grid_show: snap.config.grid_show,
        platform,
        relay_port: None,
        bus: wall.sidebar_bus_snapshot(),
    }
}

fn build_core_sidebar_input(
    wall: &Wall,
    snap: &WallSnapshot,
    collapsed: &[String],
    mode: SidebarMode,
) -> CoreSidebarInput {
    let store = wall.store();
    CoreSidebarInput {
        terminals: snap
            .terminals
            .iter()
            .map(|t| SidebarTerminal {
                id: t.id.clone(),
                name: t.name.clone(),
                group: t.group.clone(),
                hidden: t.hidden,
                attached: t.attached.clone(),
                agent_view_open: store.agent_view(&t.id).is_some_and(|av| av.open),
            })
            .collect(),
        groups: snap
            .groups
            .iter()
            .map(|g| SidebarGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                layout: g.layout,
                columns: g.columns as usize,
                members: g.members.clone(),
            })
            .collect(),
        focused_id: snap.focused_id.clone(),
        collapsed_groups: collapsed.iter().cloned().collect(),
        bus: wall.sidebar_bus_snapshot(),
        mode,
    }
}

fn build_menu_state(wall: &Wall, snap: &WallSnapshot, described: &WallFrame) -> MenuState {
    let store = wall.store();
    MenuState {
        focused_id: snap.focused_id.clone(),
        selection: snap.selection.clone(),
        fullscreen: snap.fullscreen.clone(),
        sidebar_open: described.panels.sidebar.open,
        status_window_open: described.panels.status_window.open,
        settings_panel_open: described.panels.settings_panel.open,
        config: MenuConfig {
            option_as_alt: snap.config.option_as_alt,
            menu_sizes: snap.config.menu_sizes.clone(),
            snap: snap.config.snap,
            grid_show: snap.config.grid_show,
        },
        terminals: snap
            .terminals
            .iter()
            .map(|t| MenuTerminal {
                id: t.id.clone(),
                name: t.name.clone(),
                named: t.named,
                cols: t.cols,
                rows: t.rows,
                hidden: t.hidden,
                group: t.group.clone(),
                selected: wall
                    .terminal_selection(&t.id)
                    .is_some_and(|sel| !sel.is_empty()),
                agent_view: store.agent_view(&t.id).map(|av| TerminalAgentView {
                    open: av.open,
                    side: av.side,
                }),
            })
            .collect(),
        groups: snap
            .groups
            .iter()
            .map(|g| MenuGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                named: g.named,
                layout: g.layout,
                columns: g.columns,
                bars: g.bars.unwrap_or(true),
                members: g.members.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wall::ui::configure_wall_context;
    use wall_core::input::Mods;
    use wall_core::menu::Verdict;
    use wall_core::pointer::{Buttons, PointerEvent};
    use wall_core::wall::{NoScreens, TerminalAppeared, WallOptions};

    fn test_wall() -> Wall {
        let mut wall = Wall::in_memory(WallOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("t1", 100, 30).with_title("zsh"));
        let _ = wall.run("wall.fit");
        let _ = wall.tick(5.0, 5000.0);
        let _ = wall.drain_effects();
        wall
    }

    fn run_pass_at(
        ctx: &egui::Context,
        time: f64,
        events: Vec<egui::Event>,
        panels: &mut Panels,
        wall: &mut Wall,
    ) -> (Vec<String>, usize) {
        let described = wall.frame();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut verbs = Vec::new();
        let out = ctx.run_ui(raw, |ui| {
            verbs = panels.show(ui.ctx(), wall, &described);
        });
        let shapes = out.shapes.len();
        let wants_keys = !out.shapes.is_empty() && ctx.egui_wants_keyboard_input();
        panels.sync_after_pass(wall, wants_keys);
        for v in &verbs {
            let reply = wall.run(v.as_str());
            panels.record_verb_reply(v, &reply.text);
        }
        (verbs, shapes)
    }

    #[test]
    fn settled_canvas_with_topbar_and_hovered_terminal_requests_no_idle_repaint() {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        let mut panels = Panels::new();

        // Hover t1's body so `t1.hovered == true` and `topbar` + `bar_buttons` are mounted.
        let term = wall.frame().terminals[0].clone();
        let sbox = wall.frame().camera.to_screen_box(term.body_rect, 1400.0, 900.0);
        let body_pt = Point::new(0.5 * (sbox.left + sbox.right), 0.5 * (sbox.top + sbox.bottom));
        wall.pointer_move(
            &PointerEvent {
                pointer_id: 1,
                x: body_pt.x,
                y: body_pt.y,
                button: None,
                buttons: Buttons::NONE,
                mods: Mods::default(),
                time_ms: 100.0,
            },
            &NoScreens,
        );
        let described = wall.frame();
        assert!(described.terminals[0].hovered);
        assert!(!described.animating, "resting hover must not report wall animating");

        // Run a few frames to finish initial Area sizing, then verify egui asks for no further repaint.
        let mut last_delay = std::time::Duration::ZERO;
        for step in 0..5 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 900.0),
                )),
                time: Some(1.0 + f64::from(step) * 0.1),
                ..Default::default()
            };
            let out = ctx.run_ui(raw, |ui| {
                let _ = panels.show(ui.ctx(), &wall, &described);
            });
            last_delay = out
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map_or(std::time::Duration::MAX, |v| v.repaint_delay);
        }
        assert!(
            last_delay >= std::time::Duration::from_secs(60 * 60 * 24),
            "settled canvas with hovered terminal and topbar must sleep idle, got repaint_delay={last_delay:?}"
        );
    }

    #[test]
    fn mounts_topbar_bar_buttons_and_all_six_panels_and_closes_on_escape() {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        let mut panels = Panels::new();

        // Outside fullscreen, topbar + termd chip are active without claiming keyboard.
        let described0 = wall.frame();
        assert!(panels.any_open(&wall, &described0));
        assert!(!panels.wants_keyboard(&wall));
        let (_, shapes0) = run_pass_at(&ctx, 1.0, Vec::new(), &mut panels, &mut wall);
        assert!(shapes0 > 0, "topbar and termd chip should render on canvas");

        // In solo fullscreen with no panel open, topbar hides and zero shapes are emitted.
        assert!(wall.run("wall.fullscreen on t1").text.starts_with("OK"));
        let described_fs = wall.frame();
        assert!(!panels.any_open(&wall, &described_fs));
        let (_, shapes_fs) = run_pass_at(&ctx, 1.1, Vec::new(), &mut panels, &mut wall);
        assert_eq!(shapes_fs, 0, "topbar should hide completely in fullscreen");
        assert!(wall.run("wall.fullscreen off").text.starts_with("OK"));
        let _ = wall.tick(5.0, 10_000.0);

        // Hovering t1's title bar exposes `bar_buttons` and holding `×` closes t1 after `close_hold_ms`.
        let bar = wall.frame().title_bars[0].clone();
        let sbox = wall.frame().camera.to_screen_box(bar.rect, 1400.0, 900.0);
        wall.pointer_move(
            &PointerEvent {
                pointer_id: 1,
                x: (sbox.left + sbox.right) * 0.5,
                y: (sbox.top + sbox.bottom) * 0.5,
                button: None,
                buttons: Buttons::NONE,
                mods: Mods::default(),
                time_ms: 10_100.0,
            },
            &NoScreens,
        );
        assert_eq!(wall.hovered_id(), Some("t1"));
        let bar_egui = egui::Rect::from_min_max(
            egui::pos2(sbox.left as f32, sbox.top as f32),
            egui::pos2(sbox.right as f32, sbox.bottom as f32),
        );
        let (_, [_, hide_rect, _], _) =
            bar_buttons::button_cluster_rects(bar_egui, 14.0).expect("bar tall enough for buttons");
        let hide_center = hide_rect.center();
        let (_, shapes_bar) = run_pass_at(
            &ctx,
            1.15,
            vec![egui::Event::PointerMoved(hide_center)],
            &mut panels,
            &mut wall,
        );
        assert!(shapes_bar > shapes0, "hovered title bar should add bar_buttons shapes");
        let _ = run_pass_at(
            &ctx,
            1.2,
            vec![egui::Event::PointerButton {
                pos: hide_center,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut panels,
            &mut wall,
        );
        let (verbs_hide, _) = run_pass_at(
            &ctx,
            1.25,
            vec![egui::Event::PointerButton {
                pos: hide_center,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut panels,
            &mut wall,
        );
        assert_eq!(verbs_hide, vec!["wall.hide t1".to_string()]);
        assert!(wall.run("wall.reveal t1").text.starts_with("OK"));
        let _ = wall.tick(5.0, 15_000.0);

        // 1. Command palette (`Wall::set_palette_open`).
        wall.set_palette_open(true);
        assert!(panels.wants_keyboard(&wall));
        let (_, shapes_pal) = run_pass_at(&ctx, 2.0, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_pal > 0);
        let esc = vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }];
        let _ = run_pass_at(&ctx, 2.1, esc.clone(), &mut panels, &mut wall);
        assert!(!wall.palette_open(), "Escape should close palette on Wall");

        // 2. Sidebar (`wall.sidebar open`).
        assert!(wall.run("wall.sidebar open").text.starts_with("OK"));
        assert!(!panels.wants_keyboard(&wall));
        let (_, shapes_sb) = run_pass_at(&ctx, 2.2, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_sb > 0);
        assert!(panels.close_open_on_escape(&mut wall));
        assert!(!wall.store().sidebar().open);

        // 3. Settings (`wall.settings open`).
        assert!(wall.run("wall.settings open").text.starts_with("OK"));
        assert!(panels.wants_keyboard(&wall));
        let _ = run_pass_at(&ctx, 2.3, Vec::new(), &mut panels, &mut wall);
        let (verbs_set, _) = run_pass_at(&ctx, 2.4, esc.clone(), &mut panels, &mut wall);
        assert_eq!(verbs_set, vec!["wall.settings close".to_string()]);
        assert!(!wall.store().settings_panel().open);

        // 4. Status (`wall.status open`).
        assert!(wall.run("wall.status open").text.starts_with("OK"));
        assert!(!panels.wants_keyboard(&wall));
        let (_, shapes_st) = run_pass_at(&ctx, 2.5, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_st > 0);
        assert!(panels.close_open_on_escape(&mut wall));
        assert!(!wall.store().status_window().open);

        // 5. Right-click context menu (`Wall::context_menu` -> `Verdict::Ours`).
        let dec = wall.context_menu(500.0, 400.0, Mods::default());
        assert_eq!(dec.verdict, Verdict::Ours);
        assert!(panels.wants_keyboard(&wall));
        let _ = run_pass_at(&ctx, 2.6, Vec::new(), &mut panels, &mut wall);
        let (verbs_menu, _) = run_pass_at(&ctx, 2.7, esc.clone(), &mut panels, &mut wall);
        assert_eq!(verbs_menu, vec!["wall.menu close".to_string()]);
        assert!(wall.menu().is_none());

        // 6. Help (`wall.help open`).
        assert!(wall.run("wall.help open").text.starts_with("OK"));
        assert!(panels.wants_keyboard(&wall));
        let _ = run_pass_at(&ctx, 2.8, Vec::new(), &mut panels, &mut wall);
        let (verbs_help, _) = run_pass_at(&ctx, 2.9, esc, &mut panels, &mut wall);
        assert_eq!(verbs_help, vec!["wall.help close".to_string()]);
        assert!(!wall.store().help_panel().open);

        // 7. `wall.palette` passes through `wall.run`, and `sidebar_bus_snapshot` + `platform`
        //    feed the sidebar and palette directly from `Wall`.
        use wall_core::bus::{GomuxAgent, GomuxSnapshot};
        wall.set_bus_snapshot(
            Some(GomuxSnapshot {
                gen: 1,
                host: Some("workstation".to_owned()),
                agents: vec![GomuxAgent {
                    name: "coder".to_owned(),
                    host: Some("workstation".to_owned()),
                    live: true,
                    status: Some("idle".to_owned()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            "",
        );
        assert!(wall.run("wall.palette open").text.starts_with("OK"));
        assert!(wall.palette_open());
        let snap = wall.snapshot();
        let described = wall.frame();
        let pal_snap = build_palette_snapshot(&wall, &snap, &described, wall.platform());
        assert_eq!(pal_snap.bus.agents.len(), 1);
        assert_eq!(pal_snap.bus.agents[0].name, "coder");
        let sb_in = build_core_sidebar_input(&wall, &snap, &[], SidebarMode::Host);
        assert_eq!(sb_in.mode, SidebarMode::Host);
        assert_eq!(sb_in.bus.agents.len(), 1);
        assert_eq!(sb_in.bus.agents[0].name, "coder");
        assert!(wall.run("wall.palette close").text.starts_with("OK"));
        assert!(!wall.palette_open());
    }

    #[test]
    fn sidebar_is_one_list_and_its_mode_switch_runs_the_verb_through_the_mount() {
        use wall_core::bus::{GomuxAgent, GomuxSnapshot};

        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        wall.terminal_appeared(TerminalAppeared::new("t2", 100, 30).with_title("zsh"));
        wall.terminal_appeared(TerminalAppeared::new("gx-coder", 100, 30).with_title("coder"));
        let mut panels = Panels::new();
        let live = |name: &str, host: &str, status: &str| GomuxAgent {
            name: name.to_owned(),
            host: Some(host.to_owned()),
            live: true,
            status: Some(status.to_owned()),
            ..Default::default()
        };
        wall.set_bus_snapshot(
            Some(GomuxSnapshot {
                gen: 1,
                host: Some("workstation".to_owned()),
                agents: vec![
                    live("coder", "workstation", "processing"),
                    live("gate", "workstation", "idle"),
                    live("probe", "workstation", "waiting"),
                ],
                ..Default::default()
            }),
            "",
        );
        // `gate` is attached from another host: the wall asks for a terminal
        // (no process runs here), and the one that appears is its attach terminal.
        assert!(wall.run("wall.attach gate@workstation").text.starts_with("OK"));
        wall.terminal_appeared(TerminalAppeared::new("t9", 100, 30));
        let _ = wall.drain_effects();
        assert_eq!(wall.store().attached("t9"), Some("gate@workstation"));
        assert!(wall.run("wall.group t9 t1 gx-coder").text.starts_with("OK"));
        assert!(wall.run("wall.sidebar left").text.starts_with("OK"));
        assert_eq!(wall.store().sidebar().mode, SidebarMode::Group);
        let _ = run_pass_at(&ctx, 1.0, Vec::new(), &mut panels, &mut wall);
        let _ = run_pass_at(&ctx, 1.1, Vec::new(), &mut panels, &mut wall);

        // By group: `coder` and `gate` are in their terminals' rows, this
        // machine's members before workstation's though `t9` was grouped first;
        // `probe` has no terminal.
        assert_eq!(
            panels.sidebar.rows_drawn,
            vec![
                "g:g-1[workstation=2,workstation=1]",
                "t:t1",
                "t:gx-coder+coder@workstation=processing",
                "t:t9+gate@workstation=idle",
                "s:none[workstation=1,workstation=1]",
                "t:t2",
                "a:probe@workstation=waiting",
            ]
        );

        // The header's `host` half runs the verb, and the store keeps it.
        let host_btn = panels.sidebar.mode_btn_rects[1]
            .expect("the header draws the mode switch")
            .center();
        let _ = run_pass_at(
            &ctx,
            1.2,
            vec![
                egui::Event::PointerMoved(host_btn),
                egui::Event::PointerButton {
                    pos: host_btn,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut panels,
            &mut wall,
        );
        let (verbs, _) = run_pass_at(
            &ctx,
            1.25,
            vec![egui::Event::PointerButton {
                pos: host_btn,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut panels,
            &mut wall,
        );
        assert_eq!(verbs, vec!["wall.sidebar mode host".to_string()]);
        assert_eq!(wall.store().sidebar().mode, SidebarMode::Host);
        assert!(wall.store().sidebar().open);

        let _ = run_pass_at(&ctx, 1.3, Vec::new(), &mut panels, &mut wall);
        assert_eq!(
            panels.sidebar.rows_drawn,
            vec![
                "h:workstation[3]",
                "t:t1",
                "t:gx-coder+coder@workstation=processing",
                "t:t2",
                "h:workstation[2]",
                "t:t9+gate@workstation=idle",
                "a:probe@workstation=waiting",
            ]
        );

        // `app.where` prints the mode, the switch and the list as drawn.
        let described = wall.frame();
        let report = panels.format_where(&wall, &described);
        let line = report
            .lines()
            .find(|l| l.starts_with("panel sidebar "))
            .expect("app.where has a sidebar line");
        assert!(line.contains(" mode=host "), "{line}");
        assert!(!line.contains("mode_host=none"), "{line}");
        assert!(line.contains(" agents=probe@workstation:"), "{line}");
        assert!(
            line.ends_with(
                " rows=h:workstation[3];t:t1;t:gx-coder+coder@workstation=processing;t:t2;h:workstation[2];t:t9+gate@workstation=idle;a:probe@workstation=waiting"
            ),
            "{line}"
        );

        assert!(wall.run("wall.sidebar mode group").text.starts_with("OK"));
        let _ = run_pass_at(&ctx, 1.4, Vec::new(), &mut panels, &mut wall);
        assert_eq!(panels.sidebar.rows_drawn[0], "g:g-1[workstation=2,workstation=1]");
    }

    #[test]
    fn keyboard_close_hold_progress_drives_ring_through_mount() {
        use wall_core::chord::chord_mods;
        use wall_core::input::KeyEvent;

        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        let mut panels = Panels::new();
        wall.set_now_ms(5_000.0);

        let chord = chord_mods(wall.platform());
        let _ = wall.key(
            &KeyEvent {
                code: "KeyX".to_owned(),
                key: "x".to_owned(),
                text: Some("x".to_owned()),
                mods: chord,
                repeat: false,
                pressed: true,
            },
            &NoScreens,
        );
        // Advance halfway through the 650 ms hold; terminal stays alive with ~0.5 progress.
        let _ = wall.tick(0.016, 5_350.0);
        let described = wall.frame();
        let hold_p = described
            .terminals
            .iter()
            .find(|t| t.id == "t1")
            .and_then(|t| t.close_hold_progress)
            .expect("keyboard Chord+X hold should set close_hold_progress");
        assert!(hold_p > 0.4 && hold_p < 0.7, "expected mid-hold progress, got {hold_p}");

        let want = egui::epaint::ColorMode::Solid(egui::Color32::from_rgb(0xe5, 0x53, 0x4b));
        let full = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1400.0, 900.0),
                )),
                time: Some(5.35),
                ..Default::default()
            },
            |ui| {
                let verbs = panels.show(ui.ctx(), &wall, &described);
                assert!(verbs.is_empty(), "mount must not emit duplicate wall.close");
            },
        );
        let has_ring = full.shapes.iter().any(|cs| match &cs.shape {
            egui::Shape::Path(p) => p.stroke.color == want && p.points.len() >= 3,
            egui::Shape::Vec(v) => v.iter().any(|s| {
                matches!(s, egui::Shape::Path(p) if p.stroke.color == want && p.points.len() >= 3)
            }),
            _ => false,
        });
        assert!(has_ring, "expected #e5534b close-hold ring painted during keyboard Chord+X hold");
    }

    #[test]
    fn termd_outage_notice_dismiss_button_clears_red_banner() {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        let mut panels = Panels::new();

        // Simulate daemon with a live terminal disappearing -> red dismissible `Lost` banner.
        let _ = wall.on_termd_status(
            serde_json::json!({
                "daemon": { "version": "1.0", "ns": "rs-wall" },
                "terminals": [{ "id": "t1", "cols": 100, "rows": 30 }],
            }),
            1_000,
        );
        wall.on_termd_disconnected(false, 2_000);
        wall.on_termd_socket_absent(3_000);
        assert!(
            wall.frame().status_notice.as_ref().is_some_and(|n| n.dismiss),
            "expected dismissible Lost outage notice"
        );

        // Two warm-up passes so egui lays out the banner and records `notice_dismiss_rect`.
        let _ = run_pass_at(&ctx, 1.0, Vec::new(), &mut panels, &mut wall);
        let _ = run_pass_at(&ctx, 1.1, Vec::new(), &mut panels, &mut wall);
        let dismiss_pt = panels
            .status
            .notice_dismiss_rect
            .expect("dismiss × rect must be exposed on red outage notice")
            .center();

        // Clicking × emits `wall.status.dismiss` and clears the banner.
        let _ = run_pass_at(
            &ctx,
            1.2,
            vec![
                egui::Event::PointerMoved(dismiss_pt),
                egui::Event::PointerButton {
                    pos: dismiss_pt,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut panels,
            &mut wall,
        );
        let (verbs, _) = run_pass_at(
            &ctx,
            1.3,
            vec![egui::Event::PointerButton {
                pos: dismiss_pt,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut panels,
            &mut wall,
        );
        assert_eq!(verbs, vec!["wall.status.dismiss".to_string()]);
        assert!(
            wall.frame().status_notice.is_none(),
            "clicking × must clear the red outage notice"
        );
    }

    #[test]
    fn panels_are_drawn_at_their_stored_size_and_an_edge_drag_resizes_them() {
        use crate::wall::panels::resize::Grip;
        use wall_core::dock::DOCK_WIDTHS;

        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        let mut panels = Panels::new();
        let mut now = 1.0;
        let mut pass = |events: Vec<egui::Event>, panels: &mut Panels, wall: &mut Wall| {
            now += 0.05;
            run_pass_at(&ctx, now, events, panels, wall).0
        };
        let button = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let (view_w, sidebar_w, settings_w) = (
            1400.0_f32,
            DOCK_WIDTHS.sidebar as f32,
            DOCK_WIDTHS.settings as f32,
        );

        // The sidebar and settings docked right: settings sits inside the sidebar.
        assert!(wall.run("wall.sidebar right").text.starts_with("OK"));
        assert!(wall.run("wall.settings right").text.starts_with("OK"));
        pass(Vec::new(), &mut panels, &mut wall);
        pass(Vec::new(), &mut panels, &mut wall);
        let sb = panels.sidebar.last_rect.unwrap();
        assert_eq!((sb.right(), sb.width()), (view_w, sidebar_w));
        let sp = panels.settings.last_rect.unwrap();
        assert_eq!((sp.right(), sp.width()), (view_w - sidebar_w, settings_w));

        // Drag the sidebar's canvas-facing edge 100 px left.
        let grip = panels.sidebar.resize.grip_rect(Grip::Left).unwrap().center();
        let to = grip - egui::vec2(100.0, 0.0);
        pass(vec![egui::Event::PointerMoved(grip)], &mut panels, &mut wall);
        pass(vec![button(grip, true)], &mut panels, &mut wall);
        pass(vec![egui::Event::PointerMoved(grip - egui::vec2(40.0, 0.0))], &mut panels, &mut wall);
        let during = pass(vec![egui::Event::PointerMoved(to)], &mut panels, &mut wall);
        assert!(during.is_empty(), "nothing is sent while the drag lasts: {during:?}");
        // The next pass draws with the dragged width: the sidebar is wider,
        // settings has moved over with its edge, and the topbar clears both;
        // the wall's own layout waits for the release.
        pass(Vec::new(), &mut panels, &mut wall);
        let sb = panels.sidebar.last_rect.unwrap();
        assert_eq!((sb.right(), sb.width()), (view_w, sidebar_w + 100.0));
        let sp = panels.settings.last_rect.unwrap();
        assert_eq!((sp.right(), sp.width()), (view_w - sidebar_w - 100.0, settings_w));
        let bar = panels.topbar.last_bar_rect.unwrap();
        assert!(bar.right() <= sp.left(), "topbar {bar:?} under settings {sp:?}");
        assert_eq!(wall.frame().panels.dock.widths.sidebar, DOCK_WIDTHS.sidebar);

        // Release: the width goes to the wall as a verb, and is stored.
        let sent = pass(vec![button(to, false)], &mut panels, &mut wall);
        assert_eq!(sent, vec!["wall.sidebar width 348".to_string()]);
        assert_eq!(wall.store().sidebar().size.dock_width, Some(348.0));
        pass(Vec::new(), &mut panels, &mut wall);
        assert_eq!(panels.sidebar.last_rect.unwrap().width(), 348.0);
        assert_eq!(wall.frame().panels.dock.offset(wall_core::dock::PanelKind::Settings), 348.0);
        let described = wall.frame();
        let line = panels.format_where(&wall, &described);
        let sidebar_line = line.lines().find(|l| l.starts_with("panel sidebar ")).unwrap();
        assert!(sidebar_line.contains(" size=dock=348,float=-x- grips=left:"), "{sidebar_line}");

        // A double click on the edge puts the default back.
        let grip = panels.sidebar.resize.grip_rect(Grip::Left).unwrap().center();
        let mut sent = Vec::new();
        for pressed in [true, false, true, false] {
            sent.extend(pass(vec![button(grip, pressed)], &mut panels, &mut wall));
        }
        assert_eq!(sent, vec!["wall.sidebar width default".to_string()]);
        assert_eq!(wall.store().sidebar().size.dock_width, None);
        pass(Vec::new(), &mut panels, &mut wall);
        assert_eq!(panels.sidebar.last_rect.unwrap().width(), sidebar_w);
        assert!(wall.run("wall.sidebar close").text.starts_with("OK"));

        // Floating, each panel is the size set on it, where it was put.
        for (verb, at, size) in [
            ("wall.settings", (100.0, 110.0), (600.0, 420.0)),
            ("wall.help", (60.0, 70.0), (520.0, 300.0)),
            ("wall.status", (20.0, 400.0), (560.0, 260.0)),
            ("wall.sidebar", (700.0, 80.0), (330.0, 350.0)),
        ] {
            for other in ["wall.settings", "wall.help", "wall.status", "wall.sidebar"] {
                let _ = wall.run(format!("{other} close"));
            }
            assert!(wall.run(format!("{verb} float {} {}", at.0, at.1)).text.starts_with("OK"));
            assert!(wall.run(format!("{verb} size {} {}", size.0, size.1)).text.starts_with("OK"));
            for _ in 0..3 {
                pass(Vec::new(), &mut panels, &mut wall);
            }
            let rect = match verb {
                "wall.settings" => panels.settings.last_rect,
                "wall.help" => panels.help.last_rect(),
                "wall.status" => panels.status.last_rect,
                _ => panels.sidebar.last_rect,
            }
            .unwrap();
            assert_eq!(
                (rect.left(), rect.top(), rect.width(), rect.height()),
                (at.0, at.1, size.0, size.1),
                "{verb}"
            );
        }

        // A floating panel's right edge, dragged: its place is pinned and
        // its width set; the height it had is kept.
        let grip = panels.sidebar.resize.grip_rect(Grip::Right).unwrap().center();
        let to = grip + egui::vec2(70.0, 0.0);
        pass(vec![egui::Event::PointerMoved(grip)], &mut panels, &mut wall);
        pass(vec![button(grip, true)], &mut panels, &mut wall);
        pass(vec![egui::Event::PointerMoved(grip + egui::vec2(30.0, 0.0))], &mut panels, &mut wall);
        pass(vec![egui::Event::PointerMoved(to)], &mut panels, &mut wall);
        let sent = pass(vec![button(to, false)], &mut panels, &mut wall);
        assert_eq!(
            sent,
            vec!["wall.sidebar float 700 80".to_string(), "wall.sidebar size 400".to_string()]
        );
        let stored = wall.store().sidebar().size;
        assert_eq!((stored.float_width, stored.float_height), (Some(400.0), Some(350.0)));

        // The palette is as wide as it was sized, and stays centred.
        let _ = wall.run("wall.sidebar close");
        assert!(wall.run("wall.palette size 700 380").text.starts_with("OK"));
        wall.set_palette_open(true);
        for _ in 0..3 {
            pass(Vec::new(), &mut panels, &mut wall);
        }
        let pal = panels.palette.last_rect.unwrap();
        assert_eq!((pal.width(), pal.height()), (700.0, 380.0));
        assert_eq!(pal.center().x, view_w * 0.5);
        let described = wall.frame();
        let line = panels.format_where(&wall, &described);
        let palette_line = line.lines().find(|l| l.starts_with("palette ")).unwrap();
        assert!(palette_line.contains(" size=dock=-,float=700x380 grips=left:"), "{palette_line}");
    }
}
