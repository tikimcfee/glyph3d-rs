//! A terminal emulator behind one seam: a terminal's bytes in, a
//! [`term_cells::Screen`] out.
//!
//! `g3d-termd` owns each PTY, keeps scrollback, encodes input, and answers
//! terminal queries itself. [`Vt`] is the passive viewer on the app side: it
//! streams `REPAINT` and `OUTPUT` byte slices of arbitrary chunking through a
//! vendored pure-Rust port of Ghostty's VT core (see `NOTICE`), discards any
//! query replies or host side-channel events as they arrive, keeps only the
//! visible screen (`max_scrollback = 0`), and resolves cells into final sRGB
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
use term::terminal::{ActiveScreen, Terminal};
use term_cells::{attrs, rgb, Cell, Cursor, CursorShape, Modes, Screen, CLUSTER_BASE};

/// Upper bound on distinct multi-codepoint grapheme clusters interned per
/// [`Vt`] instance (matching `core/cellClusters.js`'s `CLUSTER_LIMIT_DEFAULT`).
/// Past this limit a cluster degrades gracefully to its base scalar rather than
/// growing `Screen::clusters` without bound.
const CLUSTER_LIMIT: usize = 65_536;

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
        // Panel foreground and background match `term_cells::Screen::new`;
        // the 16 ANSI colours match `collections/terminalPalette.js`'s
        // dark-panel readable palette.
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
            fg: rgb(0xd8, 0xde, 0xe9),
            bg: rgb(0x0d, 0x10, 0x17),
            cursor: rgb(0xd8, 0xde, 0xe9),
            bold_is_bright: false,
        }
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
    /// `Screen::generation` produced by the most recent [`Vt::fill`] call on
    /// this instance, allowing an O(1) early exit when neither [`Vt`] nor the
    /// caller's [`Screen`] changed since the last frame.
    last_filled_gen: Option<u64>,
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
            last_filled_gen: None,
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

    /// Whether DEC private mode 2026 (`CSI ? 2026 h`, synchronized output) is
    /// currently active, indicating the program is mid-frame and screen
    /// publication should wait for `CSI ? 2026 l` (or a safety timeout).
    pub fn synchronized_output(&self) -> bool {
        self.terminal.modes.get(Mode::SynchronizedOutput)
    }

    /// Resolved cursor colour (`OSC 12` override if active, otherwise the
    /// theme's cursor colour). Exposed here because [`term_cells::Cursor`] does
    /// not carry a colour field.
    pub fn cursor_color(&self) -> u32 {
        let color = self
            .terminal
            .cursor_color
            .or(self.terminal.base_cursor)
            .unwrap_or(self.terminal.default_fg);
        pack_rgb(color)
    }

    /// Feed raw terminal bytes (`REPAINT` or `OUTPUT`) in order. Escape and
    /// UTF-8 sequences split across calls are reassembled seamlessly.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.stream.process(bytes, &mut self.terminal);
        self.terminal.outbound.clear();
        self.terminal.events.clear();
    }

    /// Resize the visible grid to `cols` by `rows` (each clamped to at least
    /// `1`), reflowing soft-wrapped lines on the primary screen and clamping on
    /// the alternate screen.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal
            .resize(usize::from(cols.max(1)), usize::from(rows.max(1)));
        self.terminal.outbound.clear();
        self.terminal.events.clear();
    }

    /// Bring `screen` up to date, rewriting and touching ([`Screen::touch_row`])
    /// only the rows whose resolved cells or soft-wrap flag changed since what
    /// `screen` already holds. Returns `true` iff anything on `screen` changed.
    pub fn fill(&mut self, screen: &mut Screen) -> bool {
        let cols = self.cols();
        let rows = self.rows();

        if !self.terminal.dirty
            && self.last_filled_gen == Some(screen.generation)
            && screen.cols == cols
            && screen.rows == rows
        {
            return false;
        }

        let mut any_changed = false;
        let mut generation_bumped = false;

        if screen.cols != cols || screen.rows != rows {
            screen.resize(cols, rows);
            any_changed = true;
            generation_bumped = true;
        }

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

        let cols_usize = usize::from(cols);
        let default_resolved_cell = Cell {
            content: 0,
            fg: panel_fg,
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };

        for row_idx in 0..rows {
            let y = usize::from(row_idx);
            let wrapped = self.terminal.screen().row(y).wrapped;

            self.row_scratch.clear();
            let stored_len = self.terminal.screen().row(y).cells.len().min(cols_usize);
            for x in 0..stored_len {
                let resolved = self.resolve_cell(y, x, panel_fg, panel_bg, default_resolved_cell);
                self.row_scratch.push(resolved);
            }
            if stored_len < cols_usize {
                self.row_scratch
                    .resize(cols_usize, default_resolved_cell);
            }

            // Guarantee the `term-cells` wide-tail invariant: a wide character's
            // second column has `width = 0`, `content = 0`, and carries the same
            // `bg` (and line colours/attrs) as its head column.
            for x in 1..cols_usize {
                if self.row_scratch[x].width == 0 && self.row_scratch[x - 1].width == 2 {
                    let head = self.row_scratch[x - 1];
                    self.row_scratch[x] = Cell {
                        content: 0,
                        width: 0,
                        ..head
                    };
                }
            }

            let row_differs =
                self.row_scratch.as_slice() != screen.row(row_idx) || screen.wrapped[y] != wrapped;
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

        let active_screen = self.terminal.screen();
        let next_cursor = Cursor {
            col: (active_screen.cursor.x.min(cols_usize.saturating_sub(1))) as u16,
            row: (active_screen.cursor.y.min(usize::from(rows).saturating_sub(1))) as u16,
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

        let next_modes = Modes {
            alt_screen: self.terminal.active == ActiveScreen::Alternate,
            mouse: self.terminal.mouse_tracking,
            bracketed_paste: self.terminal.modes.get(Mode::BracketedPaste),
            app_cursor: self.terminal.modes.get(Mode::CursorKeys),
            focus_events: self.terminal.modes.get(Mode::FocusEvent),
        };
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
        self.last_filled_gen = Some(screen.generation);
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
        for y in start_row..=end_row {
            let lo = if y == start_row { start_col } else { 0 };
            let hi = if y == end_row { end_col } else { cols };
            row_buf.clear();
            let row = active.row(y);
            push_row_slice(row, lo, hi, &mut row_buf);

            let joined = y < end_row && hi >= cols && row.wrapped;
            if joined {
                out.push_str(&row_buf);
            } else {
                out.push_str(row_buf.trim_end_matches([' ', '\t']));
                if y < end_row {
                    out.push('\n');
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

    fn resolve_cell(
        &mut self,
        y: usize,
        x: usize,
        panel_fg: u32,
        panel_bg: u32,
        default_resolved_cell: Cell,
    ) -> Cell {
        let src = &self.terminal.screen().row(y).cells[x];
        if src.content == CellContent::Empty && src.style.is_default() {
            return default_resolved_cell;
        }

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
                    self.cluster_scratch.clear();
                    self.cluster_scratch.extend(cluster.cps.iter().copied());
                    let fallback = cluster.cps[0] as u32;
                    let id = self.intern_cluster(fallback);
                    (id, cluster_width)
                }
            }
        };

        let bold = cell_attrs & attrs::BOLD != 0;
        let mut fg = match style.fg_color {
            StyleColor::Rgb(c) => pack_rgb(c),
            StyleColor::Palette(idx) => {
                let effective = if self.bold_is_bright && bold && idx < 8 {
                    idx + 8
                } else {
                    idx
                };
                pack_rgb(self.terminal.palette[usize::from(effective)])
            }
            StyleColor::None => panel_fg,
        };

        let mut bg = match style.bg_color {
            StyleColor::Rgb(c) => pack_rgb(c),
            StyleColor::Palette(idx) => pack_rgb(self.terminal.palette[usize::from(idx)]),
            StyleColor::None => 0,
        };

        let ul = if cell_attrs & attrs::UNDERLINE_COLOR != 0 {
            match style.underline_color {
                StyleColor::Rgb(c) => pack_rgb(c),
                StyleColor::Palette(idx) => pack_rgb(self.terminal.palette[usize::from(idx)]),
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

    fn intern_cluster(&mut self, fallback_cp: u32) -> u32 {
        if let Some(&id) = self.cluster_ids.get(self.cluster_scratch.as_str()) {
            return id;
        }
        if self.clusters.len() >= CLUSTER_LIMIT {
            return fallback_cp;
        }
        let id = CLUSTER_BASE + self.clusters.len() as u32;
        let owned = self.cluster_scratch.clone();
        self.clusters.push(owned.clone());
        self.cluster_ids.insert(owned, id);
        id
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

fn apply_theme_baseline(terminal: &mut Terminal, theme: &Theme, reset_overrides: bool) {
    let mut base_palette = term::color::default_palette();
    for (idx, &packed) in theme.palette.iter().enumerate() {
        base_palette[idx] = unpack_rgb(packed);
    }
    let fg = unpack_rgb(theme.fg);
    let bg = unpack_rgb(theme.bg);
    let cursor = unpack_rgb(theme.cursor);

    terminal.base_palette = base_palette;
    terminal.base_fg = fg;
    terminal.base_bg = bg;
    terminal.base_cursor = Some(cursor);
    terminal.known_theme_colors.fill(true);

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
