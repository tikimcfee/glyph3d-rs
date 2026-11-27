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
//! [`Wall`] owns the open and placement state of all panels; [`Panels`] owns
//! only each widget's transient UI view state (queries, active row, drafts,
//! drag offsets, hold-to-close progress, and cached help sections) and
//! intercepts `wall.palette` verbs (not registered in `wall_core::commands`).

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
use wall_core::status::{TermdStatus, chip_look, notice_for};
use wall_core::wall::{Wall, WallFrame, help_sections};

use super::panels::{
    self, bar_buttons, context_menu, help, palette, settings, status, topbar,
};

const PALETTE_INPUT_ID: &str = "wall_palette_query_input";
const SETTINGS_SEARCH_ID: &str = "wall_settings_search_input";

/// Owns every mounted panel's transient `egui` view state across frames.
#[derive(Debug, Default)]
pub struct Panels {
    pub topbar: topbar::TopbarState,
    pub bar_buttons: bar_buttons::BarButtonsState,
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
    pub fn any_open(&self, wall: &Wall, described: &WallFrame) -> bool {
        if described.fullscreen_backdrop.is_none() {
            // The topbar and `termd` chip are always visible on the canvas outside fullscreen.
            return true;
        }
        let now_ms = epoch_now_ms();
        let restart_ms = wall.config().restart_notice_ms.max(0.0).round() as u64;
        self.bar_buttons.is_holding()
            || (!self.wall_dragging && described.title_bars.iter().any(|b| b.hovered))
            || described.palette_open
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

    /// Draw the topbar, hovered title-bar buttons, and every open panel into
    /// `ctx`, intercepting `wall.palette` locally and returning the remaining
    /// `wall.*` / `terminal.*` verb lines to run after the pass.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        wall: &Wall,
        described: &WallFrame,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let platform = Platform::host();
        let viewport = panels::safe_viewport_rect(ctx);
        let fullscreen = described.fullscreen_backdrop.is_some();

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

        // 0a. Hovered terminal title-bar buttons (`panels::bar_buttons`).
        let bar_target_id: Option<String> = self
            .bar_buttons
            .hold_target()
            .map(str::to_owned)
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
            },
            &mut out,
        );
        if let Some(delay) = bar_out.repaint_after {
            ctx.request_repaint_after(delay);
        }

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
                inset: described.panels.dock.inset,
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
        let status_window_or_notice = described.panels.status_window.open
            || status_has_notice(wall.termd(), now_ms, restart_ms);

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
                chip_pos,
                chrome_inset: (
                    described.panels.dock.inset.left as f32,
                    described.panels.dock.inset.right as f32,
                ),
                platform,
            };
            status::show(ctx, &mut self.status, &input, &mut out);
        }
        self.status_was_active = status_window_or_notice;

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

        // Intercept `wall.palette [toggle|open|close]` locally (not in `wall_core::commands`).
        out.retain(|line| {
            let mut words = line.split_whitespace();
            if words.next() != Some("wall.palette") {
                return true;
            }
            let cur = self
                .pending_palette_open
                .unwrap_or(described.palette_open || self.palette.open);
            let next = match words.next() {
                Some("open") => true,
                Some("close") => false,
                Some("toggle") | None => !cur,
                Some(_) => return false,
            };
            if next {
                self.palette.open_find();
            } else {
                self.palette.close();
            }
            self.was_palette_open = next;
            self.pending_palette_open = Some(next);
            false
        });

        self.palette_search_had_focus =
            self.palette.open && ctx.memory(|m| m.has_focus(egui::Id::new(PALETTE_INPUT_ID)));
        self.settings_search_had_focus =
            settings_open && ctx.memory(|m| m.has_focus(egui::Id::new(SETTINGS_SEARCH_ID)));

        out
    }

    /// Synchronize [`Wall`] state after the `egui` pass (`palette_open` from
    /// intercepted `wall.palette` or self-close, plus `field_focus`). Returns
    /// `true` if [`Wall`] mutated and effects should be applied.
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
    }
}
