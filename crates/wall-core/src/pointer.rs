//! Pointer gesture state machine for the wall canvas, ported from `wall/WallPointer.js`
//! (with `wall/WallMouse.js` and `wall/chord.js` modifier helpers embedded behind local
//! seams until `mouse.rs` and `chord.rs` land).
//!
//! Decides what every press, drag, release, cancel, double-click, modifier toggle, and
//! wheel event on the canvas means without touching a window system or DOM:
//! - **Program mouse reporting**: when the *focused* terminal has DECSET mouse tracking
//!   enabled (`9` / `1000` / `1002` / `1003`), presses on its body (without `Shift`, and
//!   without `Alt` on the left button) belong to the terminal program, reporting chorded
//!   buttons, once-per-cell drags clamped to the grid's edge, release/cancel cleanup, and
//!   mode-`1003` hover moves.
//! - **Canvas & window gestures**: empty-canvas click blurs focus and clears the wall
//!   selection while drag pans; middle-click on a terminal body pastes the selection
//!   clipboard while middle-drag pans; `Shift`-drag on empty canvas (or starting on a
//!   title bar / border) lassoes terminals while `Shift`-click toggles a terminal and
//!   `Shift`-drag on a body selects text; title-bar drag, `Alt`-drag, and map-label drag
//!   move terminals (dropping into tiles when tiled); border drag resizes in whole cells;
//!   group headers (and `Alt`-drag on split panes) move whole groups; split dividers,
//!   outer split borders, and split-pane title bars rearrange splits.
//! - **Text selection**: body presses focus the terminal and count clicks within
//!   `click_repeat_ms` and [`DRAG_SLOP_PX`] (`1` = cell stream, `2` = word, `3` = line,
//!   `4` wraps to `1`), supporting platform block-selection modifiers (`⌘⌥` on macOS,
//!   `Ctrl+Alt` / `Super+Alt` elsewhere).
//! - **Drafting-grid snapping**: moves, border resizes, group drags, and outer split
//!   resizes snap to [`crate::snap`] when `snap` is enabled, live-inverted whenever
//!   `Ctrl` goes down or up mid-drag (never for tiled members).
//! - **Wheel routing**: gated through [`WheelStreamGate`] (`quench_wheel` across
//!   fullscreen/framing jumps), zooming with `zoom_keys` (direct pinch below
//!   [`PINCH_MAX_DELTA`], eased notch at or above), scrolling the focused terminal's
//!   history via [`TerminalScroller`], or panning the canvas.
//!
//! Everything the state machine asks of or does to the wall goes through [`PointerHost`].

use std::ops::{BitOr, BitOrAssign};

use crate::geom::{Point, Rect};
use crate::input::{Button, KeyEvent, Mods, Platform};
use crate::layout::{
    edge_cursor, lasso_touches, resize_from_drag, union_rect, Edges, GridSize, Hit, HitPart,
    MinGridSize, ResizeCursor,
};
use crate::scroll::{
    AcceptOptions, CanvasWheelTarget, DeltaMode, TerminalScroller, WheelOptions, WheelStreamGate,
};
use crate::snap::{snap_move, snap_resize, snap_step, snap_to, GridSpec};
use crate::split::{Axis, BranchSide, SplitPath};

/// Pointer travel in screen (CSS/logical) pixels required before a press becomes a drag
/// rather than a click (`DRAG_SLOP_PX` in `WallPointer.js`).
pub const DRAG_SLOP_PX: f64 = 4.0;

/// Minimum pointer travel in screen pixels before a lasso appends another polygon vertex.
pub const LASSO_STEP_PX: f64 = 3.0;

/// Smallest terminal dimensions (columns × rows) a border resize drag may produce.
pub const MIN_SIZE: MinGridSize = MinGridSize::new(20, 5);

/// Screen pixels per line used to scale [`DeltaMode::Line`] wheel events (`LINE_PX`).
pub const LINE_PX: f64 = 16.0;

/// Vertical zoom-wheel pixel delta threshold separating a continuous trackpad pinch
/// (`|dy| < 50`, applied directly) from a discrete mouse wheel notch (`|dy| >= 50`, eased).
pub const PINCH_MAX_DELTA: f64 = 50.0;

const CLICK_MODES: [SelectionMode; 3] = [
    SelectionMode::Cell,
    SelectionMode::Word,
    SelectionMode::Line,
];

// ── Input event & button mask types ─────────────────────────────────────────

/// Bitmask of currently depressed pointer buttons, matching W3C `MouseEvent.buttons`
/// (`1` = left, `2` = right, `4` = middle, `8` = back, `16` = forward).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Buttons(pub u8);

impl Buttons {
    pub const NONE: Buttons = Buttons(0);
    pub const LEFT: Buttons = Buttons(1);
    pub const RIGHT: Buttons = Buttons(2);
    pub const MIDDLE: Buttons = Buttons(4);
    pub const BACK: Buttons = Buttons(8);
    pub const FORWARD: Buttons = Buttons(16);

    /// Single-button mask corresponding to `button`.
    pub const fn from_button(button: Button) -> Buttons {
        match button {
            Button::Left => Buttons::LEFT,
            Button::Middle => Buttons::MIDDLE,
            Button::Right => Buttons::RIGHT,
            Button::Back => Buttons::BACK,
            Button::Forward => Buttons::FORWARD,
        }
    }

    /// Whether no buttons are held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether `button`'s bit is set in `self`.
    pub const fn contains(self, button: Button) -> bool {
        let bit = Buttons::from_button(button).0;
        (self.0 & bit) != 0
    }
}

impl BitOr for Buttons {
    type Output = Buttons;

    fn bitor(self, rhs: Buttons) -> Buttons {
        Buttons(self.0 | rhs.0)
    }
}

impl BitOrAssign for Buttons {
    fn bitor_assign(&mut self, rhs: Buttons) {
        self.0 |= rhs.0;
    }
}

/// Plain-data pointer event (press, move, release, cancel, or double-click) in logical
/// screen pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    /// Pointer stream identifier (`PointerEvent.pointerId`).
    pub pointer_id: i32,
    /// Client X coordinate in logical screen pixels.
    pub x: f64,
    /// Client Y coordinate in logical screen pixels.
    pub y: f64,
    /// Button that initiated a press/release/double-click (`None` for hover or unmapped buttons).
    pub button: Option<Button>,
    /// Bitmask of all buttons currently held (`MouseEvent.buttons`).
    pub buttons: Buttons,
    /// Active keyboard modifiers.
    pub mods: Mods,
    /// Event timestamp in milliseconds (`e.timeStamp`).
    pub time_ms: f64,
}

impl Default for PointerEvent {
    fn default() -> Self {
        Self {
            pointer_id: 1,
            x: 0.0,
            y: 0.0,
            button: Some(Button::Left),
            buttons: Buttons::NONE,
            mods: Mods::default(),
            time_ms: 0.0,
        }
    }
}

/// Plain-data wheel event on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelEvent {
    /// Client X coordinate in logical screen pixels.
    pub x: f64,
    /// Client Y coordinate in logical screen pixels.
    pub y: f64,
    /// Raw horizontal delta before `delta_mode` scaling (`e.deltaX`).
    pub delta_x: f64,
    /// Raw vertical delta before `delta_mode` scaling (`e.deltaY`).
    pub delta_y: f64,
    /// Unit of `delta_x` and `delta_y`.
    pub delta_mode: DeltaMode,
    /// Active keyboard modifiers.
    pub mods: Mods,
    /// Event timestamp in milliseconds.
    pub time_ms: f64,
}

impl Default for WheelEvent {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            delta_x: 0.0,
            delta_y: 0.0,
            delta_mode: DeltaMode::Pixel,
            mods: Mods::default(),
            time_ms: 0.0,
        }
    }
}

// ── Chord & mouse seams (until chord.rs and mouse.rs land) ──────────────────

/// Modifiers that turn a wheel gesture into a camera zoom (`zoomKeys` in `wallConfig.js`,
/// `parseZoomKeys` / `zoomsWheel` in `chord.js`).
///
/// Local seam in `pointer.rs` until `chord.rs` lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ZoomKeys {
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
    pub shift: bool,
}

impl ZoomKeys {
    /// Default wall `zoomKeys` (`"ctrl alt meta"`).
    pub const DEFAULT: ZoomKeys = ZoomKeys {
        ctrl: true,
        alt: true,
        meta: true,
        shift: false,
    };

    /// Parse a whitespace-separated `zoomKeys` setting string (`ctrl`, `alt`, `meta`, `shift`).
    pub fn parse(spec: &str) -> Result<ZoomKeys, String> {
        let parsed = crate::chord::parse_zoom_keys(spec)?;
        let mut keys = ZoomKeys {
            ctrl: false,
            alt: false,
            meta: false,
            shift: false,
        };
        for key in parsed {
            match key {
                crate::chord::ZoomKey::Ctrl => keys.ctrl = true,
                crate::chord::ZoomKey::Alt => keys.alt = true,
                crate::chord::ZoomKey::Meta => keys.meta = true,
                crate::chord::ZoomKey::Shift => keys.shift = true,
            }
        }
        Ok(keys)
    }

    /// Whether `mods` carries at least one configured zoom modifier (`chord.zoomsWheel`).
    pub const fn matches(self, mods: Mods) -> bool {
        (self.ctrl && mods.ctrl)
            || (self.alt && mods.alt)
            || (self.meta && mods.meta)
            || (self.shift && mods.shift)
    }
}

impl Default for ZoomKeys {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Whether `mods` inverts grid snapping during an active move, resize, or group drag
/// (unified with [`crate::chord::inverts_snap`]).
pub use crate::chord::inverts_snap;

/// Whether `mods` requests rectangular column-block text selection on `platform`
/// (delegates to [`crate::chord::block_selects`]).
pub const fn block_selects(mods: Mods, platform: Platform) -> bool {
    crate::chord::block_selects(platform, mods)
}

/// Terminal program DECSET mouse-reporting mode (unified with [`crate::mouse::MouseTracking`]).
pub use crate::mouse::MouseTracking;

/// Action reported to a mouse-tracking terminal program (unified with [`crate::mouse::MouseAction`]).
pub use crate::mouse::MouseAction;

/// Button identifier in a [`TerminalMouseEvent`] (`'left'`, `'middle'`, `'right'`, `8`,
/// `9`, or `'none'` for buttonless hover motion).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProgramMouseButton {
    Left,
    Middle,
    Right,
    /// Back button (`8` in X11 / termd).
    Back,
    /// Forward button (`9` in X11 / termd).
    Forward,
    /// No button held (hover move under `MouseTracking::Any`).
    None,
}

impl ProgramMouseButton {
    /// Map a pointer [`Button`] to its program mouse button (`WallMouse.buttonOf`).
    pub const fn from_input(button: Button) -> ProgramMouseButton {
        match button {
            Button::Left => ProgramMouseButton::Left,
            Button::Middle => ProgramMouseButton::Middle,
            Button::Right => ProgramMouseButton::Right,
            Button::Back => ProgramMouseButton::Back,
            Button::Forward => ProgramMouseButton::Forward,
        }
    }
}

/// Mouse event forwarded to a focused terminal whose program enabled DECSET mouse modes.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalMouseEvent {
    pub action: MouseAction,
    pub button: ProgramMouseButton,
    /// Fractional column coordinate in `[0, cols]`.
    pub col: f64,
    /// Fractional row coordinate in `[0, rows]`.
    pub row: f64,
    /// Held modifiers (`WallMouse.modsOf`).
    pub mods: Mods,
}

impl TerminalMouseEvent {
    /// W3C modifier names in `WallMouse.modsOf` order (`Shift`, `Control`, `Alt`, `Meta`).
    pub fn mod_names(&self) -> Vec<&'static str> {
        mods_of(self.mods)
    }
}

/// W3C modifier names in `WallMouse.modsOf` order (unified with [`crate::mouse::mods_of`]).
pub use crate::mouse::mods_of;

/// Primary button reported for a motion event with `held` buttons depressed, in xterm
/// priority order (`left`, `middle`, `right`, `8`, `9`, else `'none'`; `WallMouse.heldButton`).
pub const fn held_button(held: Buttons) -> ProgramMouseButton {
    const ORDER: [(Buttons, ProgramMouseButton); 5] = [
        (Buttons::LEFT, ProgramMouseButton::Left),
        (Buttons::MIDDLE, ProgramMouseButton::Middle),
        (Buttons::RIGHT, ProgramMouseButton::Right),
        (Buttons::BACK, ProgramMouseButton::Back),
        (Buttons::FORWARD, ProgramMouseButton::Forward),
    ];
    let mut i = 0;
    while i < ORDER.len() {
        if (held.0 & ORDER[i].0 .0) != 0 {
            return ORDER[i].1;
        }
        i += 1;
    }
    ProgramMouseButton::None
}

/// Whether a pointer press belongs to the terminal's running program rather than the wall
/// (`WallMouse.programGets`): must land on the *focused* terminal's `Body`, with mouse
/// tracking enabled, a recognized button, no `Shift`, and no `Alt` on the left button.
pub const fn program_gets(
    button: Option<Button>,
    mods: Mods,
    part: HitPart,
    focused: bool,
    tracking: MouseTracking,
) -> bool {
    let Some(btn) = button else {
        return false;
    };
    matches!(part, HitPart::Body)
        && focused
        && !matches!(tracking, MouseTracking::None)
        && !mods.shift
        && !(mods.alt && matches!(btn, Button::Left))
}

// ── Host surface types & PointerHost trait ──────────────────────────────────

/// Configuration knobs read by [`WallPointer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerConfig {
    /// Platform conventions (`Platform::Mac` vs `Platform::Other`).
    pub platform: Platform,
    /// Whether moves, resizes, and group drags snap to the grid by default (`Ctrl` inverts).
    pub snap: bool,
    /// Drafting-grid spacing and visibility floor.
    pub grid: GridSpec,
    /// Maximum gap in milliseconds between clicks in a double/triple-click sequence.
    pub click_repeat_ms: f64,
    /// Modifiers that make a wheel event zoom the camera instead of panning/scrolling.
    pub zoom_keys: ZoomKeys,
    /// Terminal wheel scroll multiplier (`scrollMultiplier`).
    pub scroll_multiplier: f64,
    /// Whether discrete mouse wheel notches glide across `step(dt)` frames (`wall._running`).
    pub glide_scroll: bool,
}

impl Default for PointerConfig {
    fn default() -> Self {
        Self {
            platform: Platform::host(),
            snap: true,
            grid: GridSpec::new(4.0, 5, 6.0),
            click_repeat_ms: 500.0,
            zoom_keys: ZoomKeys::DEFAULT,
            scroll_multiplier: 1.0,
            glide_scroll: true,
        }
    }
}

/// Fractional and integer cell coordinates of a point over a terminal's cell grid
/// (unified with [`crate::mouse::CellPoint`]).
pub use crate::mouse::CellPoint;

/// Convert local terminal coordinates `(local_x, local_y)` (where cell `(c, r)` has its
/// glyph slot at `(c * stride.x, -r * stride.y)`, left edge and vertical centre) into a
/// clamped [`CellPoint`], matching `GridBackground.cellAtPoint`.
pub fn cell_at_point(cols: u32, rows: u32, stride: Point, local_x: f64, local_y: f64) -> CellPoint {
    if cols == 0 || rows == 0 {
        return CellPoint::default();
    }
    let cols_f = f64::from(cols);
    let rows_f = f64::from(rows);
    let x = (local_x / stride.x).clamp(0.0, cols_f);
    let y = ((stride.y * 0.5 - local_y) / stride.y).clamp(0.0, rows_f);
    let col = (x.floor().max(0.0) as u32).min(cols - 1);
    let row = (y.floor().max(0.0) as u32).min(rows - 1);
    CellPoint { x, y, col, row }
}

/// Granularity of a terminal text selection drag (unified with [`crate::selection::SelectionMode`]).
pub use crate::selection::SelectionMode;

/// Hit-test target under a world point returned by [`PointerHost::hit`].
#[derive(Clone, Debug, PartialEq)]
pub enum CanvasHit {
    /// A terminal window's body, title bar, or resize border.
    Terminal { id: String, part: HitPart },
    /// A group's header bar.
    GroupHeader { group: String },
    /// A split group's inner divider band.
    Divider {
        group: String,
        path: SplitPath,
        axis: Axis,
    },
}

impl From<Hit> for CanvasHit {
    fn from(hit: Hit) -> Self {
        CanvasHit::Terminal {
            id: hit.id,
            part: hit.part,
        }
    }
}

/// Layout mode of a terminal group (unified with [`crate::config::GroupLayout`]).
pub use crate::config::GroupLayout as GroupLayoutKind;

/// Active fullscreen state inspected by [`WallPointer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FullscreenState {
    /// A single terminal occupies fullscreen (`fs.mode !== 'group'`).
    Single { id: String },
    /// A group occupies fullscreen (`fs.mode === 'group'`).
    Group {
        group_layout: GroupLayoutKind,
        elevated_ids: Vec<String>,
    },
}

impl FullscreenState {
    fn contains(&self, terminal_id: &str) -> bool {
        match self {
            FullscreenState::Single { id } => id == terminal_id,
            FullscreenState::Group { elevated_ids, .. } => {
                elevated_ids.iter().any(|member| member == terminal_id)
            }
        }
    }
}

/// Classification of a grabbed border edge on a split-group pane (`WallGroups.splitEdge`).
#[derive(Clone, Debug, PartialEq)]
pub enum SplitEdge {
    /// An inner edge adjacent to a split divider.
    Divider {
        group: String,
        path: SplitPath,
        axis: Axis,
    },
    /// An outer boundary edge that resizes the entire split rectangle.
    Outer { group: String, edges: Edges },
}

/// Visual drop-target preview while dragging a tiled member or split pane (`pointer.drop`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropPreview {
    pub rect: Rect,
    pub ok: bool,
}

/// Pointer cursor icon selected by [`WallPointer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CursorIcon {
    #[default]
    Default,
    Text,
    Pointer,
    Move,
    Grab,
    Crosshair,
    Copy,
    ColResize,
    RowResize,
    Resize(ResizeCursor),
}

impl CursorIcon {
    /// CSS cursor keyword matching `WallPointer._hover`.
    pub const fn as_css(self) -> &'static str {
        match self {
            CursorIcon::Default => "default",
            CursorIcon::Text => "text",
            CursorIcon::Pointer => "pointer",
            CursorIcon::Move => "move",
            CursorIcon::Grab => "grab",
            CursorIcon::Crosshair => "crosshair",
            CursorIcon::Copy => "copy",
            CursorIcon::ColResize => "col-resize",
            CursorIcon::RowResize => "row-resize",
            CursorIcon::Resize(edge) => edge.as_css(),
        }
    }
}

/// Kind of pointer gesture currently in hand (`pointer._drag.kind` in `WallPointer.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GestureKind {
    Program,
    FsPaste,
    Pan,
    Lasso,
    Shift,
    Text,
    Press,
    Resize,
    Group,
    Pane,
    Divider,
    SplitResize,
}

/// Everything [`WallPointer`] asks of or does to the wall, camera, terminals, groups, and
/// command router.
pub trait PointerHost {
    /// Current pointer, grid-snap, wheel, and platform configuration.
    fn config(&self) -> PointerConfig;

    /// Camera world centre `(x, y)`.
    fn camera_pos(&self) -> Point;

    /// Camera zoom in logical screen pixels per world unit.
    fn camera_zoom(&self) -> f64;

    /// Viewport height in logical screen pixels (used to scale [`DeltaMode::Page`] wheels).
    fn viewport_height(&self) -> f64 {
        600.0
    }

    /// Project logical screen coordinates `(screen_x, screen_y)` into world space (y up).
    fn to_world(&self, screen_x: f64, screen_y: f64) -> Point;

    /// Currently focused terminal ID, if any.
    fn focused_id(&self) -> Option<&str>;

    /// Active fullscreen mode, or `None` when on the open canvas.
    fn fullscreen(&self) -> Option<FullscreenState> {
        None
    }

    /// Front-to-back hit test at `world`.
    fn hit(&self, world: Point) -> Option<CanvasHit>;

    /// Whether terminal `id` exists.
    fn has_terminal(&self, id: &str) -> bool;

    /// Whether terminal `id` exists and is not hidden.
    fn is_shown(&self, id: &str) -> bool {
        self.has_terminal(id)
    }

    /// World position `(x, y)` of terminal `id`'s origin.
    fn terminal_position(&self, id: &str) -> Option<Point>;

    /// Cell grid dimensions `(cols, rows)` of terminal `id`.
    fn terminal_size(&self, id: &str) -> Option<GridSize>;

    /// World-space cell stride `(stride_x, stride_y)` of terminal `id`.
    fn terminal_stride(&self, id: &str) -> Option<Point>;

    /// World-space body rectangle of terminal `id`.
    fn terminal_rect(&self, id: &str) -> Option<Rect>;

    /// World-space full window rectangle (body + title bar) of terminal `id` for lassoing.
    fn window_rect(&self, id: &str) -> Option<Rect> {
        self.terminal_rect(id)
    }

    /// Shown terminals on the open canvas (for lasso selection).
    fn canvas_order(&self) -> Vec<String> {
        Vec::new()
    }

    /// Resolve screen point `(screen_x, screen_y)` into terminal `id`'s clamped cell coordinates.
    fn cell_at(&self, id: &str, screen_x: f64, screen_y: f64) -> Option<CellPoint>;

    /// DECSET mouse-tracking mode currently active on terminal `id`.
    fn mouse_tracking(&self, _id: &str) -> MouseTracking {
        MouseTracking::None
    }

    /// Whether terminal `id` currently holds a non-empty text selection.
    fn has_selection(&self, _id: &str) -> bool {
        false
    }

    /// Whether the wall's selection clipboard currently holds text for middle-click paste.
    fn has_selection_clipboard(&self) -> bool {
        false
    }

    /// Whether group `group_id` exists in the store.
    fn has_group(&self, _group_id: &str) -> bool {
        false
    }

    /// Group ID containing terminal `id`, if any.
    fn group_of(&self, _id: &str) -> Option<String> {
        None
    }

    /// Whether terminal `id` belongs to a `tile`-layout group.
    fn is_tiled_member(&self, _id: &str) -> bool {
        false
    }

    /// Whether terminal `id` belongs to a `split`-layout group.
    fn is_split_member(&self, _id: &str) -> bool {
        false
    }

    /// Shown member IDs of `group_id` in tiling order.
    fn shown_group_members(&self, _group_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// Classify a border edge grab on split-group member `id` as inner divider vs outer border.
    fn split_edge(&self, _id: &str, _edges: Edges) -> Option<SplitEdge> {
        None
    }

    /// Current outer world rectangle of split group `group_id`.
    fn split_rect(&self, _group_id: &str) -> Option<Rect> {
        None
    }

    // ── Mutations & side effects ────────────────────────────────────────────

    /// Acquire pointer capture for `pointer_id`.
    fn set_pointer_capture(&mut self, _pointer_id: i32) {}

    /// Release pointer capture for `pointer_id`.
    fn release_pointer_capture(&mut self, _pointer_id: i32) {}

    /// Clear any in-flight camera ease target (`camera.target = null`).
    fn cancel_camera_ease(&mut self) {}

    /// Move the camera centre directly to world `(x, y)` during a drag pan.
    fn camera_move_to(&mut self, _x: f64, _y: f64) {}

    /// Pan the camera by `(dx_px, dy_px)` screen pixels from a plain wheel gesture.
    fn camera_pan_by(&mut self, _dx_px: f64, _dy_px: f64) {}

    /// Zoom the camera continuously around `(screen_x, screen_y)` by `factor` (trackpad pinch).
    fn camera_zoom_at(&mut self, _screen_x: f64, _screen_y: f64, _factor: f64) {}

    /// Step eased mouse-wheel zoom around `(screen_x, screen_y)` for pixel delta `dy_px`.
    fn camera_wheel_zoom_at(&mut self, _screen_x: f64, _screen_y: f64, _dy_px: f64) {}

    /// Frame every shown terminal on the canvas (`wall.fitAll()`, empty-canvas double-click).
    fn fit_all(&mut self) {}

    /// Focus terminal `id` (`None` blurs focus); `frame` requests camera framing when `true`.
    fn focus(&mut self, _id: Option<&str>, _frame: bool) {}

    /// Update the hovered terminal ID for border highlighting.
    fn set_hovered(&mut self, _id: Option<&str>) {}

    /// Update the pointer cursor icon.
    fn set_cursor(&mut self, _cursor: CursorIcon) {}

    /// Raise terminal `id` to the front of the Z-stack.
    fn bring_to_front(&mut self, _id: &str) {}

    /// Raise all members of `group_id` to the front of the Z-stack.
    fn bring_group_to_front(&mut self, _group_id: &str) {}

    /// Set terminal `id`'s world position.
    fn set_terminal_position(&mut self, _id: &str, _pos: Point) {}

    /// Persist terminal `id`'s settled position into the store.
    fn remember_position(&mut self, _id: &str) {}

    /// Clear terminal `id`'s text selection.
    fn clear_selection(&mut self, _id: &str) {}

    /// Begin a text selection in terminal `id` at fractional cell `(cell_x, cell_y)`.
    fn select_start(
        &mut self,
        _id: &str,
        _cell_x: f64,
        _cell_y: f64,
        _mode: SelectionMode,
        _block: bool,
    ) {
    }

    /// Extend terminal `id`'s active text selection to fractional cell `(cell_x, cell_y)`.
    fn select_extend(&mut self, _id: &str, _cell_x: f64, _cell_y: f64) {}

    /// Notify the wall that a non-empty text selection finished (`copyOnSelect`).
    fn selection_made(&mut self, _id: &str) {}

    /// Update or clear the screen-space lasso polygon overlay (`None` clears).
    fn set_lasso(&mut self, _points: Option<&[Point]>) {}

    /// Deliver a mouse event to terminal `id`'s running program.
    fn send_mouse(&mut self, _id: &str, _event: TerminalMouseEvent) {}

    /// Execute a wall or terminal verb (`wall.select.*`, `wall.paste`, `terminal.resize`,
    /// `terminal.scroll`, `wall.group.frame`).
    fn run_command(&mut self, _args: &[String]) {}

    /// Begin dragging every member of `group_id` (`WallGroups.dragStart`).
    fn group_drag_start(&mut self, _group_id: &str) {}

    /// Translate an active group drag by `(dx, dy)` world units from its start (`WallGroups.dragTo`).
    fn group_drag_to(&mut self, _group_id: &str, _dx: f64, _dy: f64) {}

    /// Commit an active group drag (`WallGroups.dragEnd`).
    fn group_drag_end(&mut self, _group_id: &str) {}

    /// Drop tiled member `id` into its nearest tile and re-tile (`WallGroups.dropMember`).
    fn drop_tiled_member(&mut self, _id: &str) {}

    /// Re-apply `id`'s group layout after a cancelled pane drag or tiled resize (`WallGroups.relayoutOf`).
    fn relayout_member_group(&mut self, _id: &str) {}

    /// Preview where dragged tiled member `id` would land (`WallGroups.tileDrop`).
    fn tile_drop(&mut self, _id: &str) -> Option<DropPreview> {
        None
    }

    /// Preview where dragged split pane `id` would land at `world` (`WallGroups.paneDrop`).
    fn pane_drop(&mut self, _id: &str, _world: Point) -> Option<DropPreview> {
        None
    }

    /// Commit a split-pane rearrange drop at `world` (`WallGroups.dropPane`).
    fn drop_pane(&mut self, _id: &str, _world: Point) {}

    /// Move a split group's divider at `path` to world coordinate `coord` (`WallGroups.dragDivider`).
    fn drag_divider(&mut self, _group_id: &str, _path: &[BranchSide], _coord: f64) {}

    /// Resize split group `group_id` to `rect` (`WallGroups.resizeSplit`).
    fn resize_split(&mut self, _group_id: &str, _rect: Rect) {}
}

// ── Internal program-mouse state machine (port of WallMouse.js) ─────────────

#[derive(Clone, Debug)]
struct LastProgramSample {
    cell: CellPoint,
    mods: Mods,
}

#[derive(Clone, Debug)]
struct ActiveProgramGesture {
    id: String,
    held: Buttons,
    cell_key: Option<(u32, u32)>,
    last: Option<LastProgramSample>,
}

#[derive(Clone, Debug, Default)]
struct ProgramMouse {
    gesture: Option<ActiveProgramGesture>,
    hover_cell: Option<(String, u32, u32)>,
}

impl ProgramMouse {
    const BUTTON_ORDER: [(Buttons, ProgramMouseButton); 5] = [
        (Buttons::LEFT, ProgramMouseButton::Left),
        (Buttons::MIDDLE, ProgramMouseButton::Middle),
        (Buttons::RIGHT, ProgramMouseButton::Right),
        (Buttons::BACK, ProgramMouseButton::Back),
        (Buttons::FORWARD, ProgramMouseButton::Forward),
    ];

    fn tracking(host: &impl PointerHost, id: &str) -> MouseTracking {
        if !id.is_empty() && host.focused_id() == Some(id) && host.has_terminal(id) {
            host.mouse_tracking(id)
        } else {
            MouseTracking::None
        }
    }

    fn claims(
        host: &impl PointerHost,
        button: Option<Button>,
        mods: Mods,
        hit: Option<&CanvasHit>,
    ) -> bool {
        let Some(CanvasHit::Terminal { id, part }) = hit else {
            return false;
        };
        let focused = host.focused_id() == Some(id.as_str());
        let tracking = Self::tracking(host, id);
        program_gets(button, mods, *part, focused, tracking)
    }

    fn down(
        &mut self,
        host: &mut impl PointerHost,
        event: &PointerEvent,
        hit: Option<&CanvasHit>,
    ) -> bool {
        let Some(CanvasHit::Terminal { id, part }) = hit else {
            return false;
        };
        let focused = host.focused_id() == Some(id.as_str());
        let tracking = Self::tracking(host, id);
        if !program_gets(event.button, event.mods, *part, focused, tracking) {
            return false;
        }
        let Some(button) = event.button else {
            return false;
        };
        self.gesture = Some(ActiveProgramGesture {
            id: id.clone(),
            held: Buttons::NONE,
            cell_key: None,
            last: None,
        });
        self.sync_buttons(host, event, Buttons::from_button(button));
        true
    }

    fn on_move(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        let Some(id) = self.gesture.as_ref().map(|g| g.id.clone()) else {
            return;
        };
        if !host.has_terminal(&id) {
            self.gesture = None;
            return;
        }
        self.sync_buttons(host, event, event.buttons);
        let Some(held) = self.gesture.as_ref().map(|g| g.held) else {
            return;
        };
        if held.is_empty() {
            return;
        }
        let tracking = host.mouse_tracking(&id);
        if !matches!(tracking, MouseTracking::Button | MouseTracking::Any) {
            return;
        }
        let Some(at) = host.cell_at(&id, event.x, event.y) else {
            return;
        };
        let key = (at.col, at.row);
        if let Some(gesture) = self.gesture.as_mut() {
            if gesture.cell_key == Some(key) {
                return;
            }
            gesture.cell_key = Some(key);
        }
        host.send_mouse(
            &id,
            TerminalMouseEvent {
                action: MouseAction::Move,
                button: held_button(held),
                col: at.x,
                row: at.y,
                mods: event.mods,
            },
        );
    }

    fn up(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        let Some(id) = self.gesture.as_ref().map(|g| g.id.clone()) else {
            return;
        };
        if host.has_terminal(&id) {
            self.sync_buttons(host, event, event.buttons);
        }
        self.gesture = None;
    }

    fn cancel(&mut self, host: &mut impl PointerHost) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        if !host.has_terminal(&gesture.id) {
            return;
        }
        let Some(last) = gesture.last else {
            return;
        };
        for (bit, button) in Self::BUTTON_ORDER {
            if (gesture.held.0 & bit.0) != 0 {
                host.send_mouse(
                    &gesture.id,
                    TerminalMouseEvent {
                        action: MouseAction::Release,
                        button,
                        col: last.cell.x,
                        row: last.cell.y,
                        mods: last.mods,
                    },
                );
            }
        }
    }

    fn hover(
        &mut self,
        host: &mut impl PointerHost,
        screen_x: f64,
        screen_y: f64,
        mods: Mods,
        hit: Option<&CanvasHit>,
    ) {
        let body_id = match hit {
            Some(CanvasHit::Terminal {
                id,
                part: HitPart::Body,
            }) => Some(id.as_str()),
            _ => None,
        };
        let Some(id) = body_id else {
            self.hover_cell = None;
            return;
        };
        if Self::tracking(host, id) != MouseTracking::Any || mods.shift {
            self.hover_cell = None;
            return;
        }
        let Some(at) = host.cell_at(id, screen_x, screen_y) else {
            self.hover_cell = None;
            return;
        };
        if let Some((prev_id, prev_col, prev_row)) = &self.hover_cell {
            if prev_id == id && *prev_col == at.col && *prev_row == at.row {
                return;
            }
        }
        self.hover_cell = Some((id.to_owned(), at.col, at.row));
        host.send_mouse(
            id,
            TerminalMouseEvent {
                action: MouseAction::Move,
                button: ProgramMouseButton::None,
                col: at.x,
                row: at.y,
                mods,
            },
        );
    }

    fn sync_buttons(&mut self, host: &mut impl PointerHost, event: &PointerEvent, now: Buttons) {
        let Some((id, prev_held)) = self.gesture.as_ref().map(|g| (g.id.clone(), g.held)) else {
            return;
        };
        if now == prev_held {
            return;
        }
        let Some(at) = host.cell_at(&id, event.x, event.y) else {
            return;
        };
        for (bit, button) in Self::BUTTON_ORDER {
            if (now.0 & bit.0) == (prev_held.0 & bit.0) {
                continue;
            }
            let action = if (now.0 & bit.0) != 0 {
                MouseAction::Press
            } else {
                MouseAction::Release
            };
            host.send_mouse(
                &id,
                TerminalMouseEvent {
                    action,
                    button,
                    col: at.x,
                    row: at.y,
                    mods: event.mods,
                },
            );
        }
        if let Some(gesture) = self.gesture.as_mut() {
            gesture.held = now;
            gesture.cell_key = Some((at.col, at.row));
            gesture.last = Some(LastProgramSample {
                cell: at,
                mods: event.mods,
            });
        }
    }
}

// ── Internal gesture state ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ClickCounter {
    id: Option<String>,
    time_ms: f64,
    x: f64,
    y: f64,
    count: usize,
}

impl Default for ClickCounter {
    fn default() -> Self {
        Self {
            id: None,
            time_ms: f64::NEG_INFINITY,
            x: 0.0,
            y: 0.0,
            count: 0,
        }
    }
}

#[derive(Clone, Debug)]
enum DragPayload {
    Program,
    FsPaste {
        id: String,
    },
    Pan {
        paste_into: Option<String>,
    },
    Lasso {
        points: Vec<Point>,
    },
    Shift {
        id: String,
        anchor: Option<Point>,
    },
    Text {
        id: String,
        anchor: Point,
        mode: SelectionMode,
        block: bool,
        selecting: bool,
    },
    Press {
        id: String,
        click_frames: bool,
        raised: bool,
        start_rect: Option<Rect>,
    },
    Resize {
        id: String,
        edges: Edges,
        stride: Point,
        start: GridSize,
        last: GridSize,
        start_rect: Option<Rect>,
    },
    Group {
        group: String,
        pane: Option<String>,
        raised: bool,
        group_corner: Option<Option<Point>>,
    },
    Pane {
        id: String,
        raised: bool,
    },
    Divider {
        group: String,
        path: SplitPath,
        axis: Axis,
    },
    SplitResize {
        group: String,
        edges: Edges,
        start_rect: Rect,
    },
}

impl DragPayload {
    const fn kind(&self) -> GestureKind {
        match self {
            DragPayload::Program => GestureKind::Program,
            DragPayload::FsPaste { .. } => GestureKind::FsPaste,
            DragPayload::Pan { .. } => GestureKind::Pan,
            DragPayload::Lasso { .. } => GestureKind::Lasso,
            DragPayload::Shift { .. } => GestureKind::Shift,
            DragPayload::Text { .. } => GestureKind::Text,
            DragPayload::Press { .. } => GestureKind::Press,
            DragPayload::Resize { .. } => GestureKind::Resize,
            DragPayload::Group { .. } => GestureKind::Group,
            DragPayload::Pane { .. } => GestureKind::Pane,
            DragPayload::Divider { .. } => GestureKind::Divider,
            DragPayload::SplitResize { .. } => GestureKind::SplitResize,
        }
    }

    fn terminal_id(&self) -> Option<&str> {
        match self {
            DragPayload::FsPaste { id }
            | DragPayload::Shift { id, .. }
            | DragPayload::Text { id, .. }
            | DragPayload::Press { id, .. }
            | DragPayload::Resize { id, .. }
            | DragPayload::Pane { id, .. } => Some(id.as_str()),
            DragPayload::Program
            | DragPayload::Pan { .. }
            | DragPayload::Lasso { .. }
            | DragPayload::Group { .. }
            | DragPayload::Divider { .. }
            | DragPayload::SplitResize { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveDrag {
    pointer_id: i32,
    captured: bool,
    start_x: f64,
    start_y: f64,
    start_world: Point,
    cam_start: Point,
    start_pos: Option<Point>,
    moved: bool,
    drop: Option<DropPreview>,
    payload: DragPayload,
}

fn format_js_number(value: f64) -> String {
    if value == 0.0 {
        "0".to_owned()
    } else {
        value.to_string()
    }
}

fn dispatch_scroll(host: &mut impl PointerHost, id: &str, lines: i32, cell: Point) {
    host.run_command(&[
        "terminal.scroll".to_owned(),
        id.to_owned(),
        lines.to_string(),
        format_js_number(cell.x),
        format_js_number(cell.y),
    ]);
}

// ── WallPointer state machine ───────────────────────────────────────────────

/// Pointer and wheel gesture state machine for the wall (`wall/WallPointer.js`).
#[derive(Clone, Debug, Default)]
pub struct WallPointer {
    drag: Option<ActiveDrag>,
    last_screen: Option<Point>,
    mouse: ProgramMouse,
    scroller: TerminalScroller,
    wheel_gate: WheelStreamGate,
    clicks: ClickCounter,
}

impl WallPointer {
    /// Construct a fresh idle pointer state machine.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tear down transient state (scroll momentum, wheel stream gate, active lasso overlay).
    pub fn reset(&mut self, host: &mut impl PointerHost) {
        if let Some(drag) = self.drag.take() {
            if drag.captured {
                host.release_pointer_capture(drag.pointer_id);
            }
        }
        self.mouse = ProgramMouse::default();
        self.scroller.reset();
        self.wheel_gate.reset();
        host.set_lasso(None);
    }

    /// Advance any in-flight wheel-notch scroll momentum by `dt` seconds (`WallPointer.step`).
    pub fn step(&mut self, host: &mut impl PointerHost, dt: f64) {
        if let Some(active_id) = self.scroller.active_id() {
            if !host.is_shown(active_id) {
                self.scroller.reset();
                return;
            }
        }
        let lines = self.scroller.step(dt);
        if lines != 0 {
            if let Some(active_id) = self.scroller.active_id() {
                let id = active_id.to_owned();
                let cell = self.scroller.active_cell();
                dispatch_scroll(host, &id, lines, cell);
            }
        }
    }

    /// Whether wheel-notch lines are still gliding into a terminal across frames.
    pub fn gliding(&self) -> bool {
        self.scroller.gliding()
    }

    /// Cancel any buffered terminal scroll remainder or momentum (e.g. when typing a key or
    /// changing focus; `WallPointer.cancelScroll`).
    pub fn cancel_scroll(&mut self) {
        self.scroller.reset();
    }

    /// Cancel synthetic scroll momentum and absorb any active OS inertial wheel stream across
    /// a fullscreen or framing transition (`WallPointer.quenchWheel`).
    pub fn quench_wheel(&mut self, now_ms: f64) {
        self.scroller.reset();
        self.wheel_gate.quench(now_ms);
    }

    /// Whether a pointer gesture is currently active (`WallPointer.busy`).
    pub fn busy(&self) -> bool {
        self.drag.is_some()
    }

    /// Kind of gesture currently in hand, if any.
    pub fn active_gesture(&self) -> Option<GestureKind> {
        self.drag.as_ref().map(|d| d.payload.kind())
    }

    /// Drop preview for a tiled member or split pane currently being dragged (`WallPointer.drop`).
    pub fn drop_preview(&self) -> Option<DropPreview> {
        self.drag.as_ref().and_then(|d| d.drop)
    }

    /// Whether a freeform lasso is currently being drawn (`WallPointer.lassoing`).
    pub fn lassoing(&self) -> bool {
        matches!(
            self.drag.as_ref().map(|d| &d.payload),
            Some(DragPayload::Lasso { .. })
        )
    }

    /// Start a gesture from a press on terminal `id`'s map label: clicking frames the
    /// terminal, while dragging moves it (`WallPointer.pressLabel`).
    pub fn press_label(&mut self, host: &mut impl PointerHost, id: &str, event: &PointerEvent) {
        if event.button != Some(Button::Left) {
            return;
        }
        self.start_drag(
            host,
            event,
            Some(id),
            DragPayload::Press {
                id: id.to_owned(),
                click_frames: true,
                raised: false,
                start_rect: None,
            },
        );
    }

    /// Handle a pointer button press (`pointerdown`).
    pub fn on_down(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        // Another button joining an active program gesture arrives via `buttons` on `on_move`.
        if matches!(
            self.drag.as_ref().map(|d| &d.payload),
            Some(DragPayload::Program)
        ) {
            return;
        }
        let config = host.config();
        let fullscreen = host.fullscreen();
        let world = host.to_world(event.x, event.y);
        let at = host.hit(world);

        if self.mouse.down(host, event, at.as_ref()) {
            if fullscreen.is_none() {
                host.cancel_camera_ease();
            }
            self.start_drag(host, event, None, DragPayload::Program);
            self.capture_current(host);
            return;
        }

        if !matches!(event.button, Some(Button::Left | Button::Middle)) {
            return;
        }
        // On macOS, Ctrl+left-click is the OS secondary click (handled by the context menu).
        if config.platform == Platform::Mac
            && event.mods.ctrl
            && event.button == Some(Button::Left)
        {
            return;
        }

        if let Some(fs) = fullscreen {
            self.on_down_fullscreen(host, event, config, fs, at);
            return;
        }

        host.cancel_camera_ease();
        let left_hit = if event.button == Some(Button::Left) {
            at.as_ref()
        } else {
            None
        };

        if event.button == Some(Button::Left) && event.mods.shift {
            if let Some(CanvasHit::Terminal { id, part }) = left_hit {
                let anchor = if *part == HitPart::Body {
                    self.anchor_of(host, id, event.x, event.y)
                } else {
                    None
                };
                self.start_drag(
                    host,
                    event,
                    Some(id),
                    DragPayload::Shift {
                        id: id.clone(),
                        anchor,
                    },
                );
            } else {
                self.start_drag(
                    host,
                    event,
                    None,
                    DragPayload::Lasso {
                        points: vec![Point::new(event.x, event.y)],
                    },
                );
            }
            self.capture_current(host);
            return;
        }

        let Some(hit) = left_hit else {
            let paste_into = if event.button == Some(Button::Middle) {
                match at {
                    Some(CanvasHit::Terminal {
                        id,
                        part: HitPart::Body,
                    }) => Some(id),
                    _ => None,
                }
            } else {
                None
            };
            self.start_drag(host, event, None, DragPayload::Pan { paste_into });
            self.capture_current(host);
            return;
        };

        match hit {
            CanvasHit::Divider { group, path, axis } => {
                self.start_drag(
                    host,
                    event,
                    None,
                    DragPayload::Divider {
                        group: group.clone(),
                        path: path.clone(),
                        axis: *axis,
                    },
                );
            }
            CanvasHit::GroupHeader { group } => {
                host.group_drag_start(group);
                self.start_drag(
                    host,
                    event,
                    None,
                    DragPayload::Group {
                        group: group.clone(),
                        pane: None,
                        raised: false,
                        group_corner: None,
                    },
                );
            }
            CanvasHit::Terminal { id, part } => {
                let is_block = block_selects(event.mods, config.platform);
                if *part == HitPart::Body && (!event.mods.alt || is_block) {
                    self.start_text(host, event, id, is_block, config.click_repeat_ms);
                } else if *part == HitPart::Title && !event.mods.alt && host.is_split_member(id) {
                    self.start_drag(
                        host,
                        event,
                        Some(id),
                        DragPayload::Pane {
                            id: id.clone(),
                            raised: false,
                        },
                    );
                } else if event.mods.alt && host.is_split_member(id) {
                    // Alt on a split pane moves its whole split; a click with no travel focuses it.
                    if let Some(gid) = host.group_of(id) {
                        host.group_drag_start(&gid);
                        self.start_drag(
                            host,
                            event,
                            None,
                            DragPayload::Group {
                                group: gid,
                                pane: Some(id.clone()),
                                raised: false,
                                group_corner: None,
                            },
                        );
                    }
                } else if event.mods.alt || *part == HitPart::Title {
                    self.start_drag(
                        host,
                        event,
                        Some(id),
                        DragPayload::Press {
                            id: id.clone(),
                            click_frames: false,
                            raised: false,
                            start_rect: None,
                        },
                    );
                } else if let HitPart::Edge(edges) = *part {
                    if host.is_split_member(id) {
                        if let Some(split_edge) = host.split_edge(id, edges) {
                            match split_edge {
                                SplitEdge::Divider { group, path, axis } => {
                                    self.start_drag(
                                        host,
                                        event,
                                        None,
                                        DragPayload::Divider { group, path, axis },
                                    );
                                }
                                SplitEdge::Outer {
                                    group,
                                    edges: outer,
                                } => {
                                    if let Some(start_rect) = host.split_rect(&group) {
                                        self.start_drag(
                                            host,
                                            event,
                                            None,
                                            DragPayload::SplitResize {
                                                group,
                                                edges: outer,
                                                start_rect,
                                            },
                                        );
                                    }
                                }
                            }
                            host.focus(Some(id), false);
                        }
                    } else if let (Some(stride), Some(size)) =
                        (host.terminal_stride(id), host.terminal_size(id))
                    {
                        self.start_drag(
                            host,
                            event,
                            Some(id),
                            DragPayload::Resize {
                                id: id.clone(),
                                edges,
                                stride,
                                start: size,
                                last: size,
                                start_rect: None,
                            },
                        );
                        host.focus(Some(id), false);
                    }
                }
            }
        }
        self.capture_current(host);
    }

    fn on_down_fullscreen(
        &mut self,
        host: &mut impl PointerHost,
        event: &PointerEvent,
        config: PointerConfig,
        fullscreen: FullscreenState,
        at: Option<CanvasHit>,
    ) {
        let is_block = block_selects(event.mods, config.platform);
        match fullscreen {
            FullscreenState::Single { id: fs_id } => {
                if host.focused_id() != Some(fs_id.as_str()) {
                    host.focus(Some(&fs_id), false);
                }
                let hit_fs = matches!(&at, Some(CanvasHit::Terminal { id, .. }) if *id == fs_id);
                if !hit_fs {
                    return;
                }
                if event.button == Some(Button::Middle) {
                    self.start_drag(
                        host,
                        event,
                        Some(fs_id.as_str()),
                        DragPayload::FsPaste { id: fs_id.clone() },
                    );
                } else if event.mods.shift {
                    if let Some(anchor) = self.anchor_of(host, &fs_id, event.x, event.y) {
                        self.start_drag(
                            host,
                            event,
                            Some(fs_id.as_str()),
                            DragPayload::Text {
                                id: fs_id.clone(),
                                anchor,
                                mode: SelectionMode::Cell,
                                block: false,
                                selecting: false,
                            },
                        );
                    }
                } else if event.mods.alt && !is_block {
                    return;
                } else {
                    self.start_text(host, event, &fs_id, is_block, config.click_repeat_ms);
                }
                self.capture_current(host);
            }
            FullscreenState::Group { group_layout, .. } => {
                let Some(hit) = at else {
                    return;
                };
                if event.button == Some(Button::Middle) {
                    if let CanvasHit::Terminal {
                        id,
                        part: HitPart::Body,
                    } = hit
                    {
                        self.start_drag(
                            host,
                            event,
                            Some(id.as_str()),
                            DragPayload::FsPaste { id: id.clone() },
                        );
                        self.capture_current(host);
                    }
                    return;
                }
                match hit {
                    CanvasHit::Divider { group, path, axis } => {
                        self.start_drag(
                            host,
                            event,
                            None,
                            DragPayload::Divider { group, path, axis },
                        );
                        self.capture_current(host);
                    }
                    CanvasHit::GroupHeader { .. } => {}
                    CanvasHit::Terminal { id, part } => {
                        let freeform = group_layout == GroupLayoutKind::Freeform;
                        if freeform {
                            if let HitPart::Edge(edges) = part {
                                if let (Some(stride), Some(size)) =
                                    (host.terminal_stride(&id), host.terminal_size(&id))
                                {
                                    self.start_drag(
                                        host,
                                        event,
                                        Some(&id),
                                        DragPayload::Resize {
                                            id: id.clone(),
                                            edges,
                                            stride,
                                            start: size,
                                            last: size,
                                            start_rect: None,
                                        },
                                    );
                                    host.focus(Some(&id), false);
                                    self.capture_current(host);
                                }
                                return;
                            }
                            if part == HitPart::Title || (event.mods.alt && !is_block) {
                                self.start_drag(
                                    host,
                                    event,
                                    Some(id.as_str()),
                                    DragPayload::Press {
                                        id: id.clone(),
                                        click_frames: false,
                                        raised: false,
                                        start_rect: None,
                                    },
                                );
                                self.capture_current(host);
                                return;
                            }
                        }
                        if part == HitPart::Title && group_layout == GroupLayoutKind::Split {
                            self.start_drag(
                                host,
                                event,
                                Some(id.as_str()),
                                DragPayload::Pane {
                                    id: id.clone(),
                                    raised: false,
                                },
                            );
                            self.capture_current(host);
                            return;
                        }
                        if part == HitPart::Title {
                            if host.focused_id() != Some(id.as_str()) {
                                host.focus(Some(&id), false);
                            }
                            return;
                        }
                        if event.mods.shift {
                            if host.focused_id() != Some(id.as_str()) {
                                host.focus(Some(&id), false);
                            }
                            if let Some(anchor) = self.anchor_of(host, &id, event.x, event.y) {
                                self.start_drag(
                                    host,
                                    event,
                                    Some(id.as_str()),
                                    DragPayload::Text {
                                        id: id.clone(),
                                        anchor,
                                        mode: SelectionMode::Cell,
                                        block: false,
                                        selecting: false,
                                    },
                                );
                                self.capture_current(host);
                            }
                        } else if event.mods.alt && !is_block {
                            if host.focused_id() != Some(id.as_str()) {
                                host.focus(Some(&id), false);
                            }
                        } else {
                            self.start_text(host, event, &id, is_block, config.click_repeat_ms);
                            self.capture_current(host);
                        }
                    }
                }
            }
        }
    }

    /// Handle pointer movement (`pointermove`).
    pub fn on_move(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        self.last_screen = Some(Point::new(event.x, event.y));
        if self.drag.is_none() {
            self.update_hover(host, event.x, event.y, event.mods);
            return;
        }
        if matches!(
            self.drag.as_ref().map(|d| &d.payload),
            Some(DragPayload::Program)
        ) {
            self.mouse.on_move(host, event);
            return;
        }

        if let Some(drag) = self.drag.as_mut() {
            if (event.x - drag.start_x).hypot(event.y - drag.start_y) > DRAG_SLOP_PX {
                drag.moved = true;
            }
        }

        // Pan moves immediately with pointer travel.
        if let Some(drag) = self.drag.as_ref() {
            if matches!(drag.payload, DragPayload::Pan { .. }) {
                let zoom = host.camera_zoom();
                host.camera_move_to(
                    drag.cam_start.x - (event.x - drag.start_x) / zoom,
                    drag.cam_start.y + (event.y - drag.start_y) / zoom,
                );
                return;
            }
        }

        // A Shift-press resolves once it travels past DRAG_SLOP_PX: body -> Text, else -> Lasso.
        if let Some(drag) = self.drag.as_mut() {
            if let DragPayload::Shift { id, anchor } = &drag.payload {
                if !drag.moved {
                    return;
                }
                let id_owned = id.clone();
                if let Some(anchor_pt) = *anchor {
                    if host.has_terminal(&id_owned) {
                        drag.payload = DragPayload::Text {
                            id: id_owned.clone(),
                            anchor: anchor_pt,
                            mode: SelectionMode::Cell,
                            block: false,
                            selecting: false,
                        };
                        if host.focused_id() != Some(id_owned.as_str()) {
                            host.focus(Some(&id_owned), false);
                        }
                    } else {
                        drag.payload = DragPayload::Lasso {
                            points: vec![Point::new(drag.start_x, drag.start_y)],
                        };
                    }
                } else {
                    drag.payload = DragPayload::Lasso {
                        points: vec![Point::new(drag.start_x, drag.start_y)],
                    };
                }
            }
        }

        // Text selection drag.
        if matches!(
            self.drag.as_ref().map(|d| &d.payload),
            Some(DragPayload::Text { .. })
        ) {
            self.move_text(host, event.x, event.y);
            return;
        }

        // Freeform lasso drag.
        if let Some(drag) = self.drag.as_mut() {
            if let DragPayload::Lasso { points } = &mut drag.payload {
                if let Some(last) = points.last().copied() {
                    if (event.x - last.x).hypot(event.y - last.y) >= LASSO_STEP_PX {
                        points.push(Point::new(event.x, event.y));
                    }
                }
                if drag.moved {
                    host.set_lasso(Some(points));
                }
                return;
            }
        }

        let world = host.to_world(event.x, event.y);
        let Some(start_world) = self.drag.as_ref().map(|d| d.start_world) else {
            return;
        };
        let dx = world.x - start_world.x;
        let dy = world.y - start_world.y;

        // Divider, SplitResize, and Group gestures do not depend on a single terminal grid.
        if let Some(kind) = self.drag.as_ref().map(|d| d.payload.kind()) {
            match kind {
                GestureKind::Divider => {
                    let Some(drag) = self.drag.as_ref() else {
                        return;
                    };
                    if !drag.moved {
                        return;
                    }
                    let DragPayload::Divider { group, path, axis } = &drag.payload else {
                        return;
                    };
                    let group = group.clone();
                    let path = path.clone();
                    let axis = *axis;
                    if !host.has_group(&group) {
                        self.end_drag(host);
                        return;
                    }
                    let coord = match axis {
                        Axis::X => world.x,
                        Axis::Y => world.y,
                    };
                    host.drag_divider(&group, &path, coord);
                    return;
                }
                GestureKind::SplitResize => {
                    let Some(drag) = self.drag.as_ref() else {
                        return;
                    };
                    if !drag.moved {
                        return;
                    }
                    let DragPayload::SplitResize {
                        group,
                        edges,
                        start_rect,
                    } = &drag.payload
                    else {
                        return;
                    };
                    let group = group.clone();
                    let edges = *edges;
                    let start_rect = *start_rect;
                    if !host.has_group(&group) {
                        self.end_drag(host);
                        return;
                    }
                    let rect = self.split_rect_for(host, start_rect, edges, dx, dy, event.mods);
                    host.resize_split(&group, rect);
                    return;
                }
                GestureKind::Group => {
                    let Some(group) = self.drag.as_ref().and_then(|d| match &d.payload {
                        DragPayload::Group { group, .. } => Some(group.clone()),
                        _ => None,
                    }) else {
                        return;
                    };
                    if !host.has_group(&group) {
                        self.end_drag(host);
                        return;
                    }
                    let moved = self.drag.as_ref().is_some_and(|d| d.moved);
                    if moved {
                        let needs_raise = self.drag.as_ref().is_some_and(|d| {
                            matches!(&d.payload, DragPayload::Group { raised: false, .. })
                        });
                        if needs_raise {
                            if let Some(drag) = self.drag.as_mut() {
                                if let DragPayload::Group { raised, .. } = &mut drag.payload {
                                    *raised = true;
                                }
                            }
                            host.bring_group_to_front(&group);
                        }
                        let delta = self.snap_group_delta(host, &group, dx, dy, event.mods);
                        host.group_drag_to(&group, delta.x, delta.y);
                    }
                    return;
                }
                _ => {}
            }
        }

        // Terminal-bound gestures (FsPaste, Pane, Press, Resize).
        let Some(term_id) = self
            .drag
            .as_ref()
            .and_then(|d| d.payload.terminal_id().map(str::to_owned))
        else {
            return;
        };
        if !host.has_terminal(&term_id) {
            self.end_drag(host);
            return;
        }

        let Some(kind) = self.drag.as_ref().map(|d| d.payload.kind()) else {
            return;
        };
        match kind {
            GestureKind::Pane => {
                let moved = self.drag.as_ref().is_some_and(|d| d.moved);
                if !moved {
                    return;
                }
                if !host.is_split_member(&term_id) {
                    self.end_drag(host);
                    return;
                }
                let needs_raise = self.drag.as_ref().is_some_and(|d| {
                    matches!(&d.payload, DragPayload::Pane { raised: false, .. })
                });
                if needs_raise {
                    if let Some(drag) = self.drag.as_mut() {
                        if let DragPayload::Pane { raised, .. } = &mut drag.payload {
                            *raised = true;
                        }
                    }
                    host.bring_to_front(&term_id);
                }
                if let Some(start_pos) = self.drag.as_ref().and_then(|d| d.start_pos) {
                    host.set_terminal_position(&term_id, Point::new(start_pos.x + dx, start_pos.y + dy));
                }
                let drop = host.pane_drop(&term_id, world);
                if let Some(drag) = self.drag.as_mut() {
                    drag.drop = drop;
                }
            }
            GestureKind::Press => {
                let moved = self.drag.as_ref().is_some_and(|d| d.moved);
                if !moved {
                    return;
                }
                let needs_raise = self.drag.as_ref().is_some_and(|d| {
                    matches!(&d.payload, DragPayload::Press { raised: false, .. })
                });
                if needs_raise {
                    if let Some(drag) = self.drag.as_mut() {
                        if let DragPayload::Press { raised, .. } = &mut drag.payload {
                            *raised = true;
                        }
                    }
                    host.bring_to_front(&term_id);
                }
                let snap_shift = self.snap_press_shift(host, &term_id, dx, dy, event.mods);
                if let Some(start_pos) = self.drag.as_ref().and_then(|d| d.start_pos) {
                    host.set_terminal_position(
                        &term_id,
                        Point::new(start_pos.x + dx + snap_shift.x, start_pos.y + dy + snap_shift.y),
                    );
                }
                if host.is_tiled_member(&term_id) {
                    let drop = host.tile_drop(&term_id);
                    if let Some(drag) = self.drag.as_mut() {
                        drag.drop = drop;
                    }
                }
            }
            GestureKind::Resize => {
                let step = self.snap_step_for_terminal(host, Some(&term_id), event.mods);
                let init_rect = host.terminal_rect(&term_id);
                let Some((moved, edges, stride, start, last, start_pos, start_rect)) =
                    self.drag.as_mut().and_then(|d| {
                        let start_pos = d.start_pos;
                        let moved = d.moved;
                        if let DragPayload::Resize {
                            edges,
                            stride,
                            start,
                            last,
                            start_rect,
                            ..
                        } = &mut d.payload
                        {
                            if start_rect.is_none() {
                                *start_rect = init_rect;
                            }
                            Some((moved, *edges, *stride, *start, *last, start_pos, *start_rect))
                        } else {
                            None
                        }
                    })
                else {
                    return;
                };
                if step > 0.0 && !moved {
                    return;
                }
                let resized = match (step > 0.0, start_rect) {
                    (true, Some(rect)) => {
                        snap_resize(edges, rect, start, stride, dx, dy, MIN_SIZE, step)
                    }
                    _ => resize_from_drag(edges, start, stride, dx, dy, MIN_SIZE),
                };
                if resized.cols != last.cols || resized.rows != last.rows {
                    if let Some(drag) = self.drag.as_mut() {
                        if let DragPayload::Resize { last, .. } = &mut drag.payload {
                            *last = GridSize::new(resized.cols, resized.rows);
                        }
                    }
                    host.run_command(&[
                        "terminal.resize".to_owned(),
                        term_id.clone(),
                        resized.cols.to_string(),
                        resized.rows.to_string(),
                    ]);
                }
                if !host.is_tiled_member(&term_id) {
                    if let Some(pos) = start_pos {
                        host.set_terminal_position(
                            &term_id,
                            Point::new(pos.x + resized.shift_x, pos.y + resized.shift_y),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Handle pointer button release (`pointerup`).
    pub fn on_up(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        if self
            .drag
            .as_ref()
            .is_none_or(|d| d.pointer_id != event.pointer_id)
        {
            return;
        }
        let Some(drag) = self.end_drag(host) else {
            return;
        };

        match drag.payload {
            DragPayload::Program => {
                self.mouse.up(host, event);
            }
            DragPayload::FsPaste { id } => {
                if !drag.moved && host.has_terminal(&id) && host.has_selection_clipboard() {
                    host.run_command(&["wall.paste".to_owned(), id, "selection".to_owned()]);
                }
            }
            DragPayload::Pan { paste_into } => {
                if drag.moved {
                    return;
                }
                if let Some(paste_id) = paste_into {
                    if host.has_terminal(&paste_id) && host.has_selection_clipboard() {
                        host.run_command(&[
                            "wall.paste".to_owned(),
                            paste_id,
                            "selection".to_owned(),
                        ]);
                    }
                    return;
                }
                host.focus(None, false);
                host.run_command(&["wall.select".to_owned(), "none".to_owned()]);
            }
            DragPayload::Lasso { points } => {
                if drag.moved {
                    self.lasso_select(host, &points);
                }
            }
            DragPayload::Divider { .. } | DragPayload::SplitResize { .. } => {}
            DragPayload::Group { group, pane, .. } => {
                if !host.has_group(&group) {
                    return;
                }
                if drag.moved {
                    host.group_drag_end(&group);
                } else if let Some(pane_id) = pane {
                    host.focus(Some(&pane_id), false);
                } else {
                    host.bring_group_to_front(&group);
                    let mut cmd = vec!["wall.select".to_owned()];
                    cmd.extend(host.shown_group_members(&group));
                    host.run_command(&cmd);
                }
            }
            DragPayload::Shift { id, .. } => {
                if host.has_terminal(&id) {
                    host.run_command(&["wall.select.toggle".to_owned(), id]);
                }
            }
            DragPayload::Text { id, selecting, .. } => {
                if host.has_terminal(&id) && selecting && host.has_selection(&id) {
                    host.selection_made(&id);
                }
            }
            DragPayload::Pane { id, .. } => {
                if !host.has_terminal(&id) {
                    return;
                }
                if !drag.moved {
                    if host.focused_id() != Some(id.as_str()) {
                        host.focus(Some(&id), false);
                    }
                    return;
                }
                if host.is_split_member(&id) {
                    let drop_world = host.to_world(event.x, event.y);
                    host.drop_pane(&id, drop_world);
                }
            }
            DragPayload::Press {
                id, click_frames, ..
            } => {
                if !host.has_terminal(&id) {
                    return;
                }
                if !drag.moved {
                    host.focus(Some(&id), click_frames);
                } else {
                    self.settle_terminal(host, &id, GestureKind::Press);
                }
            }
            DragPayload::Resize { id, .. } => {
                if host.has_terminal(&id) {
                    self.settle_terminal(host, &id, GestureKind::Resize);
                }
            }
        }
    }

    /// Handle pointer loss (`pointercancel` / `lostpointercapture`).
    pub fn on_cancel(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        if self
            .drag
            .as_ref()
            .is_none_or(|d| d.pointer_id != event.pointer_id)
        {
            return;
        }
        let Some(drag) = self.end_drag(host) else {
            return;
        };
        match drag.payload {
            DragPayload::Program => self.mouse.cancel(host),
            DragPayload::Press { id, .. } if drag.moved && host.has_terminal(&id) => {
                self.settle_terminal(host, &id, GestureKind::Press);
            }
            DragPayload::Resize { id, .. } if drag.moved && host.has_terminal(&id) => {
                self.settle_terminal(host, &id, GestureKind::Resize);
            }
            DragPayload::Pane { id, .. } if drag.moved && host.has_terminal(&id) => {
                host.relayout_member_group(&id);
            }
            DragPayload::Group { group, .. } if drag.moved => {
                host.group_drag_end(&group);
            }
            _ => {}
        }
    }

    /// Handle a double-click (`dblclick`): frames a group header, frames a terminal on its
    /// title bar (never its body, where double-click selects a word), or fits all on empty
    /// canvas.
    pub fn on_double_click(&mut self, host: &mut impl PointerHost, event: &PointerEvent) {
        if host.fullscreen().is_some() || event.mods.shift {
            return;
        }
        let world = host.to_world(event.x, event.y);
        let hit = host.hit(world);
        if ProgramMouse::claims(host, event.button, event.mods, hit.as_ref()) {
            return;
        }
        match hit {
            Some(CanvasHit::GroupHeader { group }) => {
                host.run_command(&["wall.group.frame".to_owned(), group]);
            }
            Some(CanvasHit::Terminal {
                id,
                part: HitPart::Title,
            }) => {
                host.focus(Some(&id), true);
            }
            None => {
                host.fit_all();
            }
            _ => {}
        }
    }

    /// Handle a key press or release affecting hover cursors (`Alt` / `Shift`) or live
    /// mid-drag grid-snap inversion (`Control`).
    pub fn on_key(&mut self, host: &mut impl PointerHost, event: &KeyEvent) {
        if (event.key == "Alt" || event.key == "Shift") && self.drag.is_none() {
            if let Some(last) = self.last_screen {
                self.update_hover(host, last.x, last.y, event.mods);
            }
        }
        if event.key == "Control" {
            let re_snap = matches!(
                self.drag.as_ref().map(|d| d.payload.kind()),
                Some(GestureKind::Press | GestureKind::Resize | GestureKind::Group)
            );
            if re_snap {
                if let (Some(last), Some(pointer_id)) =
                    (self.last_screen, self.drag.as_ref().map(|d| d.pointer_id))
                {
                    let synthetic = PointerEvent {
                        pointer_id,
                        x: last.x,
                        y: last.y,
                        button: None,
                        buttons: Buttons::LEFT,
                        mods: Mods {
                            ctrl: event.mods.ctrl,
                            ..Mods::default()
                        },
                        time_ms: 0.0,
                    };
                    self.on_move(host, &synthetic);
                }
            }
        }
    }

    /// Handle a wheel event on the canvas (`WallPointer._onWheel`).
    pub fn on_wheel(&mut self, host: &mut impl PointerHost, event: &WheelEvent) {
        let config = host.config();
        let px_scale = match event.delta_mode {
            DeltaMode::Pixel => 1.0,
            DeltaMode::Line => LINE_PX,
            DeltaMode::Page => host.viewport_height(),
        };
        let dx = event.delta_x * px_scale;
        let dy = event.delta_y * px_scale;
        let raw_zoom = config.zoom_keys.matches(event.mods);
        let gate = self.wheel_gate.accept(
            dx,
            dy,
            event.time_ms,
            AcceptOptions {
                zoom: raw_zoom,
                ctrl_key: event.mods.ctrl,
            },
        );
        if !gate.ok {
            return;
        }
        let zoom = gate.zoom;

        if let Some(fullscreen) = host.fullscreen() {
            if zoom {
                return;
            }
            let scroll_id = match &fullscreen {
                FullscreenState::Single { id } => Some(id.clone()),
                FullscreenState::Group { elevated_ids, .. } => {
                    let hit = host.hit(host.to_world(event.x, event.y));
                    let hit_id = match hit {
                        Some(CanvasHit::Terminal { id, .. }) => Some(id),
                        _ => None,
                    };
                    hit_id.or_else(|| {
                        host.focused_id()
                            .filter(|focused| fullscreen.contains(focused))
                            .map(str::to_owned)
                            .or_else(|| elevated_ids.first().cloned())
                    })
                }
            };
            if let Some(id) = scroll_id {
                self.scroll_terminal(host, &id, event, config);
            }
            return;
        }

        if !zoom {
            let mut hit_focused: Option<String> = None;
            if let Some(focused) = host.focused_id() {
                let focused_owned = focused.to_owned();
                if host.is_shown(&focused_owned) {
                    if let Some(rect) = host.terminal_rect(&focused_owned) {
                        let world = host.to_world(event.x, event.y);
                        if rect.contains(world) {
                            hit_focused = Some(focused_owned);
                        }
                    }
                }
            }
            let target = self
                .wheel_gate
                .canvas_target(event.x, event.y, hit_focused.as_deref());
            if let CanvasWheelTarget::Terminal { id, .. } = target {
                if host.focused_id() == Some(id.as_str()) && host.is_shown(&id) {
                    self.scroll_terminal(host, &id, event, config);
                }
                return;
            }
            host.camera_pan_by(dx, dy);
            return;
        }

        if dy == 0.0 {
            return;
        }
        if dy.abs() < PINCH_MAX_DELTA {
            host.camera_zoom_at(event.x, event.y, (-dy * 0.01).exp());
        } else {
            host.camera_wheel_zoom_at(event.x, event.y, dy);
        }
    }

    // ── Private helpers ─────────────────────────────────────────────────────

    fn start_drag(
        &mut self,
        host: &impl PointerHost,
        event: &PointerEvent,
        terminal_id: Option<&str>,
        payload: DragPayload,
    ) {
        let start_world = host.to_world(event.x, event.y);
        let start_pos = terminal_id.and_then(|id| host.terminal_position(id));
        self.drag = Some(ActiveDrag {
            pointer_id: event.pointer_id,
            captured: false,
            start_x: event.x,
            start_y: event.y,
            start_world,
            cam_start: host.camera_pos(),
            start_pos,
            moved: false,
            drop: None,
            payload,
        });
    }

    fn capture_current(&mut self, host: &mut impl PointerHost) {
        if let Some(drag) = self.drag.as_mut() {
            host.set_pointer_capture(drag.pointer_id);
            drag.captured = true;
        }
    }

    fn end_drag(&mut self, host: &mut impl PointerHost) -> Option<ActiveDrag> {
        let drag = self.drag.take()?;
        if drag.captured {
            host.release_pointer_capture(drag.pointer_id);
        }
        if matches!(drag.payload, DragPayload::Lasso { .. }) {
            host.set_lasso(None);
        }
        Some(drag)
    }

    fn anchor_of(&self, host: &impl PointerHost, id: &str, screen_x: f64, screen_y: f64) -> Option<Point> {
        host.cell_at(id, screen_x, screen_y)
            .map(|at| Point::new(at.x, at.y))
    }

    fn start_text(
        &mut self,
        host: &mut impl PointerHost,
        event: &PointerEvent,
        id: &str,
        block: bool,
        click_repeat_ms: f64,
    ) {
        if host.focused_id() != Some(id) {
            host.focus(Some(id), false);
        }
        let again = self.clicks.id.as_deref() == Some(id)
            && (event.time_ms - self.clicks.time_ms) <= click_repeat_ms
            && (event.x - self.clicks.x).hypot(event.y - self.clicks.y) <= DRAG_SLOP_PX;
        self.clicks.count = if again {
            (self.clicks.count % CLICK_MODES.len()) + 1
        } else {
            1
        };
        self.clicks.id = Some(id.to_owned());
        self.clicks.time_ms = event.time_ms;
        self.clicks.x = event.x;
        self.clicks.y = event.y;

        let mode = CLICK_MODES[self.clicks.count - 1];
        let Some(anchor) = self.anchor_of(host, id, event.x, event.y) else {
            return;
        };
        let effective_block = block && mode == SelectionMode::Cell;
        let selecting = mode != SelectionMode::Cell;
        self.start_drag(
            host,
            event,
            Some(id),
            DragPayload::Text {
                id: id.to_owned(),
                anchor,
                mode,
                block: effective_block,
                selecting,
            },
        );
        if mode == SelectionMode::Cell {
            host.clear_selection(id);
        } else {
            host.select_start(id, anchor.x, anchor.y, mode, false);
        }
    }

    fn move_text(&mut self, host: &mut impl PointerHost, screen_x: f64, screen_y: f64) {
        let Some((id, moved, anchor, mode, block, selecting)) = self.drag.as_ref().and_then(|d| {
            if let DragPayload::Text {
                id,
                anchor,
                mode,
                block,
                selecting,
            } = &d.payload
            {
                Some((id.clone(), d.moved, *anchor, *mode, *block, *selecting))
            } else {
                None
            }
        }) else {
            return;
        };

        if !host.has_terminal(&id) {
            self.end_drag(host);
            return;
        }
        if !selecting {
            if !moved {
                return;
            }
            host.select_start(&id, anchor.x, anchor.y, mode, block);
            if let Some(drag) = self.drag.as_mut() {
                if let DragPayload::Text { selecting, .. } = &mut drag.payload {
                    *selecting = true;
                }
            }
        }
        if let Some(at) = host.cell_at(&id, screen_x, screen_y) {
            host.select_extend(&id, at.x, at.y);
        }
    }

    fn settle_terminal(&mut self, host: &mut impl PointerHost, id: &str, kind: GestureKind) {
        if kind == GestureKind::Press && host.is_tiled_member(id) {
            host.drop_tiled_member(id);
        } else if host.is_tiled_member(id) {
            host.relayout_member_group(id);
        } else {
            host.remember_position(id);
        }
    }

    fn lasso_select(&mut self, host: &mut impl PointerHost, screen_points: &[Point]) {
        let poly: Vec<Point> = screen_points
            .iter()
            .map(|pt| host.to_world(pt.x, pt.y))
            .collect();
        let touched: Vec<String> = host
            .canvas_order()
            .into_iter()
            .filter(|id| {
                host.window_rect(id)
                    .is_some_and(|rect| lasso_touches(&poly, rect))
            })
            .collect();
        if !touched.is_empty() {
            let mut cmd = vec!["wall.select.add".to_owned()];
            cmd.extend(touched);
            host.run_command(&cmd);
        }
    }

    fn scroll_terminal(
        &mut self,
        host: &mut impl PointerHost,
        id: &str,
        event: &WheelEvent,
        config: PointerConfig,
    ) {
        if event.delta_y == 0.0 {
            return;
        }
        let row_px = host
            .terminal_stride(id)
            .map(|stride| stride.y * host.camera_zoom());
        let rows = host.terminal_size(id).map_or(24, |size| size.rows);
        let cell = host
            .cell_at(id, event.x, event.y)
            .map_or(Point::new(0.0, 0.0), |at| Point::new(at.x, at.y));
        let lines = self.scroller.wheel(
            id,
            event.delta_y,
            event.delta_mode,
            cell,
            event.time_ms,
            WheelOptions {
                row_px,
                rows,
                multiplier: config.scroll_multiplier,
                glide: config.glide_scroll,
            },
        );
        if lines != 0 {
            dispatch_scroll(host, id, lines, cell);
        }
    }

    fn update_hover(
        &mut self,
        host: &mut impl PointerHost,
        screen_x: f64,
        screen_y: f64,
        mods: Mods,
    ) {
        let config = host.config();
        let is_block = block_selects(mods, config.platform);
        let hit = host.hit(host.to_world(screen_x, screen_y));
        let hovered_id = match &hit {
            Some(CanvasHit::Terminal { id, .. }) => Some(id.as_str()),
            _ => None,
        };
        let claim_mods = Mods {
            shift: mods.shift,
            alt: mods.alt,
            ..Mods::default()
        };

        if let Some(fs) = host.fullscreen() {
            match fs {
                FullscreenState::Group { group_layout, .. } => {
                    host.set_hovered(hovered_id);
                    self.mouse.hover(host, screen_x, screen_y, mods, hit.as_ref());
                    let freeform = group_layout == GroupLayoutKind::Freeform;
                    let cursor = match &hit {
                        None => CursorIcon::Default,
                        Some(_)
                            if ProgramMouse::claims(
                                host,
                                Some(Button::Left),
                                claim_mods,
                                hit.as_ref(),
                            ) =>
                        {
                            CursorIcon::Default
                        }
                        Some(CanvasHit::Divider { axis, .. }) => match axis {
                            Axis::X => CursorIcon::ColResize,
                            Axis::Y => CursorIcon::RowResize,
                        },
                        Some(CanvasHit::Terminal { part, .. })
                            if freeform && ((mods.alt && !is_block) || *part == HitPart::Title) =>
                        {
                            CursorIcon::Move
                        }
                        Some(CanvasHit::Terminal {
                            part: HitPart::Edge(edges),
                            ..
                        }) if freeform => CursorIcon::Resize(edge_cursor(*edges)),
                        Some(CanvasHit::Terminal {
                            part: HitPart::Title,
                            ..
                        }) => CursorIcon::Pointer,
                        _ => CursorIcon::Text,
                    };
                    host.set_cursor(cursor);
                }
                FullscreenState::Single { .. } => {
                    host.set_hovered(None);
                    self.mouse.hover(host, screen_x, screen_y, mods, hit.as_ref());
                    let cursor = if hit.is_none()
                        || ProgramMouse::claims(host, Some(Button::Left), claim_mods, hit.as_ref())
                    {
                        CursorIcon::Default
                    } else {
                        CursorIcon::Text
                    };
                    host.set_cursor(cursor);
                }
            }
            return;
        }

        host.set_hovered(hovered_id);
        self.mouse.hover(host, screen_x, screen_y, mods, hit.as_ref());
        let cursor = match &hit {
            None => {
                if mods.shift {
                    CursorIcon::Crosshair
                } else {
                    CursorIcon::Grab
                }
            }
            Some(_)
                if ProgramMouse::claims(host, Some(Button::Left), claim_mods, hit.as_ref()) =>
            {
                CursorIcon::Default
            }
            Some(CanvasHit::Divider { axis, .. }) => match axis {
                Axis::X => CursorIcon::ColResize,
                Axis::Y => CursorIcon::RowResize,
            },
            Some(CanvasHit::GroupHeader { .. }) => CursorIcon::Move,
            Some(_) if mods.shift => CursorIcon::Copy,
            Some(CanvasHit::Terminal { part, .. })
                if (mods.alt && !is_block) || *part == HitPart::Title =>
            {
                CursorIcon::Move
            }
            Some(CanvasHit::Terminal {
                id,
                part: HitPart::Edge(edges),
            }) => self.edge_cursor_for(host, id, *edges),
            Some(CanvasHit::Terminal { .. }) => CursorIcon::Text,
        };
        host.set_cursor(cursor);
    }

    fn edge_cursor_for(&self, host: &impl PointerHost, id: &str, edges: Edges) -> CursorIcon {
        if !host.is_split_member(id) {
            return CursorIcon::Resize(edge_cursor(edges));
        }
        match host.split_edge(id, edges) {
            Some(SplitEdge::Divider { axis, .. }) => match axis {
                Axis::X => CursorIcon::ColResize,
                Axis::Y => CursorIcon::RowResize,
            },
            Some(SplitEdge::Outer { edges: outer, .. }) => CursorIcon::Resize(edge_cursor(outer)),
            None => CursorIcon::Resize(edge_cursor(edges)),
        }
    }

    fn split_rect_for(
        &self,
        host: &impl PointerHost,
        start_rect: Rect,
        edges: Edges,
        dx: f64,
        dy: f64,
        mods: Mods,
    ) -> Rect {
        let mut rect = start_rect;
        if edges.left {
            rect.min_x += dx;
        }
        if edges.right {
            rect.max_x += dx;
        }
        if edges.top {
            rect.max_y += dy;
        }
        if edges.bottom {
            rect.min_y += dy;
        }
        let config = host.config();
        let step = if config.snap == inverts_snap(mods) {
            0.0
        } else {
            snap_step(config.grid, host.camera_zoom())
        };
        if step > 0.0 {
            if edges.left {
                rect.min_x = snap_to(rect.min_x, step);
            }
            if edges.right {
                rect.max_x = snap_to(rect.max_x, step);
            }
            if edges.top {
                rect.max_y = snap_to(rect.max_y, step);
            }
            if edges.bottom {
                rect.min_y = snap_to(rect.min_y, step);
            }
        }
        rect
    }

    fn snap_step_for_terminal(
        &self,
        host: &impl PointerHost,
        terminal_id: Option<&str>,
        mods: Mods,
    ) -> f64 {
        let config = host.config();
        if config.snap == inverts_snap(mods) {
            return 0.0;
        }
        if let Some(id) = terminal_id {
            if host.is_tiled_member(id) {
                return 0.0;
            }
        }
        snap_step(config.grid, host.camera_zoom())
    }

    fn snap_press_shift(
        &mut self,
        host: &impl PointerHost,
        id: &str,
        dx: f64,
        dy: f64,
        mods: Mods,
    ) -> Point {
        let init_rect = host.terminal_rect(id);
        let start_rect = self.drag.as_mut().and_then(|d| {
            if let DragPayload::Press { start_rect, .. } = &mut d.payload {
                if start_rect.is_none() {
                    *start_rect = init_rect;
                }
                *start_rect
            } else {
                None
            }
        });
        let step = self.snap_step_for_terminal(host, Some(id), mods);
        if step == 0.0 {
            return Point::new(0.0, 0.0);
        }
        let Some(rect) = start_rect else {
            return Point::new(0.0, 0.0);
        };
        let moved_rect = Rect {
            min_x: rect.min_x + dx,
            min_y: rect.min_y + dy,
            max_x: rect.max_x + dx,
            max_y: rect.max_y + dy,
        };
        snap_move(moved_rect, step)
    }

    fn snap_group_delta(
        &mut self,
        host: &impl PointerHost,
        group: &str,
        dx: f64,
        dy: f64,
        mods: Mods,
    ) -> Point {
        let needs_corner = self.drag.as_ref().is_some_and(|d| {
            matches!(
                &d.payload,
                DragPayload::Group {
                    group_corner: None,
                    ..
                }
            )
        });
        if needs_corner {
            let member_rects: Vec<Rect> = host
                .shown_group_members(group)
                .into_iter()
                .filter_map(|id| host.terminal_rect(&id))
                .collect();
            let corner = union_rect(&member_rects).map(|u| Point::new(u.min_x, u.max_y));
            if let Some(drag) = self.drag.as_mut() {
                if let DragPayload::Group { group_corner, .. } = &mut drag.payload {
                    *group_corner = Some(corner);
                }
            }
        }
        let corner = self.drag.as_ref().and_then(|d| match &d.payload {
            DragPayload::Group {
                group_corner: Some(corner),
                ..
            } => *corner,
            _ => None,
        });
        let step = self.snap_step_for_terminal(host, None, mods);
        if step > 0.0 {
            if let Some(c) = corner {
                return Point::new(snap_to(c.x + dx, step) - c.x, snap_to(c.y + dy, step) - c.y);
            }
        }
        Point::new(dx, dy)
    }
}
