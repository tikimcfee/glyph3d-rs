//! A terminal emulator behind one seam: a terminal's bytes in, a
//! [`term_cells::Screen`] out.
//!
//! `g3d-termd` owns each PTY, keeps scrollback, encodes input, and answers
//! terminal queries itself. [`Vt`] is the passive viewer on the app side: it
//! streams `REPAINT` and `OUTPUT` byte slices of arbitrary chunking through a
//! vendored pure-Rust port of Ghostty's VT core (see `NOTICE`), discards any
//! emulator query replies as they arrive, buffers host side-channel events in a
//! small bounded ring ([`Vt::take_events`] / [`Vt::drain_events`]), keeps only
//! the visible screen (`max_scrollback = 0`), and resolves cells into final sRGB
//! colours on a [`term_cells::Screen`], touching only rows that changed.

mod cell_width;
mod term;

use std::collections::HashMap;

use term::color::Rgb;
use term::modes::Mode;
use term::page::CellContent;
use term::screen::CursorStyle;
use term::stream::Stream;
use term::style::{Style, StyleColor, StyleFlags, Underline};
use term::terminal::{ActiveScreen, TermEvent, Terminal};
use term_cells::{
    attrs, rgb, Cell, Cursor, CursorShape, Modes, MouseFormat, Screen, CLUSTER_BASE,
};

/// Upper bound on distinct multi-codepoint grapheme clusters interned per
/// [`Vt`] instance (matching `core/cellClusters.js`'s `CLUSTER_LIMIT_DEFAULT`).
/// Past this limit a cluster degrades gracefully to its base scalar rather than
/// growing `Screen::clusters` without bound.
const CLUSTER_LIMIT: usize = 65_536;

/// Host-facing side-channel events produced by the terminal stream.
///
/// Buffered in a bounded ring inside [`Vt`] (evicting the oldest unread entry
/// past 256 events, so a caller that ignores events never grows memory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VtEvent {
    /// `BEL` (`0x07`).
    Bell,
    /// `OSC 52` clipboard write (`ESC ] 52 ; selection ; <base64> ST`),
    /// decoded as UTF-8. `selection` is the target byte (`b'c'` clipboard,
    /// `b'p'` primary, etc.).
    ClipboardWrite { selection: u8, text: String },
    /// `OSC 52` clipboard read request (`ESC ] 52 ; selection ; ? ST`).
    ClipboardRead { selection: u8 },
    /// `OSC 7` working-directory report (`ESC ] 7 ; <url> ST`).
    PwdChanged(String),
    /// `OSC 0` / `OSC 2` window title update.
    TitleChanged(String),
    /// `OSC 9` / `OSC 777` desktop notification.
    Notification { title: String, body: String },
}

impl From<TermEvent> for VtEvent {
    fn from(event: TermEvent) -> Self {
        match event {
            TermEvent::Bell => Self::Bell,
            TermEvent::ClipboardSet { kind, text } => Self::ClipboardWrite {
                selection: kind,
                text,
            },
            TermEvent::ClipboardRead { kind } => Self::ClipboardRead { selection: kind },
            TermEvent::PwdChanged(url) => Self::PwdChanged(url),
            TermEvent::TitleChanged(title) => Self::TitleChanged(title),
            TermEvent::Notification { title, body } => Self::Notification { title, body },
        }
    }
}

/// Colour policy for a [`Vt`] terminal: 16-colour ANSI palette, default
/// foreground, background, and cursor colours, plus standard-palette bold
/// brightening.
///
/// Every colour uses [`term_cells::rgba`] packing (`0xAABBGGRR`, opaque alpha
/// `0xff`, as produced by [`term_cells::rgb`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// ANSI 16-colour palette (`0..8` standard, `8..16` bright).
    pub palette: [u32; 16],
    /// Default text colour when a cell names no foreground.
    pub fg: u32,
    /// Default panel colour behind cells with no explicit fill.
    pub bg: u32,
    /// Default cursor colour (before any `OSC 12` override).
    pub cursor: u32,
    /// When `true`, `SGR 1` (`BOLD`) on standard palette indices `0..8`
    /// resolves to bright indices `8..16` (defaults to `false`, matching the JS
    /// wall's `BOLD_IS_BRIGHT_DEFAULT`).
    pub bold_is_bright: bool,
}

impl Default for Theme {
    fn default() -> Self {
        // Matches the browser wall's terminal theme:
        // - 16 ANSI colours: `packages/glyph3d-core/src/collections/terminalPalette.js` (`ANSI_16`)
        // - Default foreground (`#cccccc`): `terminalPalette.js` (`RESET_FG = {0.8, 0.8, 0.8}`)
        // - Default background (`#0a0a1e`): `collections/TerminalGrid.js` (`backgroundColor ?? 0x0a0a1e`)
        // - Default cursor (`#6ee7a0`): `collections/TerminalGrid.js` (`TERMINAL_CURSOR_DEFAULTS.color`)
        // - `bold_is_bright` (`false`): `collections/TerminalEmulator.js` (`BOLD_IS_BRIGHT_DEFAULT`)
        Self {
            palette: [
                rgb(0x00, 0x00, 0x00), // 0: black
                rgb(0xb3, 0x1a, 0x1a), // 1: red
                rgb(0x1a, 0xb3, 0x1a), // 2: green
                rgb(0xb3, 0xb3, 0x1a), // 3: yellow
                rgb(0x1a, 0x1a, 0xe6), // 4: blue
                rgb(0xb3, 0x1a, 0xb3), // 5: magenta
                rgb(0x1a, 0xb3, 0xb3), // 6: cyan
                rgb(0xbf, 0xbf, 0xbf), // 7: white (light grey)
                rgb(0x66, 0x66, 0x66), // 8: bright black (dark grey)
                rgb(0xff, 0x4d, 0x4d), // 9: bright red
                rgb(0x4d, 0xff, 0x4d), // 10: bright green
                rgb(0xff, 0xff, 0x4d), // 11: bright yellow
                rgb(0x66, 0x66, 0xff), // 12: bright blue
                rgb(0xff, 0x4d, 0xff), // 13: bright magenta
                rgb(0x4d, 0xff, 0xff), // 14: bright cyan
                rgb(0xff, 0xff, 0xff), // 15: bright white
            ],
            fg: rgb(0xcc, 0xcc, 0xcc),
            bg: rgb(0x0a, 0x0a, 0x1e),
            cursor: rgb(0x6e, 0xe7, 0xa0),
            bold_is_bright: false,
        }
    }
}

/// Scope of vertical scrolling recorded between two [`Vt::fill`] calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollScope {
    /// No vertical scroll occurred since the previous fill.
    #[default]
    None,
    /// Every scroll since the previous fill covered the whole screen
    /// (`0..rows`, full width), shifting all rows upward by
    /// [`FillDelta::scrolled_top`].
    FullScreen,
    /// Scrolling occurred within margins (`DECSTBM` / `DECSLRM`), downward
    /// (`RI` / `CSI T` / `CSI L`), or via `CSI M` line deletion, so the screen
    /// as a whole did not shift uniformly upward.
    Margins,
}

/// Per-fill delta describing how [`Vt::fill`] moved or rewrote cells since the
/// previous fill of the same [`Screen`].
///
/// Borrowed without allocation via [`Vt::last_fill_delta`] after [`Vt::fill`],
/// or directly via [`Vt::fill_delta`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FillDelta {
    /// Whether anything on [`Screen`] changed (the boolean [`Vt::fill`] returns).
    pub changed: bool,
    /// Whether the grid resized since the previous fill.
    pub resized: bool,
    /// Number of rows scrolled off row `0` (the top of the screen) since the
    /// previous fill.
    pub scrolled_top: u32,
    /// Whether scrolling since the previous fill covered the whole screen or
    /// was confined to margins.
    pub scroll_scope: ScrollScope,
    rewritten_cols: Vec<(u16, u16)>,
}

impl FillDelta {
    /// True when the whole screen scrolled upward (`scrolled_top > 0` and
    /// `scroll_scope == ScrollScope::FullScreen`).
    pub fn is_whole_screen_scroll(&self) -> bool {
        self.scroll_scope == ScrollScope::FullScreen && self.scrolled_top > 0
    }

    /// True when scrolling since the previous fill was confined to margins
    /// (`scroll_scope == ScrollScope::Margins`).
    pub fn within_margins(&self) -> bool {
        self.scroll_scope == ScrollScope::Margins
    }

    /// Per-row half-open column ranges `(lo, hi)` (`0..rows`) of cells that
    /// were rewritten in place (text, attributes, or colour) rather than
    /// carried unchanged by a whole-screen scroll. `(0, 0)` (or `lo >= hi`)
    /// means no cell on that row was rewritten in place.
    pub fn rewritten_cols(&self) -> &[(u16, u16)] {
        &self.rewritten_cols
    }

    /// Half-open column range `(lo, hi)` rewritten in place on `row`
    /// (`(0, 0)` when unchanged or out of bounds).
    pub fn row_rewritten_cols(&self, row: u16) -> (u16, u16) {
        self.rewritten_cols
            .get(usize::from(row))
            .copied()
            .unwrap_or((0, 0))
    }
}

/// Passive VT emulator turning a byte stream into a [`term_cells::Screen`].
pub struct Vt {
    stream: Stream,
    terminal: Terminal,
    bold_is_bright: bool,
    clusters: Vec<String>,
    cluster_ids: HashMap<String, u32>,
    row_scratch: Vec<Cell>,
    cluster_scratch: String,
    /// `(screen.cells.as_ptr() as usize, screen.generation)` produced by the
    /// most recent [`Vt::fill`] call on this instance, enabling both an O(1)
    /// idle fast path and safe per-row dirty skipping when filling the same
    /// [`Screen`] incrementally.
    last_filled: Option<(usize, u64)>,
    last_delta: FillDelta,
    last_delta_has_rewrites: bool,
}

impl Vt {
    /// Create a passive emulator of `cols` by `rows` cells (each clamped to at
    /// least `1`) with zero scrollback and `theme` as its colour baseline.
    pub fn new(cols: u16, rows: u16, theme: &Theme) -> Self {
        let cols_usize = usize::from(cols.max(1));
        let rows_usize = usize::from(rows.max(1));
        let mut terminal = Terminal::with_scrollback(cols_usize, rows_usize, 0);
        terminal.passive = true;
        apply_theme_baseline(&mut terminal, theme, true);
        Self {
            stream: Stream::new(),
            terminal,
            bold_is_bright: theme.bold_is_bright,
            clusters: Vec::new(),
            cluster_ids: HashMap::new(),
            row_scratch: Vec::with_capacity(cols_usize),
            cluster_scratch: String::new(),
            last_filled: None,
            last_delta: FillDelta {
                rewritten_cols: vec![(0, 0); rows_usize],
                ..FillDelta::default()
            },
            last_delta_has_rewrites: false,
        }
    }

    /// Update the colour theme baseline while preserving any active `OSC`
    /// dynamic overrides (`OSC 4/10/11/12`) set by the running program.
    pub fn set_theme(&mut self, theme: &Theme) {
        apply_theme_baseline(&mut self.terminal, theme, false);
        self.bold_is_bright = theme.bold_is_bright;
    }

    /// Current grid width in columns.
    pub fn cols(&self) -> u16 {
        self.terminal.cols() as u16
    }

    /// Current grid height in rows.
    pub fn rows(&self) -> u16 {
        self.terminal.rows() as u16
    }

    /// Resolved cursor colour (`OSC 12` override if active, else `theme.cursor`
    /// when its alpha is non-zero, else `0` meaning none was named so the host
    /// draws its own; also written into [`Screen::cursor_color`] by [`Vt::fill`]).
    pub fn cursor_color(&self) -> u32 {
        match self.terminal.cursor_color.or(self.terminal.base_cursor) {
            Some(color) => pack_rgb(color),
            None => 0,
        }
    }

    /// Latest working directory reported by the shell via `OSC 7` (empty if
    /// none has been reported, or after `RIS` `ESC c`).
    pub fn pwd(&self) -> &str {
        &self.terminal.pwd
    }

    /// `OSC 8` hyperlink URI under cell `(col, row)` on the active screen, if
    /// any. Landing on the right half of a wide character resolves its head.
    pub fn hyperlink(&self, col: u16, row: u16) -> Option<&str> {
        self.terminal
            .cell_hyperlink(usize::from(col), usize::from(row))
    }

    /// Current live [`term_cells::Modes`] of the emulator (callable immediately
    /// after [`Vt::feed`] before [`Vt::fill`] runs).
    pub fn modes(&self) -> Modes {
        Modes {
            alt_screen: self.terminal.active == ActiveScreen::Alternate,
            mouse: self.terminal.mouse_tracking,
            mouse_format: self.terminal.mouse_format,
            bracketed_paste: self.terminal.modes.get(Mode::BracketedPaste),
            app_cursor: self.terminal.modes.get(Mode::CursorKeys),
            focus_events: self.terminal.modes.get(Mode::FocusEvent),
            kitty_keyboard: self.terminal.kitty_flags(),
            synchronized_output: self.terminal.modes.get(Mode::SynchronizedOutput),
        }
    }

    /// Whether synchronized output (`DECSET ?2026`) is currently open, asking
    /// the host to hold frame presentation until `?2026l` (or a timeout/resize).
    pub fn synchronized_output(&self) -> bool {
        self.terminal.modes.get(Mode::SynchronizedOutput)
    }

    /// Active mouse coordinate encoding format (`DECSET 1005/1006/1015/1016`).
    pub fn mouse_format(&self) -> MouseFormat {
        self.terminal.mouse_format
    }

    /// Whether focus-in / focus-out reporting (`DECSET ?1004`) is active.
    pub fn focus_events(&self) -> bool {
        self.terminal.modes.get(Mode::FocusEvent)
    }

    /// Active screen's current Kitty keyboard protocol flags (`0..=31`).
    pub fn kitty_keyboard(&self) -> u8 {
        self.terminal.kitty_flags()
    }

    /// Active xterm `modifyOtherKeys` level (`0..=2`, `CSI > 4 ; Pv m`).
    pub fn modify_other_keys(&self) -> u8 {
        self.terminal.modify_other_keys
    }

    /// Whether any host side-channel events ([`VtEvent`]) are queued.
    pub fn has_events(&self) -> bool {
        !self.terminal.events.is_empty()
    }

    /// Drain all queued [`VtEvent`]s in arrival order without allocating a `Vec`.
    pub fn drain_events(&mut self) -> impl Iterator<Item = VtEvent> + '_ {
        self.terminal.events.drain(..).map(VtEvent::from)
    }

    /// Take all queued [`VtEvent`]s in arrival order.
    pub fn take_events(&mut self) -> Vec<VtEvent> {
        self.drain_events().collect()
    }

    /// Feed raw terminal bytes (`REPAINT` or `OUTPUT`) in order. Escape and
    /// UTF-8 sequences split across calls are reassembled seamlessly.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.stream.process(bytes, &mut self.terminal);
        self.terminal.outbound.clear();
    }

    /// Resize the visible grid to `cols` by `rows` (each clamped to at least
    /// `1`), reflowing soft-wrapped lines on the primary screen and clamping on
    /// the alternate screen.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal
            .resize(usize::from(cols.max(1)), usize::from(rows.max(1)));
        self.terminal.outbound.clear();
    }

    /// Delta recorded by the most recent [`Vt::fill`] call (scrolled rows,
    /// whole-screen vs margin scope, resize flag, and per-row in-place
    /// rewritten column spans).
    pub fn last_fill_delta(&self) -> &FillDelta {
        &self.last_delta
    }

    /// Run [`Vt::fill`] and borrow the resulting [`FillDelta`] directly.
    pub fn fill_delta(&mut self, screen: &mut Screen) -> &FillDelta {
        self.fill(screen);
        &self.last_delta
    }

    /// Bring `screen` up to date, rewriting and touching ([`Screen::touch_row`])
    /// only the rows whose resolved cells or soft-wrap flag changed since what
    /// `screen` already holds. Returns `true` iff anything on `screen` changed.
    pub fn fill(&mut self, screen: &mut Screen) -> bool {
        let cols = self.cols();
        let rows = self.rows();
        let screen_id = (screen.cells.as_ptr() as usize, screen.generation);

        if !self.terminal.dirty
            && !self.terminal.resized_since_fill
            && !self.terminal.screen_switched_since_fill
            && self.last_filled == Some(screen_id)
            && screen.cols == cols
            && screen.rows == rows
        {
            self.last_delta.changed = false;
            self.last_delta.resized = false;
            self.last_delta.scrolled_top = 0;
            self.last_delta.scroll_scope = ScrollScope::None;
            if self.last_delta_has_rewrites {
                self.last_delta.rewritten_cols.fill((0, 0));
                self.last_delta_has_rewrites = false;
            }
            return false;
        }

        let mut any_changed = false;
        let mut generation_bumped = false;
        let mut same_screen = self.last_filled == Some(screen_id);
        let size_mismatch = screen.cols != cols || screen.rows != rows;
        let resized = self.terminal.resized_since_fill || size_mismatch;
        let screen_switched = self.terminal.screen_switched_since_fill;

        if size_mismatch {
            screen.resize(cols, rows);
            any_changed = true;
            generation_bumped = true;
            same_screen = false;
        } else if resized {
            any_changed = true;
        }

        let rows_usize = usize::from(rows);
        let cols_usize = usize::from(cols);
        if self.last_delta_has_rewrites {
            self.last_delta.rewritten_cols.fill((0, 0));
        }
        if self.last_delta.rewritten_cols.len() != rows_usize {
            self.last_delta.rewritten_cols.resize(rows_usize, (0, 0));
        }
        let mut any_rewrites = false;

        let (scrolled_top, scroll_scope) = if resized || screen_switched {
            (0u32, ScrollScope::None)
        } else {
            let ps = &self.terminal.screen().pending_scroll;
            let scope = if ps.saw_margin_scroll {
                ScrollScope::Margins
            } else if ps.saw_full_screen_up && ps.scrolled_top > 0 {
                ScrollScope::FullScreen
            } else {
                ScrollScope::None
            };
            (ps.scrolled_top, scope)
        };

        if screen_switched && cols > 0 {
            self.last_delta.rewritten_cols.fill((0, cols));
            any_rewrites = rows > 0;
        } else if scroll_scope == ScrollScope::Margins {
            let moved = &self.terminal.screen().pending_scroll.margin_moved_cols;
            for (y, &span) in moved.iter().take(rows_usize).enumerate() {
                if span.0 < span.1 {
                    self.last_delta.rewritten_cols[y] = span;
                    any_rewrites = true;
                }
            }
        }

        self.terminal.primary.pending_scroll.reset();
        self.terminal.alternate.pending_scroll.reset();
        self.terminal.resized_since_fill = false;
        self.terminal.screen_switched_since_fill = false;

        let incremental_rows = same_screen
            && !self.terminal.all_rows_dirty
            && scrolled_top == 0
            && scroll_scope == ScrollScope::None;

        // Resolve panel defaults, honouring DECSCNM (`?5` reverse screen mode).
        let reverse_screen = self.terminal.modes.get(Mode::ReverseColors);
        let (panel_fg, panel_bg) = if reverse_screen {
            (
                pack_rgb(self.terminal.default_bg),
                pack_rgb(self.terminal.default_fg),
            )
        } else {
            (
                pack_rgb(self.terminal.default_fg),
                pack_rgb(self.terminal.default_bg),
            )
        };

        let default_resolved_cell = Cell {
            content: 0,
            fg: panel_fg,
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };

        let full_scroll_shift = if same_screen && !resized && scroll_scope == ScrollScope::FullScreen {
            Some(scrolled_top as usize)
        } else {
            None
        };

        for row_idx in 0..rows {
            let y = usize::from(row_idx);
            if incremental_rows && !self.terminal.screen().row(y).dirty {
                continue;
            }

            let wrapped = self.terminal.screen().row(y).wrapped;
            self.resolve_row(y, cols_usize, panel_fg, panel_bg, default_resolved_cell);
            self.terminal.screen_mut().row_mut(y).dirty = false;

            let (in_place_span, row_differs) = if screen_switched {
                let differs = self.row_scratch.as_slice() != screen.row(row_idx)
                    || screen.wrapped[y] != wrapped;
                ((0, cols), differs)
            } else if let Some(k) = full_scroll_shift {
                let span = if k < rows_usize && y + k < rows_usize {
                    let src_row = (y + k) as u16;
                    diff_row_span(
                        &self.row_scratch,
                        screen.row(src_row),
                        wrapped,
                        screen.wrapped[y + k],
                    )
                } else {
                    diff_row_against_blank(&self.row_scratch, default_resolved_cell, wrapped)
                };
                let differs = self.row_scratch.as_slice() != screen.row(row_idx)
                    || screen.wrapped[y] != wrapped;
                (span, differs)
            } else {
                let span = diff_row_span(
                    &self.row_scratch,
                    screen.row(row_idx),
                    wrapped,
                    screen.wrapped[y],
                );
                (span, span.0 < span.1)
            };

            if in_place_span.0 < in_place_span.1 {
                let merged = union_col_span(self.last_delta.rewritten_cols[y], in_place_span);
                self.last_delta.rewritten_cols[y] = merged;
                any_rewrites = true;
            }

            if row_differs {
                screen.row_mut(row_idx).copy_from_slice(&self.row_scratch);
                screen.wrapped[y] = wrapped;
                screen.touch_row(row_idx);
                any_changed = true;
                generation_bumped = true;
            }
        }

        // Sync interned multi-codepoint clusters into `screen.clusters` with
        // stable `CLUSTER_BASE + index` handles so unchanged rows never renumber.
        self.sync_clusters(&mut screen.clusters);

        if screen.default_fg != panel_fg {
            screen.default_fg = panel_fg;
            any_changed = true;
        }
        if screen.default_bg != panel_bg {
            screen.default_bg = panel_bg;
            any_changed = true;
        }

        let next_cursor_color = self.cursor_color();
        if screen.cursor_color != next_cursor_color {
            screen.cursor_color = next_cursor_color;
            any_changed = true;
        }

        let active_screen = self.terminal.screen();
        let next_cursor = Cursor {
            col: (active_screen.cursor.x.min(cols_usize.saturating_sub(1))) as u16,
            row: (active_screen.cursor.y.min(rows_usize.saturating_sub(1))) as u16,
            visible: self.terminal.modes.get(Mode::CursorVisible),
            shape: match self.terminal.cursor_style {
                CursorStyle::Default | CursorStyle::BlinkingBlock | CursorStyle::SteadyBlock => {
                    CursorShape::Block
                }
                CursorStyle::BlinkingBar | CursorStyle::SteadyBar => CursorShape::Bar,
                CursorStyle::BlinkingUnderline | CursorStyle::SteadyUnderline => {
                    CursorShape::Underline
                }
            },
            blink: self.terminal.modes.get(Mode::CursorBlinking),
        };
        if screen.cursor != next_cursor {
            screen.cursor = next_cursor;
            any_changed = true;
        }

        let next_modes = self.modes();
        if screen.modes != next_modes {
            screen.modes = next_modes;
            any_changed = true;
        }

        if screen.title != self.terminal.title {
            screen.title.clear();
            screen.title.push_str(&self.terminal.title);
            any_changed = true;
        }

        if any_changed && !generation_bumped {
            screen.generation += 1;
        }

        self.terminal.dirty = false;
        self.terminal.all_rows_dirty = false;
        self.last_delta.changed = any_changed;
        self.last_delta.resized = resized;
        self.last_delta.scrolled_top = scrolled_top;
        self.last_delta.scroll_scope = scroll_scope;
        self.last_delta_has_rewrites = any_rewrites;
        self.last_filled = Some((screen.cells.as_ptr() as usize, screen.generation));
        any_changed
    }

    /// Extract the stream text between two cell boundaries `(col0, row0)` and
    /// `(col1, row1)` (`col` in `0..=cols`, end-exclusive in reading order,
    /// matching `collections/terminalSelection.js`).
    ///
    /// Endpoints may be passed in either order; a boundary that lands inside a
    /// wide character widens to include the whole character. Rows whose selected
    /// span reaches `cols` and whose soft-wrap flag is set join directly onto
    /// the next row without a newline or trailing-space trim; hard line breaks
    /// trim trailing spaces/tabs and emit `'\n'`.
    pub fn text(&self, col0: u16, row0: u16, col1: u16, row1: u16) -> String {
        let active = self.terminal.screen();
        let cols = active.cols;
        let rows = active.rows;
        if cols == 0 || rows == 0 {
            return String::new();
        }

        let clamp_row = |r: u16| usize::from(r).min(rows - 1);
        let clamp_col = |c: u16| usize::from(c).min(cols);

        let mut start_row = clamp_row(row0);
        let mut start_col = clamp_col(col0);
        let mut end_row = clamp_row(row1);
        let mut end_col = clamp_col(col1);

        if start_row > end_row || (start_row == end_row && start_col > end_col) {
            std::mem::swap(&mut start_row, &mut end_row);
            std::mem::swap(&mut start_col, &mut end_col);
        }

        if start_row == end_row && start_col >= end_col {
            return String::new();
        }

        // Widen boundaries so a wide character is never split in half.
        if start_col > 0 && start_col < cols {
            if let Some(cell) = active.row(start_row).cell(start_col) {
                if cell.content == CellContent::WideTail {
                    start_col -= 1;
                }
            }
        }
        if end_col > 0 && end_col < cols {
            if let Some(cell) = active.row(end_row).cell(end_col - 1) {
                if cell.content.is_wide_head() {
                    end_col += 1;
                }
            }
        }

        let mut out = String::new();
        let mut row_buf = String::new();
        let mut line_start = 0usize;
        for y in start_row..=end_row {
            let lo = if y == start_row { start_col } else { 0 };
            let hi = if y == end_row { end_col } else { cols };
            row_buf.clear();
            let row = active.row(y);
            push_row_slice(row, lo, hi, &mut row_buf);
            out.push_str(&row_buf);

            let joined = y < end_row && hi >= cols && row.wrapped;
            if !joined {
                let trimmed_len =
                    line_start + out[line_start..].trim_end_matches([' ', '\t']).len();
                out.truncate(trimmed_len);
                if y < end_row {
                    out.push('\n');
                    line_start = out.len();
                }
            }
        }
        out
    }

    /// Extract rectangular (block) selection text across columns
    /// `min(col0, col1)..max(col0, col1)` and rows `min(row0, row1)..=max(row0, row1)`,
    /// trimming trailing whitespace on each row and joining rows with `'\n'`.
    pub fn block_text(&self, col0: u16, row0: u16, col1: u16, row1: u16) -> String {
        let active = self.terminal.screen();
        let cols = active.cols;
        let rows = active.rows;
        if cols == 0 || rows == 0 {
            return String::new();
        }

        let start_row = usize::from(row0.min(row1)).min(rows - 1);
        let end_row = usize::from(row0.max(row1)).min(rows - 1);
        let lo = usize::from(col0.min(col1)).min(cols);
        let hi = usize::from(col0.max(col1)).min(cols);
        if lo >= hi {
            return String::new();
        }

        let mut out = String::new();
        let mut row_buf = String::new();
        for y in start_row..=end_row {
            row_buf.clear();
            let row = active.row(y);
            let row_lo = if lo > 0
                && lo < cols
                && row
                    .cell(lo)
                    .is_some_and(|c| c.content == CellContent::WideTail)
            {
                lo - 1
            } else {
                lo
            };
            push_row_slice(row, row_lo, hi, &mut row_buf);
            out.push_str(row_buf.trim_end());
            if y < end_row {
                out.push('\n');
            }
        }
        out
    }

    /// Extract a single visual row (`0..cols`) with trailing spaces trimmed,
    /// without joining across soft wraps.
    pub fn row_text(&self, row: u16) -> String {
        let active = self.terminal.screen();
        let y = usize::from(row);
        if y >= active.rows {
            return String::new();
        }
        let mut out = String::new();
        push_row_slice(active.row(y), 0, active.cols, &mut out);
        let trimmed_len = out.trim_end_matches([' ', '\t']).len();
        out.truncate(trimmed_len);
        out
    }

    fn resolve_row(
        &mut self,
        y: usize,
        cols_usize: usize,
        panel_fg: u32,
        panel_bg: u32,
        default_resolved_cell: Cell,
    ) {
        let Self {
            terminal,
            bold_is_bright,
            clusters,
            cluster_ids,
            row_scratch,
            cluster_scratch,
            ..
        } = self;

        row_scratch.clear();
        let active = match terminal.active {
            ActiveScreen::Primary => &terminal.primary,
            ActiveScreen::Alternate => &terminal.alternate,
        };
        let row_cells = &active.active[y].cells;
        let stored_len = row_cells.len().min(cols_usize);
        let palette = &terminal.palette;
        let bold_bright = *bold_is_bright;
        let mut has_wide_tail = false;

        for src in &row_cells[..stored_len] {
            // Fast path (`TerminalEmulator.js` rule): most terminal cells carry
            // the default style (no attributes, default fg, transparent bg).
            if src.style.is_default() {
                let fast = match &src.content {
                    CellContent::Empty | CellContent::WideSpacerHead => Some(default_resolved_cell),
                    CellContent::Char(ch) => Some(Cell {
                        content: *ch as u32,
                        ..default_resolved_cell
                    }),
                    CellContent::WideChar(ch) => Some(Cell {
                        content: *ch as u32,
                        width: 2,
                        ..default_resolved_cell
                    }),
                    CellContent::WideTail => {
                        has_wide_tail = true;
                        Some(Cell {
                            content: 0,
                            width: 0,
                            ..default_resolved_cell
                        })
                    }
                    CellContent::Cluster(_) => None,
                };
                if let Some(cell) = fast {
                    row_scratch.push(cell);
                    continue;
                }
            }

            let resolved = resolve_styled_cell(
                src,
                palette,
                bold_bright,
                panel_fg,
                panel_bg,
                clusters,
                cluster_ids,
                cluster_scratch,
            );
            if resolved.width == 0 {
                has_wide_tail = true;
            }
            row_scratch.push(resolved);
        }

        if stored_len < cols_usize {
            row_scratch.resize(cols_usize, default_resolved_cell);
        }

        if has_wide_tail {
            for x in 1..cols_usize {
                if row_scratch[x].width == 0 && row_scratch[x - 1].width == 2 {
                    let head = row_scratch[x - 1];
                    row_scratch[x] = Cell {
                        content: 0,
                        width: 0,
                        ..head
                    };
                }
            }
        }
    }

    fn sync_clusters(&self, dst: &mut Vec<String>) {
        if dst.len() > self.clusters.len() || !self.clusters.starts_with(dst.as_slice()) {
            dst.clear();
        }
        if dst.len() < self.clusters.len() {
            dst.extend(self.clusters[dst.len()..].iter().cloned());
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "split-borrow fields of Vt keep row slice unaliased")]
fn resolve_styled_cell(
    src: &term::page::Cell,
    palette: &term::color::Palette,
    bold_is_bright: bool,
    panel_fg: u32,
    panel_bg: u32,
    clusters: &mut Vec<String>,
    cluster_ids: &mut HashMap<String, u32>,
    cluster_scratch: &mut String,
) -> Cell {
    let style = src.style;
    let cell_attrs = resolve_attrs(style);
    let conceal = cell_attrs & attrs::CONCEAL != 0;

    let (content, width) = match &src.content {
        CellContent::Empty => (0, 1),
        CellContent::Char(ch) => (if conceal { 0 } else { *ch as u32 }, 1),
        CellContent::WideChar(ch) => (if conceal { 0 } else { *ch as u32 }, 2),
        CellContent::WideTail => (0, 0),
        CellContent::WideSpacerHead => (0, 1),
        CellContent::Cluster(cluster) => {
            let cluster_width = cluster.width.clamp(1, 2);
            if conceal || cluster.cps.is_empty() {
                (0, cluster_width)
            } else if cluster.cps.len() == 1 {
                (cluster.cps[0] as u32, cluster_width)
            } else {
                cluster_scratch.clear();
                cluster_scratch.extend(cluster.cps.iter().copied());
                let fallback = cluster.cps[0] as u32;
                let id = intern_cluster(clusters, cluster_ids, cluster_scratch, fallback);
                (id, cluster_width)
            }
        }
    };

    let bold = cell_attrs & attrs::BOLD != 0;
    let mut fg = match style.fg_color {
        StyleColor::Rgb(c) => pack_rgb(c),
        StyleColor::Palette(idx) => {
            let effective = if bold_is_bright && bold && idx < 8 {
                idx + 8
            } else {
                idx
            };
            pack_rgb(palette[usize::from(effective)])
        }
        StyleColor::None => panel_fg,
    };

    let mut bg = match style.bg_color {
        StyleColor::Rgb(c) => pack_rgb(c),
        StyleColor::Palette(idx) => pack_rgb(palette[usize::from(idx)]),
        StyleColor::None => 0,
    };

    let ul = if cell_attrs & attrs::UNDERLINE_COLOR != 0 {
        match style.underline_color {
            StyleColor::Rgb(c) => pack_rgb(c),
            StyleColor::Palette(idx) => pack_rgb(palette[usize::from(idx)]),
            StyleColor::None => 0,
        }
    } else {
        0
    };

    if cell_attrs & attrs::INVERSE != 0 {
        let text_color = if bg != 0 { bg } else { panel_bg };
        bg = fg;
        fg = text_color;
    }

    Cell {
        content,
        fg,
        bg,
        ul,
        attrs: cell_attrs,
        width,
        _pad: 0,
    }
}

fn intern_cluster(
    clusters: &mut Vec<String>,
    cluster_ids: &mut HashMap<String, u32>,
    cluster_scratch: &str,
    fallback_cp: u32,
) -> u32 {
    if let Some(&id) = cluster_ids.get(cluster_scratch) {
        return id;
    }
    if clusters.len() >= CLUSTER_LIMIT {
        return fallback_cp;
    }
    let id = CLUSTER_BASE + clusters.len() as u32;
    let owned = cluster_scratch.to_owned();
    clusters.push(owned.clone());
    cluster_ids.insert(owned, id);
    id
}

#[inline]
fn pack_rgb(c: Rgb) -> u32 {
    rgb(c.r, c.g, c.b)
}

#[inline]
fn unpack_rgb(packed: u32) -> Rgb {
    Rgb::new(
        (packed & 0xff) as u8,
        ((packed >> 8) & 0xff) as u8,
        ((packed >> 16) & 0xff) as u8,
    )
}

#[inline]
fn union_col_span(a: (u16, u16), b: (u16, u16)) -> (u16, u16) {
    if a.0 >= a.1 {
        b
    } else if b.0 >= b.1 {
        a
    } else {
        (a.0.min(b.0), a.1.max(b.1))
    }
}

#[inline]
fn diff_row_span(
    next: &[Cell],
    prev: &[Cell],
    next_wrapped: bool,
    prev_wrapped: bool,
) -> (u16, u16) {
    let cols = next.len();
    debug_assert_eq!(cols, prev.len());
    let mut span = (0u16, 0u16);
    if let Some(lo) = (0..cols).find(|&x| next[x] != prev[x]) {
        let hi_inclusive = (lo..cols)
            .rev()
            .find(|&x| next[x] != prev[x])
            .unwrap_or(lo);
        let mut hi = hi_inclusive + 1;
        if hi < cols && (next[hi_inclusive].width == 2 || prev[hi_inclusive].width == 2) {
            hi += 1;
        }
        span = (lo as u16, hi as u16);
    }
    if next_wrapped != prev_wrapped && cols > 0 {
        span = union_col_span(span, ((cols - 1) as u16, cols as u16));
    }
    span
}

#[inline]
fn diff_row_against_blank(next: &[Cell], blank: Cell, next_wrapped: bool) -> (u16, u16) {
    let cols = next.len();
    let mut span = (0u16, 0u16);
    if let Some(lo) = (0..cols).find(|&x| next[x] != blank) {
        let hi_inclusive = (lo..cols).rev().find(|&x| next[x] != blank).unwrap_or(lo);
        let mut hi = hi_inclusive + 1;
        if hi < cols && next[hi_inclusive].width == 2 {
            hi += 1;
        }
        span = (lo as u16, hi as u16);
    }
    if next_wrapped && cols > 0 {
        span = union_col_span(span, ((cols - 1) as u16, cols as u16));
    }
    span
}

fn apply_theme_baseline(terminal: &mut Terminal, theme: &Theme, reset_overrides: bool) {
    let mut base_palette = term::color::default_palette();
    for (idx, &packed) in theme.palette.iter().enumerate() {
        base_palette[idx] = unpack_rgb(packed);
    }
    let fg = unpack_rgb(theme.fg);
    let bg = unpack_rgb(theme.bg);
    let has_theme_cursor = (theme.cursor >> 24) != 0;

    terminal.base_palette = base_palette;
    terminal.base_fg = fg;
    terminal.base_bg = bg;
    terminal.base_cursor = has_theme_cursor.then(|| unpack_rgb(theme.cursor));
    terminal.known_theme_colors.fill(true);
    terminal.known_theme_colors[258] = has_theme_cursor;

    if reset_overrides {
        terminal.palette = base_palette;
        terminal.default_fg = fg;
        terminal.default_bg = bg;
        terminal.cursor_color = None;
        terminal.palette_overrides.fill(false);
        terminal.foreground_override = false;
        terminal.background_override = false;
    } else {
        for (idx, &base_color) in base_palette.iter().enumerate() {
            if !terminal.palette_overrides[idx] {
                terminal.palette[idx] = base_color;
            }
        }
        if !terminal.foreground_override {
            terminal.default_fg = fg;
        }
        if !terminal.background_override {
            terminal.default_bg = bg;
        }
    }
    terminal.dirty = true;
    terminal.all_rows_dirty = true;
}

/// Map internal [`Style`] flags onto [`term_cells::attrs`] bit-for-bit.
///
/// Note: in the Ghostty/makepad `StyleFlags` word, bit 1 is `ITALIC` and bit 2
/// is `FAINT`, whereas in `term_cells::attrs` (matching `core/cellAttrs.js`)
/// bit 1 is `DIM` and bit 2 is `ITALIC`. Always map each bit by name.
#[inline]
fn resolve_attrs(style: Style) -> u16 {
    let flags = style.flags;
    let mut out = 0u16;
    if flags.has(StyleFlags::BOLD) {
        out |= attrs::BOLD;
    }
    if flags.has(StyleFlags::FAINT) {
        out |= attrs::DIM;
    }
    if flags.has(StyleFlags::ITALIC) {
        out |= attrs::ITALIC;
    }
    if flags.has(StyleFlags::BLINK) {
        out |= attrs::BLINK;
    }
    if flags.has(StyleFlags::INVERSE) {
        out |= attrs::INVERSE;
    }
    if flags.has(StyleFlags::INVISIBLE) {
        out |= attrs::CONCEAL;
    }
    if flags.has(StyleFlags::STRIKETHROUGH) {
        out |= attrs::STRIKE;
    }
    if flags.has(StyleFlags::OVERLINE) {
        out |= attrs::OVERLINE;
    }

    let underline = flags.underline();
    let underline_bits = match underline {
        Underline::None => attrs::UNDERLINE_NONE,
        Underline::Single => attrs::UNDERLINE_SINGLE,
        Underline::Double => attrs::UNDERLINE_DOUBLE,
        Underline::Curly => attrs::UNDERLINE_CURLY,
        Underline::Dotted => attrs::UNDERLINE_DOTTED,
        Underline::Dashed => attrs::UNDERLINE_DASHED,
    };
    out |= underline_bits << attrs::UNDERLINE_SHIFT;

    // Match `TerminalEmulator.js`: `UNDERLINE_COLOR` is set only when an
    // underline style is active AND `SGR 58` named a non-default colour.
    if underline != Underline::None && style.underline_color != StyleColor::None {
        out |= attrs::UNDERLINE_COLOR;
    }
    out
}

fn push_row_slice(row: &term::page::Row, lo: usize, hi: usize, out: &mut String) {
    for x in lo..hi {
        match row.cell(x) {
            Some(cell) => cell.content.push_text(out),
            None => out.push(' '),
        }
    }
}
