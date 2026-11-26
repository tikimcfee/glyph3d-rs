//! Mounts all six `egui` chrome panels over the live [`Wall`]:
//!
//! - Command palette ([`panels::palette`], open state in `WallFrame::palette_open`)
//! - Sidebar ([`panels::sidebar`], open & dock state in `WallFrame::panels.sidebar`)
//! - Settings ([`panels::settings`], open & dock state in `WallFrame::panels.settings_panel`)
//! - `termd` status window + trouble notice ([`panels::status`], open & dock state in `WallFrame::panels.status_window`)
//! - Right-click / menu-key context menu ([`panels::context_menu`], open state in `WallFrame::menu`)
//! - Help reference sheet ([`panels::help`], open & dock state in `WallFrame::panels.help_panel`)
//!
//! [`Wall`] owns the open and placement state of all six panels; [`Panels`]
//! owns only each panel's transient UI view state (queries, active row, drafts,
//! drag offsets, hold-to-close progress, and cached help sections).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use wall_core::dock::{DOCK_WIDTHS, DockSide};
use wall_core::geom::Point;
use wall_core::input::Platform;
use wall_core::keymap::{FieldFocus, HelpGroup};
use wall_core::menu::{
    MenuConfig, MenuDimensions, MenuGroup, MenuState, MenuTerminal, TerminalAgentView,
};
use wall_core::model::WallSnapshot;
use wall_core::palette::{PaletteGroup, PaletteSnapshot, PaletteTerminal};
use wall_core::sidebar::{
    BusSnapshot, SidebarGroup, SidebarInput as CoreSidebarInput, SidebarTerminal, build_sidebar,
};
use wall_core::status::{TermdStatus, notice_for};
use wall_core::wall::{Wall, WallFrame, help_sections};

use super::panels::{self, context_menu, help, palette, settings, status};

const PALETTE_INPUT_ID: &str = "wall_palette_query_input";
const SETTINGS_SEARCH_ID: &str = "wall_settings_search_input";

/// Owns every mounted panel's transient `egui` view state across frames.
#[derive(Debug, Default)]
pub struct Panels {
    pub palette: palette::PaletteState,
    pub sidebar: panels::sidebar::SidebarState,
    pub settings: settings::SettingsState,
    pub status: status::StatusState,
    pub context_menu: context_menu::ContextMenuState,
    pub help: help::HelpPanelState,
    was_palette_open: bool,
    palette_closed_by_ui: bool,
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

    /// Whether any mounted panel (or `termd` trouble banner) needs an `egui` pass
    /// on this frame, including one cleanup pass when a stateful panel just closed.
    pub fn any_open(&self, wall: &Wall, described: &WallFrame) -> bool {
        let now_ms = epoch_now_ms();
        let restart_ms = wall.config().restart_notice_ms.max(0.0).round() as u64;
        described.palette_open
            || self.palette.open
            || self.was_palette_open
            || described.menu.is_some()
            || described.panels.sidebar.open
            || described.panels.settings_panel.open
            || self.settings_was_open
            || described.panels.status_window.open
            || status_has_notice(wall.termd(), now_ms, restart_ms)
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
            || wall.store().settings_panel().open
            || (wall.store().help_panel().open && (!self.help_was_open || self.help_wants_keys))
    }

    /// Draw every currently open panel into `ctx`, reading open/dock state from
    /// `wall` and `described`, and returning the `wall.*` / `terminal.*` verb
    /// lines to run after the pass.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        wall: &Wall,
        described: &WallFrame,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let platform = Platform::host();
        let viewport = panels::safe_viewport_rect(ctx);

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
                self.palette_closed_by_ui = true;
            }
        }

        let now_ms = epoch_now_ms();
        let restart_ms = wall.config().restart_notice_ms.max(0.0).round() as u64;
        let status_active = described.panels.status_window.open
            || status_has_notice(wall.termd(), now_ms, restart_ms);

        let needs_snapshot = self.palette.open
            || described.menu.is_some()
            || described.panels.sidebar.open
            || status_active
            || self.status_was_active;
        let snapshot = needs_snapshot.then(|| wall.snapshot());

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

        // 1. Sidebar (`wall.sidebar`).
        self.sidebar.open = described.panels.sidebar.open;
        if described.panels.sidebar.open {
            let sb = &described.panels.sidebar;
            self.sidebar.side = sb.side;
            if self.last_sidebar_float != Some(sb.float) {
                self.sidebar.float_pos = egui::pos2(sb.float.x as f32, sb.float.y as f32);
                self.last_sidebar_float = Some(sb.float);
            }
            if let Some(snap) = &snapshot {
                let core_input = build_core_sidebar_input(wall, snap, &sb.collapsed);
                let input = panels::sidebar::SidebarInput {
                    model: build_sidebar(&core_input),
                    dock_rect: panels::sidebar::sidebar_dock_rect(
                        viewport,
                        sb.side,
                        &described.panels.dock,
                    ),
                    platform,
                    close_hold_ms: wall.config().close_hold_ms.max(0.0).round() as u64,
                };
                panels::sidebar::show(ctx, &mut self.sidebar, &input, &mut out);
            }
        } else {
            self.last_sidebar_float = None;
        }

        // 2. `termd` status window & trouble notice (`wall.status`).
        if status_active || self.status_was_active {
            let wall_names: BTreeMap<String, String> = snapshot
                .as_ref()
                .map(|snap| {
                    snap.terminals
                        .iter()
                        .map(|t| (t.id.clone(), t.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let st = &described.panels.status_window;
            let input = status::StatusInput {
                placement: st.clone(),
                status: wall.termd().clone(),
                rows: None,
                wall_names,
                now_ms,
                restart_notice_ms: restart_ms,
                errors_shown: wall.config().errors_shown as usize,
                dock_rect: dock_rect(
                    viewport,
                    st.side,
                    described.panels.dock.status,
                    DOCK_WIDTHS.status,
                ),
                chip_pos: None,
                chrome_inset: (
                    described.panels.dock.inset.left as f32,
                    described.panels.dock.inset.right as f32,
                ),
                platform,
            };
            status::show(ctx, &mut self.status, &input, &mut out);
        }
        self.status_was_active = status_active;

        // 3. Settings panel (`wall.settings`).
        let settings_open = described.panels.settings_panel.open;
        if settings_open || self.settings_was_open {
            let sp = &described.panels.settings_panel;
            let input = settings::SettingsInput {
                placement: sp.clone(),
                config: wall.config().clone(),
                dialled: wall.store().config().keys().cloned().collect(),
                dock_rect: dock_rect(
                    viewport,
                    sp.side,
                    described.panels.dock.settings,
                    DOCK_WIDTHS.settings,
                ),
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
                float_pos: hp.float,
                dock_rect: help::HelpPanelInput::dock_rect_for(
                    hp.side,
                    &described.panels.dock,
                    viewport,
                ),
                platform,
                sections,
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
                    snapshot: build_palette_snapshot(snap, described, platform),
                };
                palette::show(ctx, &mut self.palette, &input, &mut out);
            }
            if !self.palette.open {
                self.was_palette_open = false;
                self.palette_closed_by_ui = true;
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
                menu_hint: wall.config().menu_hint,
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
        self.settings_search_had_focus = settings_open
            && ctx.memory(|m| m.has_focus(egui::Id::new(SETTINGS_SEARCH_ID)));

        out
    }

    /// Synchronize [`Wall`] state after the `egui` pass (`palette_open` when the
    /// palette closed itself, plus `field_focus`). Returns `true` if [`Wall`]
    /// mutated and effects should be applied.
    pub fn sync_after_pass(&mut self, wall: &mut Wall, egui_wants_keyboard: bool) -> bool {
        let mut mutated = false;
        if self.palette_closed_by_ui {
            self.palette_closed_by_ui = false;
            if wall.palette_open() {
                wall.set_palette_open(false);
                mutated = true;
            }
        }
        let field_focus = if egui_wants_keyboard || self.wants_keyboard(wall) {
            FieldFocus::Input
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
            self.palette_closed_by_ui = false;
            wall.set_palette_open(false);
            return true;
        }
        if wall.store().help_panel().open {
            let _ = wall.run("wall.help close");
            return true;
        }
        if wall.store().settings_panel().open {
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

fn status_has_notice(status: &TermdStatus, now_ms: u64, restart_notice_ms: u64) -> bool {
    let since_ms = if status.since_ms() > 0 {
        status.since_ms()
    } else {
        now_ms
    };
    notice_for(status, since_ms, now_ms, restart_notice_ms).is_some()
}

fn is_palette_chord(platform: Platform, modifiers: egui::Modifiers) -> bool {
    match platform {
        Platform::Mac => (modifiers.mac_cmd || modifiers.command) && !modifiers.ctrl && !modifiers.alt,
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
        bus: BusSnapshot::default(),
    }
}

fn build_core_sidebar_input(
    wall: &Wall,
    snap: &WallSnapshot,
    collapsed: &[String],
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
        bus: BusSnapshot::default(),
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
    use wall_core::wall::{TerminalAppeared, WallOptions};

    fn test_wall() -> Wall {
        let mut wall = Wall::in_memory(WallOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("t1", 100, 30).with_title("zsh"));
        let _ = wall.drain_effects();
        wall
    }

    fn run_pass(ctx: &egui::Context, events: Vec<egui::Event>, panels: &mut Panels, wall: &mut Wall) -> (Vec<String>, usize) {
        let described = wall.frame();
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        raw.time = Some(1.0);
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
    fn mounts_all_six_panels_from_wall_state_and_closes_on_escape() {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        let mut wall = test_wall();
        let mut panels = Panels::new();

        // Initially nothing is open and zero shapes are emitted.
        let described0 = wall.frame();
        assert!(!panels.any_open(&wall, &described0));
        assert!(!panels.wants_keyboard(&wall));
        let (_, shapes0) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert_eq!(shapes0, 0);

        // 1. Command palette (`Wall::set_palette_open`).
        wall.set_palette_open(true);
        assert!(panels.wants_keyboard(&wall));
        let (_, shapes_pal) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_pal > 0, "palette should emit shapes when open");
        // Escape with empty query closes palette and syncs `wall.palette_open()` back to false.
        let esc = vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }];
        let _ = run_pass(&ctx, esc.clone(), &mut panels, &mut wall);
        assert!(!wall.palette_open(), "Escape should close palette on Wall");

        // 2. Sidebar (`wall.sidebar open`).
        assert!(wall.run("wall.sidebar open").text.starts_with("OK"));
        assert!(!panels.wants_keyboard(&wall), "sidebar must not steal terminal keys");
        let (_, shapes_sb) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_sb > 0, "sidebar should emit shapes when open");
        assert!(panels.close_open_on_escape(&mut wall));
        assert!(!wall.store().sidebar().open);

        // 3. Settings (`wall.settings open`).
        assert!(wall.run("wall.settings open").text.starts_with("OK"));
        assert!(panels.wants_keyboard(&wall));
        let (_, shapes_set) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_set > 0, "settings should emit shapes when open");
        let (verbs_set, _) = run_pass(&ctx, esc.clone(), &mut panels, &mut wall);
        assert_eq!(verbs_set, vec!["wall.settings close".to_string()]);
        assert!(!wall.store().settings_panel().open);

        // 4. Status (`wall.status open`).
        assert!(wall.run("wall.status open").text.starts_with("OK"));
        assert!(!panels.wants_keyboard(&wall), "status must not steal terminal keys");
        let (_, shapes_st) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_st > 0, "status should emit shapes when open");
        assert!(panels.close_open_on_escape(&mut wall));
        assert!(!wall.store().status_window().open);

        // 5. Right-click context menu (`Wall::context_menu` -> `Verdict::Ours`).
        let dec = wall.context_menu(200.0, 200.0, Mods::default());
        assert_eq!(dec.verdict, Verdict::Ours);
        assert!(wall.menu().is_some());
        assert!(panels.wants_keyboard(&wall));
        let (_, shapes_menu) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_menu > 0, "context menu should emit shapes when open");
        let (verbs_menu, _) = run_pass(&ctx, esc.clone(), &mut panels, &mut wall);
        assert_eq!(verbs_menu, vec!["wall.menu close".to_string()]);
        assert!(wall.menu().is_none());

        // 6. Help (`wall.help open`).
        assert!(wall.run("wall.help open").text.starts_with("OK"));
        assert!(panels.wants_keyboard(&wall));
        let (_, shapes_help) = run_pass(&ctx, Vec::new(), &mut panels, &mut wall);
        assert!(shapes_help > 0, "help should emit shapes when open");
        let (verbs_help, _) = run_pass(&ctx, esc, &mut panels, &mut wall);
        assert_eq!(verbs_help, vec!["wall.help close".to_string()]);
        assert!(!wall.store().help_panel().open);
    }
}
