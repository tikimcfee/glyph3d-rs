//! Title-bar sizing, text-floor hysteresis, terminal short titles, split
//! bar-room caps, and bar-vs-map-label switching, ported from `wall/TitleBar.js`,
//! `wall/WallNames.js`, and `TerminalWall.js` (`shortTitle`, `nameOf`, `titleOf`).
//!
//! Decides — without GPU meshes or DOM nodes — what title string each terminal
//! wears, how tall its title bar stands in world units at a given camera zoom
//! (constant screen height capped in world space so zoomed-out bars never climb
//! onto the row above), whether the whole title fits inside the bar above the
//! text pixel floor (`minTextPx`, with hysteresis), and which on-screen
//! terminals hand their names to map labels ([`crate::labels::place_labels`]).

use std::collections::{HashMap, HashSet};

use crate::camera::CameraPose;
use crate::geom::Rect;
use crate::labels::{place_labels, Candidate, LabelItem, LabelView, Placement, ScreenBox};
use crate::layout::BAR_ROOM;

/// Constant on-screen height of a title bar in CSS pixels (`TitleBar.js`).
pub const TITLE_BAR_PX: f64 = 22.0;

/// Default smallest on-screen line height (CSS pixels) to which a bar's title
/// may shrink before giving way to a map label (`TitleBar.js`, `glyph.greekPx`).
pub const MIN_TEXT_PX: f64 = 8.0;

/// Natural title text line height as a fraction of the bar's height (`TitleBar.js`).
pub const TEXT_FRACTION: f64 = 0.6;

/// Left and right horizontal padding inside a title bar, in bar heights (`TitleBar.js`).
pub const TEXT_INSET: f64 = 0.45;

/// Hysteresis multiplier above `min_text_px` required for a title that fell to
/// a map label to return into its title bar (`TitleBar.js`).
pub const FIT_HYSTERESIS: f64 = 1.1;

/// Width kept clear at a terminal title bar's right end for hover buttons
/// (agent view, hide, hold-close), measured in bar heights (`WallNames.js`).
pub const BAR_BUTTON_ROOM: f64 = 3.15;

/// Screen width (CSS pixels) below which a terminal's name switches from its
/// title bar to a map label (`WallNames.js`).
pub const MAP_LABEL_ON_PX: f64 = 140.0;

/// Screen width (CSS pixels) above which a terminal in map-label mode switches
/// back to its title bar once its name also fits (`WallNames.js`).
pub const MAP_LABEL_OFF_PX: f64 = 170.0;

/// Constant height of a map label box in CSS pixels (`MapLabels.js`).
pub const MAP_LABEL_HEIGHT_PX: f64 = 18.0;

/// Horizontal padding on each side of a map label in CSS pixels (`MapLabels.js`).
pub const MAP_LABEL_PAD_X_PX: f64 = 6.0;

/// Approximate character advance in CSS pixels for the default `12px` monospace
/// map-label font when the caller does not supply an explicit measured width.
const DEFAULT_MAP_LABEL_CHAR_PX: f64 = 7.2;

/// Idle and focused sRGB colours (`0xRRGGBB`) for a bar's strip fill and title text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BarStyle {
    pub fill_idle: u32,
    pub fill_focused: u32,
    pub text_idle: u32,
    pub text_focused: u32,
}

/// Default colour palette of a terminal's title bar (`TERMINAL_BAR_STYLE` in `TitleBar.js`).
pub const TERMINAL_BAR_STYLE: BarStyle = BarStyle {
    fill_idle: 0x1f2633,
    fill_focused: 0x16402c,
    text_idle: 0xc4ccd8,
    text_focused: 0x8ef0b8,
};

/// Quieter colour palette of a group's header strip (`HEADER_STYLE` in `GroupFrame.js`).
pub const GROUP_HEADER_STYLE: BarStyle = BarStyle {
    fill_idle: 0x121722,
    fill_focused: 0x12261d,
    text_idle: 0x8b95a7,
    text_focused: 0x8ef0b8,
};

/// Shorten a shell-set window title by stripping a leading `user@host:` prompt
/// prefix and trimming outer whitespace (`shortTitle` in `TerminalWall.js`).
pub fn short_title(title: &str) -> String {
    let stripped = strip_user_host_prefix(title).unwrap_or(title);
    stripped.trim().to_string()
}

fn strip_user_host_prefix(title: &str) -> Option<&str> {
    let at_pos = title.find('@')?;
    let user = &title[..at_pos];
    if user.is_empty() || user.chars().any(char::is_whitespace) {
        return None;
    }
    let after_at = &title[at_pos + 1..];
    let colon_pos = after_at.find(':')?;
    let host = &after_at[..colon_pos];
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    Some(&after_at[colon_pos + 1..])
}

/// Resolve a terminal's display name (`TerminalWall.nameOf`):
/// custom store name, else attached agent target, else shortened shell title,
/// else the terminal's `id`.
pub fn terminal_name(
    id: &str,
    custom_name: Option<&str>,
    attached: Option<&str>,
    window_title: Option<&str>,
) -> String {
    if let Some(name) = custom_name.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    if let Some(target) = attached.filter(|s| !s.is_empty()) {
        return target.to_string();
    }
    if let Some(raw_title) = window_title {
        let shortened = short_title(raw_title);
        if !shortened.is_empty() {
            return shortened;
        }
    }
    id.to_string()
}

/// Format what a terminal's title bar displays (`TerminalWall.titleOf`): its
/// resolved `name`, plus `" · <agent_status_word>"` when attached to a live bus agent.
pub fn bar_title(name: &str, agent_status_word: Option<&str>) -> String {
    match agent_status_word.filter(|s| !s.is_empty()) {
        Some(status) => format!("{name} · {status}"),
        None => name.to_string(),
    }
}

/// World-space height of a title bar at `zoom` (CSS px per world unit), capped
/// at `max_world_height` (`TitleBar.worldHeight`).
pub fn bar_world_height(zoom: f64, max_world_height: f64) -> f64 {
    if zoom <= 0.0 {
        max_world_height
    } else {
        (TITLE_BAR_PX / zoom).min(max_world_height)
    }
}

/// Estimate a map label's box width in CSS pixels when no measured glyph width
/// is supplied (`ceil(text_px) + 2 * PAD_X + 2` in `MapLabels._width`).
pub fn estimate_map_label_width(name: &str) -> f64 {
    ((name.chars().count() as f64) * DEFAULT_MAP_LABEL_CHAR_PX).ceil()
        + 2.0 * MAP_LABEL_PAD_X_PX
        + 2.0
}

/// Geometry and text-fit outcome produced by [`TitleBarModel::layout`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarLayout {
    /// Strip bounds sitting on the parent box's top edge (in the box's space).
    pub strip_rect: Rect,
    /// World-space height of the bar (`bar_world_height(zoom, max_world_height)`).
    pub world_height: f64,
    /// Line height applied to the title label (in the box's local units).
    pub line_height: f64,
    /// Resulting on-screen text height in CSS pixels (`0.0` when squeezed out).
    pub text_px: f64,
    /// Left X coordinate where the title text begins (`min_x + h * TEXT_INSET`).
    pub text_left: f64,
    /// Horizontal room available for the title text after insets and button reserve.
    pub available_room: f64,
    /// Whether the whole name reads above `min_text_px` (with hysteresis).
    pub name_fits: bool,
}

/// Pure layout and text-fit state of a single `TitleBar` (`wall/TitleBar.js`).
#[derive(Clone, Debug, PartialEq)]
pub struct TitleBarModel {
    max_world_height: f64,
    min_text_px: f64,
    reserve_bars: f64,
    style: BarStyle,
    name: String,
    focused: bool,
    visible: bool,
    name_visible: bool,
    fits: bool,
}

impl TitleBarModel {
    /// Create a title-bar model with explicit `max_world_height`, `min_text_px`,
    /// right-end `reserve_bars` (in bar heights), and colour `style`.
    pub fn new(
        max_world_height: f64,
        min_text_px: f64,
        reserve_bars: f64,
        style: BarStyle,
    ) -> TitleBarModel {
        TitleBarModel {
            max_world_height,
            min_text_px,
            reserve_bars,
            style,
            name: String::new(),
            focused: false,
            visible: true,
            name_visible: true,
            fits: true,
        }
    }

    /// Standard terminal title bar (`max_world_height = BAR_ROOM`,
    /// `reserve_bars = BAR_BUTTON_ROOM`, [`TERMINAL_BAR_STYLE`]).
    pub fn terminal(min_text_px: Option<f64>) -> TitleBarModel {
        TitleBarModel::new(
            BAR_ROOM,
            min_text_px.unwrap_or(MIN_TEXT_PX),
            BAR_BUTTON_ROOM,
            TERMINAL_BAR_STYLE,
        )
    }

    /// Standard group header bar (`max_world_height = BAR_ROOM`,
    /// `min_text_px = 0.0`, `reserve_bars = 0.0`, [`GROUP_HEADER_STYLE`]).
    pub fn group_header() -> TitleBarModel {
        TitleBarModel::new(BAR_ROOM, 0.0, 0.0, GROUP_HEADER_STYLE)
    }

    pub const fn max_world_height(&self) -> f64 {
        self.max_world_height
    }

    /// Cap the bar's maximum world height (`TitleBar.setMaxWorldHeight`).
    pub fn set_max_world_height(&mut self, height: f64) {
        self.max_world_height = height;
    }

    pub const fn min_text_px(&self) -> f64 {
        self.min_text_px
    }

    pub const fn reserve_bars(&self) -> f64 {
        self.reserve_bars
    }

    pub const fn style(&self) -> BarStyle {
        self.style
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    pub const fn focused(&self) -> bool {
        self.focused
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub const fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub const fn name_visible(&self) -> bool {
        self.name_visible
    }

    pub fn set_name_visible(&mut self, visible: bool) {
        self.name_visible = visible;
    }

    /// Whether the whole name fit above `min_text_px` at the last [`Self::layout`].
    pub const fn name_fits(&self) -> bool {
        self.fits
    }

    /// Active `(fill_rgb, text_rgb)` colours for the bar's current focus state.
    pub const fn active_colors(&self) -> (u32, u32) {
        if self.focused {
            (self.style.fill_focused, self.style.text_focused)
        } else {
            (self.style.fill_idle, self.style.text_idle)
        }
    }

    /// World height occupied by this bar at `zoom` (`TitleBar.worldHeight`).
    pub fn world_height(&self, zoom: f64) -> f64 {
        bar_world_height(zoom, self.max_world_height)
    }

    /// Lay the bar along the top edge (`min_x..max_x` at `max_y`) of a box whose
    /// parent Y scale is `scale_y` (`1.0` when working directly in world units),
    /// given `unit_text_width` (the rendered width of `self.name` at line height
    /// `1.0`, i.e. `label.width / label.lineHeight` in `FieldLabel`).
    pub fn layout(
        &mut self,
        min_x: f64,
        max_x: f64,
        max_y: f64,
        scale_y: f64,
        unit_text_width: f64,
        zoom: f64,
    ) -> BarLayout {
        let sy = if scale_y > 0.0 { scale_y } else { 1.0 };
        let world_h = self.world_height(zoom);
        let h = world_h / sy;
        let w = max_x - min_x;

        let natural = h * TEXT_FRACTION;
        let available_room = w - 2.0 * h * TEXT_INSET - self.reserve_bars * h;
        let full = unit_text_width * natural;
        let line = if full <= available_room {
            natural
        } else if available_room > 0.0 && full > 0.0 {
            natural * available_room / full
        } else {
            0.0
        };
        let text_px = line * sy * zoom;
        let floor_factor = if self.fits { 1.0 } else { FIT_HYSTERESIS };
        self.fits = text_px >= self.min_text_px * floor_factor;

        BarLayout {
            strip_rect: Rect {
                min_x,
                min_y: max_y,
                max_x,
                max_y: max_y + h,
            },
            world_height: world_h,
            line_height: if line > 0.0 { line } else { natural },
            text_px,
            text_left: min_x + h * TEXT_INSET,
            available_room,
            name_fits: self.fits,
        }
    }
}

/// Per-terminal input to [`WallNames::update`].
#[derive(Clone, Debug, PartialEq)]
pub struct NamedTerminal {
    pub id: String,
    /// Resolved title text (`bar_title` / `terminal_name`).
    pub name: String,
    /// Terminal body bounds in world space (without the title bar).
    pub body_rect: Rect,
    /// Width of `name` at unit line height (`1.0`), e.g. `chars * cell_aspect`.
    pub unit_text_width: f64,
    /// Optional measured width of `name`'s map label box in CSS pixels (falls
    /// back to [`estimate_map_label_width`] when `None`).
    pub map_label_width_px: Option<f64>,
}

/// Frame inputs passed to [`WallNames::update`].
#[derive(Clone, Debug)]
pub struct NamesFrame {
    pub camera: CameraPose,
    pub view_w: f64,
    pub view_h: f64,
    /// Shown terminals in canvas arrival order (`order` in `WallNames.update`).
    pub terminals: Vec<NamedTerminal>,
    pub focused_id: Option<String>,
    pub hovered_id: Option<String>,
    /// Lone fullscreen terminal whose title bar is hidden (`fullscreenSoloId`).
    pub fullscreen_solo_id: Option<String>,
    /// Whether any single-pane or group fullscreen is active (suppresses map labels).
    pub fullscreen_active: bool,
}

/// Title-bar output for one terminal from [`WallNames::update`].
#[derive(Clone, Debug, PartialEq)]
pub struct BarDecision {
    pub id: String,
    pub name: String,
    pub visible: bool,
    pub name_visible: bool,
    pub focused: bool,
    pub layout: Option<BarLayout>,
}

/// Map-label output for one terminal from [`WallNames::update`].
#[derive(Clone, Debug, PartialEq)]
pub struct MapLabelDecision {
    pub id: String,
    pub name: String,
    pub focused: bool,
    pub target: ScreenBox,
    pub placement: Option<Placement>,
}

/// Complete per-frame naming plan produced by [`WallNames::update`].
#[derive(Clone, Debug, PartialEq)]
pub struct NamesPlan {
    pub bars: Vec<BarDecision>,
    pub map_labels: Vec<MapLabelDecision>,
}

/// Stateful coordinator for terminal title bars, split bar-room caps, and map
/// labels (`wall/WallNames.js`).
#[derive(Clone, Debug)]
pub struct WallNames {
    min_text_px: f64,
    bars: HashMap<String, TitleBarModel>,
    map_mode: HashSet<String>,
    room: HashMap<String, f64>,
    prev_placements: HashMap<String, Candidate>,
}

impl Default for WallNames {
    fn default() -> WallNames {
        WallNames::new(None)
    }
}

impl WallNames {
    /// Construct a `WallNames` model using `min_text_px` (defaults to [`MIN_TEXT_PX`]).
    pub fn new(min_text_px: Option<f64>) -> WallNames {
        WallNames {
            min_text_px: min_text_px.unwrap_or(MIN_TEXT_PX),
            bars: HashMap::new(),
            map_mode: HashSet::new(),
            room: HashMap::new(),
            prev_placements: HashMap::new(),
        }
    }

    /// Register a newly arrived terminal's title bar, applying any split bar
    /// room that was recorded for `id` before it arrived (`WallNames.attach`).
    pub fn attach(&mut self, id: &str) {
        let bar = TitleBarModel::terminal(Some(self.min_text_px));
        self.bars.insert(id.to_string(), bar);
        if let Some(&saved_room) = self.room.get(id) {
            self.set_bar_room(id, Some(saved_room));
        }
    }

    /// Forget state for terminals where `is_live(id)` returns `false` (`WallNames.prune`).
    pub fn prune<F: FnMut(&str) -> bool>(&mut self, mut is_live: F) {
        let dead: Vec<String> = self
            .bars
            .keys()
            .chain(self.room.keys())
            .filter(|id| !is_live(id))
            .cloned()
            .collect();
        for id in dead {
            self.bars.remove(&id);
            self.map_mode.remove(&id);
            self.room.remove(&id);
            self.prev_placements.remove(&id);
        }
    }

    /// Set the world-height room a split pane leaves for `id`'s title bar:
    /// - `Some(0.0)`: hide the bar completely and drop `id` from map-label mode;
    /// - `Some(r)` (`r > 0.0`): cap the bar's world height to `r` and show it;
    /// - `None`: restore the standard [`BAR_ROOM`] cap (`WallNames.setBarRoom`).
    pub fn set_bar_room(&mut self, id: &str, room: Option<f64>) {
        match room {
            None => {
                self.room.remove(id);
                if let Some(bar) = self.bars.get_mut(id) {
                    bar.set_max_world_height(BAR_ROOM);
                    bar.set_visible(true);
                }
            }
            Some(r) => {
                self.room.insert(id.to_string(), r);
                if r == 0.0 {
                    self.map_mode.remove(id);
                }
                if let Some(bar) = self.bars.get_mut(id) {
                    bar.set_max_world_height(if r == 0.0 { BAR_ROOM } else { r });
                    bar.set_visible(r != 0.0);
                }
            }
        }
    }

    /// Current split bar-room override for `id`, if any.
    pub fn bar_room(&self, id: &str) -> Option<f64> {
        self.room.get(id).copied()
    }

    /// World height of `id`'s title bar at `zoom` (`0.0` when its bar is suppressed
    /// by `set_bar_room(id, Some(0.0))` or `id` is not attached; `WallNames.barHeight`).
    pub fn bar_height(&self, id: &str, zoom: f64) -> f64 {
        if self.room.get(id) == Some(&0.0) {
            return 0.0;
        }
        self.bars
            .get(id)
            .map_or(0.0, |bar| bar.world_height(zoom))
    }

    /// Whether `id` is currently displaying its name as a map label rather than in its bar.
    pub fn in_map_mode(&self, id: &str) -> bool {
        self.map_mode.contains(id)
    }

    /// Inspect `id`'s [`TitleBarModel`], if attached.
    pub fn bar(&self, id: &str) -> Option<&TitleBarModel> {
        self.bars.get(id)
    }

    /// Lay out every shown terminal's title bar for `frame`, update bar-vs-map
    /// hysteresis, and place visible map labels (`WallNames.update`).
    pub fn update(&mut self, frame: &NamesFrame) -> NamesPlan {
        let zoom = frame.camera.zoom;
        let fs_solo = frame.fullscreen_solo_id.as_deref();
        let fs_active = frame.fullscreen_active || fs_solo.is_some();

        for term in &frame.terminals {
            if !self.bars.contains_key(&term.id) {
                self.attach(&term.id);
            }
        }

        let mut layouts: HashMap<String, BarLayout> = HashMap::with_capacity(frame.terminals.len());
        for term in &frame.terminals {
            let Some(bar) = self.bars.get_mut(&term.id) else {
                continue;
            };
            if fs_solo == Some(term.id.as_str()) || self.room.get(&term.id) == Some(&0.0) {
                bar.set_visible(false);
                continue;
            }
            bar.set_visible(true);
            bar.set_name(term.name.clone());
            bar.set_focused(frame.focused_id.as_deref() == Some(term.id.as_str()));
            let layout = bar.layout(
                term.body_rect.min_x,
                term.body_rect.max_x,
                term.body_rect.max_y,
                1.0,
                term.unit_text_width,
                zoom,
            );
            layouts.insert(term.id.clone(), layout);
        }

        if fs_active {
            let mut bar_decisions = Vec::with_capacity(frame.terminals.len());
            for term in &frame.terminals {
                if fs_solo != Some(term.id.as_str()) {
                    if let Some(bar) = self.bars.get_mut(&term.id) {
                        bar.set_name_visible(true);
                    }
                }
                let bar = &self.bars[&term.id];
                bar_decisions.push(BarDecision {
                    id: term.id.clone(),
                    name: term.name.clone(),
                    visible: bar.visible(),
                    name_visible: bar.name_visible(),
                    focused: bar.focused(),
                    layout: layouts.remove(&term.id),
                });
            }
            self.prev_placements.clear();
            return NamesPlan {
                bars: bar_decisions,
                map_labels: Vec::new(),
            };
        }

        let mut boxes: HashMap<String, ScreenBox> = HashMap::new();
        let mut obstacles: Vec<ScreenBox> = Vec::new();
        for term in &frame.terminals {
            if self.room.get(&term.id) == Some(&0.0) {
                continue;
            }
            let bar_h = self.bar_height(&term.id, zoom);
            let win_rect = Rect {
                max_y: term.body_rect.max_y + bar_h,
                ..term.body_rect
            };
            let screen_box = frame.camera.to_screen_box(win_rect, frame.view_w, frame.view_h);
            if screen_box.right < 0.0
                || screen_box.left > frame.view_w
                || screen_box.bottom < 0.0
                || screen_box.top > frame.view_h
            {
                continue;
            }
            boxes.insert(term.id.clone(), screen_box);
            obstacles.push(screen_box);

            let width_px = screen_box.right - screen_box.left;
            let fits = self
                .bars
                .get(&term.id)
                .is_none_or(TitleBarModel::name_fits);
            if width_px < MAP_LABEL_ON_PX || !fits {
                self.map_mode.insert(term.id.clone());
            } else if width_px > MAP_LABEL_OFF_PX {
                self.map_mode.remove(&term.id);
            }
        }

        let rank = |id: &str| -> u8 {
            if frame.focused_id.as_deref() == Some(id) {
                0
            } else if frame.hovered_id.as_deref() == Some(id) {
                1
            } else {
                2
            }
        };

        let mut bar_decisions = Vec::with_capacity(frame.terminals.len());
        let mut raw_items: Vec<(&NamedTerminal, ScreenBox)> = Vec::new();
        for term in &frame.terminals {
            let is_map = self.map_mode.contains(&term.id);
            if let Some(bar) = self.bars.get_mut(&term.id) {
                bar.set_name_visible(!is_map);
            }
            if is_map {
                if let Some(&target) = boxes.get(&term.id) {
                    raw_items.push((term, target));
                }
            }
            let bar = &self.bars[&term.id];
            bar_decisions.push(BarDecision {
                id: term.id.clone(),
                name: term.name.clone(),
                visible: bar.visible(),
                name_visible: bar.name_visible(),
                focused: bar.focused(),
                layout: layouts.remove(&term.id),
            });
        }

        raw_items.sort_by_key(|(term, _)| rank(&term.id));

        let label_requests: Vec<LabelItem> = raw_items
            .iter()
            .map(|(term, target)| LabelItem {
                id: term.id.clone(),
                target: *target,
                w: term
                    .map_label_width_px
                    .unwrap_or_else(|| estimate_map_label_width(&term.name)),
                h: MAP_LABEL_HEIGHT_PX,
            })
            .collect();

        let view = LabelView::new(frame.view_w, frame.view_h);
        let placed = place_labels(&label_requests, view, &self.prev_placements, Some(&obstacles));

        let mut next_prev = HashMap::new();
        let mut map_labels = Vec::with_capacity(raw_items.len());
        for (term, target) in raw_items {
            let placement = placed.get(&term.id).and_then(|opt| *opt);
            if let Some(p) = placement {
                next_prev.insert(term.id.clone(), p.cand);
            }
            map_labels.push(MapLabelDecision {
                id: term.id.clone(),
                name: term.name.clone(),
                focused: frame.focused_id.as_deref() == Some(term.id.as_str()),
                target,
                placement,
            });
        }
        self.prev_placements = next_prev;

        NamesPlan {
            bars: bar_decisions,
            map_labels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_title_and_terminal_name_follow_precedence_and_strip_prompt_prefix() {
        assert_eq!(short_title("lugo@laptop-2: ~/src/glyph3d-rs"), "~/src/glyph3d-rs");
        assert_eq!(short_title("root@workstation:/var/log"), "/var/log");
        assert_eq!(short_title("  user@host:~/keep-prefix "), "user@host:~/keep-prefix");
        assert_eq!(short_title("vim Cargo.toml"), "vim Cargo.toml");
        assert_eq!(short_title(""), "");

        // Custom name > attached target > shortened shell title > terminal id.
        assert_eq!(
            terminal_name("term-1", Some(" build "), Some("helper"), Some("lugo@m2:~/x")),
            "build"
        );
        assert_eq!(
            terminal_name("term-1", Some("   "), Some("helper@m2"), Some("lugo@m2:~/x")),
            "helper@m2"
        );
        assert_eq!(
            terminal_name("term-1", None, None, Some("lugo@m2: ~/repo")),
            "~/repo"
        );
        assert_eq!(terminal_name("term-1", None, None, Some("")), "term-1");
        assert_eq!(bar_title("helper", Some("waiting")), "helper · waiting");
        assert_eq!(bar_title("term-1", None), "term-1");
    }

    #[test]
    fn title_bar_world_height_caps_at_max_world_height_and_text_floor_uses_hysteresis() {
        let mut bar = TitleBarModel::terminal(Some(8.0));
        // Zoomed in (zoom = 10 px/wu): 22 / 10 = 2.2 wu (< BAR_ROOM = 9.6).
        assert!((bar.world_height(10.0) - 2.2).abs() < 1e-9);
        // Zoomed far out (zoom = 1 px/wu): 22 / 1 = 22 -> capped at BAR_ROOM (9.6).
        assert_eq!(bar.world_height(1.0), BAR_ROOM);

        // Long title ("long-title..." with unit_text_width = 30) in a 50-wide box at zoom = 2:
        // world_height = min(11, 9.6) = 9.6, natural = 5.76, room = 50 - (0.9 + 3.15)*9.6 = 11.12,
        // full = 30 * 5.76 = 172.8 -> line = 5.76 * 11.12 / 172.8 = 0.37067 -> text_px = 0.74 px < 8 px.
        let shrunk = bar.layout(0.0, 50.0, 0.0, 1.0, 30.0, 2.0);
        assert!(!shrunk.name_fits);
        assert!(!bar.name_fits());

        // Once `_fits` is false, coming back requires >= 8.0 * 1.1 = 8.8 px (FIT_HYSTERESIS).
        // Pick unit_text_width = 1.0, width = 100.0, zoom chosen so natural text_px = 8.4 px:
        // At zoom = 1.4583333..., world_height = 9.6, natural = 5.76, text_px = 5.76 * zoom = 8.4 (< 8.8).
        let mid_zoom = 8.4 / (BAR_ROOM * TEXT_FRACTION);
        let held_out = bar.layout(0.0, 100.0, 0.0, 1.0, 1.0, mid_zoom);
        assert!((held_out.text_px - 8.4).abs() < 1e-6);
        assert!(!held_out.name_fits);

        // Above 8.8 px (e.g. 9.0 px), the title returns to the bar (`name_fits = true`).
        let recovery_zoom = 9.0 / (BAR_ROOM * TEXT_FRACTION);
        let recovered = bar.layout(0.0, 100.0, 0.0, 1.0, 1.0, recovery_zoom);
        assert!(recovered.name_fits);

        // And while `name_fits` is true, 8.4 px (>= 8.0) stays fitting!
        let stays_in = bar.layout(0.0, 100.0, 0.0, 1.0, 1.0, mid_zoom);
        assert!(stays_in.name_fits);
    }

    #[test]
    fn wall_names_switches_narrow_or_shrunk_bars_to_map_labels_with_width_hysteresis() {
        let mut names = WallNames::new(Some(8.0));
        let make_frame = |zoom: f64, width_world: f64, focused: Option<&str>, hovered: Option<&str>| NamesFrame {
            camera: CameraPose::new(0.0, 0.0, zoom),
            view_w: 1200.0,
            view_h: 800.0,
            terminals: vec![
                NamedTerminal {
                    id: "a".to_string(),
                    name: "alpha".to_string(),
                    body_rect: Rect::from_top_left(-50.0, 20.0, width_world, 40.0),
                    unit_text_width: 3.0,
                    map_label_width_px: Some(60.0),
                },
                NamedTerminal {
                    id: "b".to_string(),
                    name: "bravo".to_string(),
                    body_rect: Rect::from_top_left(80.0, 20.0, width_world, 40.0),
                    unit_text_width: 3.0,
                    map_label_width_px: Some(60.0),
                },
            ],
            focused_id: focused.map(str::to_owned),
            hovered_id: hovered.map(str::to_owned),
            fullscreen_solo_id: None,
            fullscreen_active: false,
        };

        // Screen width = 60 * 2.0 = 120 px (< MAP_LABEL_ON_PX = 140) -> enters map label mode.
        // Focused 'b' ranks ahead of hovered/idle 'a'.
        let plan1 = names.update(&make_frame(2.0, 60.0, Some("b"), Some("a")));
        assert!(names.in_map_mode("a"));
        assert!(names.in_map_mode("b"));
        assert!(!plan1.bars[0].name_visible);
        assert_eq!(plan1.map_labels.len(), 2);
        assert_eq!(plan1.map_labels[0].id, "b");
        assert_eq!(plan1.map_labels[1].id, "a");
        assert!(plan1.map_labels[0].placement.is_some());

        // Screen width = 77.5 * 2.0 = 155 px (inside 140..=170 hysteresis band) -> stays in map mode!
        let plan2 = names.update(&make_frame(2.0, 77.5, None, None));
        assert_eq!(plan2.map_labels.len(), 2);

        // Screen width = 90 * 2.0 = 180 px (> MAP_LABEL_OFF_PX = 170, and text fits) -> returns to bar!
        let plan3 = names.update(&make_frame(2.0, 90.0, None, None));
        assert!(plan3.map_labels.is_empty());
        assert!(plan3.bars[0].name_visible);
    }

    #[test]
    fn split_bar_room_and_fullscreen_suppress_bars_and_map_labels() {
        let mut names = WallNames::new(None);
        // Split sets room BEFORE attach: attach picks it up.
        names.set_bar_room("pane-1", Some(3.0));
        names.attach("pane-1");
        assert_eq!(names.bar_height("pane-1", 1.0), 3.0);

        // `Some(0.0)` hides the bar completely and skips map-label generation.
        names.set_bar_room("pane-1", Some(0.0));
        assert_eq!(names.bar_height("pane-1", 10.0), 0.0);

        let plan = names.update(&NamesFrame {
            camera: CameraPose::new(0.0, 0.0, 2.0),
            view_w: 1000.0,
            view_h: 800.0,
            terminals: vec![NamedTerminal {
                id: "pane-1".to_string(),
                name: "pane-1".to_string(),
                body_rect: Rect::from_top_left(0.0, 0.0, 40.0, 20.0),
                unit_text_width: 4.0,
                map_label_width_px: Some(50.0),
            }],
            focused_id: None,
            hovered_id: None,
            fullscreen_solo_id: None,
            fullscreen_active: false,
        });
        assert!(!plan.bars[0].visible);
        assert!(plan.map_labels.is_empty());

        // Restoring `None` gives back BAR_ROOM; fullscreen suppresses map labels and solo's bar.
        names.set_bar_room("pane-1", None);
        assert_eq!(names.bar_height("pane-1", 1.0), BAR_ROOM);
        let fs_plan = names.update(&NamesFrame {
            camera: CameraPose::new(0.0, 0.0, 2.0),
            view_w: 1000.0,
            view_h: 800.0,
            terminals: vec![NamedTerminal {
                id: "pane-1".to_string(),
                name: "pane-1".to_string(),
                body_rect: Rect::from_top_left(0.0, 0.0, 40.0, 20.0),
                unit_text_width: 4.0,
                map_label_width_px: Some(50.0),
            }],
            focused_id: Some("pane-1".to_string()),
            hovered_id: None,
            fullscreen_solo_id: Some("pane-1".to_string()),
            fullscreen_active: true,
        });
        assert!(!fs_plan.bars[0].visible);
        assert!(fs_plan.map_labels.is_empty());
    }
}
