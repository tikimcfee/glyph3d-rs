//! Terminal text-selection model over [`term_cells::Screen`]
//! (ports `packages/glyph3d-core/src/collections/terminalSelection.js`).
//!
//! Decides which cells of a [`Screen`] are selected by a single-click stream or
//! rectangular column block, a double-click word selection (using Ghostty's
//! `selection-word-chars` boundary set across soft wraps), or a triple-click
//! logical-line selection (joining soft-wrapped rows and omitting leading and
//! trailing blanks); how a selection shifts when the screen scrolls up; and
//! what text a selection produces (trimming trailing blanks per hard line,
//! joining soft-wrapped rows without trimming their wrap-edge spaces, and
//! preserving wide characters and grapheme clusters).
//!
//! ### `Screen::wrapped` convention
//! In [`term_cells::Screen`], `screen.wrapped[r]` is `true` when **row `r`
//! continues onto row `r + 1`** (Ghostty's `Row.wrapped` convention), whereas
//! xterm.js's `line(y).isWrapped` in `terminalSelection.js` was `true` on the
//! continuation row (`y` continues `y - 1`). Every soft-wrap check here reads
//! [`Screen::wrapped`] on the upper row (`row_wraps_into_next(screen, r)`) so
//! live [`Screen`]s produced by `glyph-vt` join the exact wrapped rows.

use std::collections::HashSet;

use term_cells::{Cell, Screen, CLUSTER_BASE};

/// Characters that end a double-click word besides whitespace — Ghostty's
/// `selection-word-chars` default (`WORD_BOUNDARY_DEFAULT`).
pub const WORD_BOUNDARY_DEFAULT: &str = "'\"│`|:;,()[]{}<>$";

/// Default selection appearance and word-boundary character set
/// (`TERMINAL_SELECTION_DEFAULTS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionDefaults {
    pub background: &'static str,
    pub foreground: &'static str,
    pub word_chars: &'static str,
}

/// Default selection colours and word-boundary characters.
pub const TERMINAL_SELECTION_DEFAULTS: SelectionDefaults = SelectionDefaults {
    background: "#cccccc",
    foreground: "#0a0a1e",
    word_chars: WORD_BOUNDARY_DEFAULT,
};

/// Granularity of a pointer-driven selection (`'cell' | 'word' | 'line'`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SelectionMode {
    /// Stream (or column block) between the cell boundaries nearest anchor and head.
    #[default]
    Cell,
    /// Whole runs of word or boundary cells, following soft wraps.
    Word,
    /// Whole logical lines (soft-wrapped rows joined), outer blanks excluded.
    Line,
}

const fn is_space(code: u32) -> bool {
    code == 0 || code == b' ' as u32 || code == b'\t' as u32
}

/// Whether row `r` soft-wraps onto row `r + 1` (`Screen::wrapped[r]`).
#[inline]
fn row_wraps_into_next(screen: &Screen, r: usize) -> bool {
    r + 1 < screen.rows as usize && screen.wrapped.get(r).copied().unwrap_or(false)
}

/// Base codepoint of cell `(y, x)`: `0` for empty / wide-tail cells, the
/// scalar value, or the first scalar of a [`CLUSTER_BASE`] cluster.
#[inline]
fn cell_code(screen: &Screen, y: usize, x: usize) -> u32 {
    let cols = screen.cols as usize;
    let rows = screen.rows as usize;
    if y >= rows || x >= cols {
        return 0;
    }
    let cell = &screen.cells[y * cols + x];
    if cell.content == 0 {
        0
    } else if cell.content >= CLUSTER_BASE {
        screen
            .clusters
            .get((cell.content - CLUSTER_BASE) as usize)
            .and_then(|cluster| cluster.chars().next())
            .map_or(0, |ch| ch as u32)
    } else {
        cell.content
    }
}

/// Cell width at `(y, x)`: `2` for a wide head, `0` for its trailing cell, else `1`.
#[inline]
fn cell_width(screen: &Screen, y: usize, x: usize) -> u8 {
    let cols = screen.cols as usize;
    let rows = screen.rows as usize;
    if y >= rows || x >= cols {
        return 1;
    }
    screen.cells[y * cols + x].width
}

/// Append the untrimmed text of row `y`'s cells `[x0, x1)` to `buf`.
///
/// Empty single-width cells (`content == 0, width != 0`) emit `' '` as xterm's
/// `translateToString(false)` does; wide trailing cells (`width == 0`) emit
/// nothing; clusters emit their full cluster string.
fn push_row_slice(screen: &Screen, y: usize, x0: usize, x1: usize, buf: &mut String) {
    let cols = screen.cols as usize;
    if y >= screen.rows as usize {
        return;
    }
    let start = x0.min(cols);
    let end = x1.min(cols);
    if end <= start {
        return;
    }
    let row = &screen.cells[y * cols..y * cols + cols];
    for cell in &row[start..end] {
        if cell.width == 0 {
            continue;
        }
        if cell.content == 0 {
            buf.push(' ');
        } else if cell.content >= CLUSTER_BASE {
            if let Some(cluster) = screen.clusters.get((cell.content - CLUSTER_BASE) as usize) {
                buf.push_str(cluster);
            }
        } else if let Some(ch) = char::from_u32(cell.content) {
            buf.push(ch);
        }
    }
}

/// State of one terminal's text selection (`TerminalSelection` in JS).
///
/// Resolved coordinates hold a stream from `(r0, c0)` to `(r1, c1)` (`c1`
/// exclusive) or, when `block` is true, rows `r0..=r1` and columns `c0..c1`.
/// Extending or querying spans (`start`, `extend`, `lo`, `hi`) performs zero
/// heap allocations.
#[derive(Clone, Debug)]
pub struct TerminalSelection {
    boundary: HashSet<u32>,
    pub active: bool,
    pub mode: SelectionMode,
    pub block: bool,
    anchor_x: f64,
    anchor_y: f64,
    pub r0: usize,
    pub c0: usize,
    pub r1: usize,
    pub c1: usize,
}

impl Default for TerminalSelection {
    fn default() -> TerminalSelection {
        TerminalSelection::new()
    }
}

impl TerminalSelection {
    /// Construct an empty selection using [`WORD_BOUNDARY_DEFAULT`].
    pub fn new() -> TerminalSelection {
        TerminalSelection::with_word_chars(WORD_BOUNDARY_DEFAULT)
    }

    /// Construct an empty selection with custom word-boundary `chars`.
    pub fn with_word_chars(chars: &str) -> TerminalSelection {
        let mut selection = TerminalSelection {
            boundary: HashSet::new(),
            active: false,
            mode: SelectionMode::Cell,
            block: false,
            anchor_x: 0.0,
            anchor_y: 0.0,
            r0: 0,
            c0: 0,
            r1: 0,
            c1: 0,
        };
        selection.set_word_chars(chars);
        selection
    }

    /// Replace the characters that end a word besides whitespace
    /// (Ghostty's `selection-word-chars`).
    pub fn set_word_chars(&mut self, chars: &str) {
        self.boundary.clear();
        for ch in chars.chars() {
            self.boundary.insert(ch as u32);
        }
    }

    /// Clear any active selection.
    pub fn clear(&mut self) {
        self.active = false;
        self.mode = SelectionMode::Cell;
        self.block = false;
        self.anchor_x = 0.0;
        self.anchor_y = 0.0;
        self.r0 = 0;
        self.c0 = 0;
        self.r1 = 0;
        self.c1 = 0;
    }

    /// True when no cells are currently selected (`empty` getter in JS).
    pub fn is_empty(&self) -> bool {
        if !self.active {
            return true;
        }
        if self.block {
            return self.c0 >= self.c1;
        }
        self.r0 > self.r1 || (self.r0 == self.r1 && self.c0 >= self.c1)
    }

    /// Start a selection at fractional cell coordinates `(x, y)` from the
    /// top-left of `screen`. In [`SelectionMode::Word`] and
    /// [`SelectionMode::Line`], the word or logical line under `(x, y)` is
    /// selected immediately.
    pub fn start(
        &mut self,
        screen: &Screen,
        x: f64,
        y: f64,
        mode: SelectionMode,
        block: bool,
    ) {
        self.active = true;
        self.mode = mode;
        self.block = block && mode == SelectionMode::Cell;
        self.anchor_x = x;
        self.anchor_y = y;
        self.extend(screen, x, y);
    }

    /// Move the selection's head to fractional cell coordinates `(x, y)` while
    /// keeping its anchor fixed. Returns `true` when the selected cell range changed.
    pub fn extend(&mut self, screen: &Screen, x: f64, y: f64) -> bool {
        if !self.active {
            return false;
        }
        let (prev_r0, prev_c0, prev_r1, prev_c1) = (self.r0, self.c0, self.r1, self.c1);
        if screen.rows == 0 || screen.cols == 0 {
            self.r0 = 0;
            self.c0 = 0;
            self.r1 = 0;
            self.c1 = 0;
        } else if self.mode == SelectionMode::Cell {
            self.resolve_cells(screen, x, y);
        } else {
            self.resolve_units(screen, x, y);
        }
        prev_r0 != self.r0 || prev_c0 != self.c0 || prev_r1 != self.r1 || prev_c1 != self.c1
    }

    /// Select the exact cell range `(col0, row0)` to `(col1, row1)` (`col1`
    /// exclusive) as a stream or rectangular `block`, clamping out-of-range
    /// coordinates to `screen`.
    pub fn set(
        &mut self,
        screen: &Screen,
        col0: isize,
        row0: isize,
        col1: isize,
        row1: isize,
        block: bool,
    ) {
        self.active = true;
        self.mode = SelectionMode::Cell;
        self.block = block;
        let rows = screen.rows as usize;
        let cols = screen.cols as usize;
        if rows == 0 || cols == 0 {
            self.r0 = 0;
            self.c0 = 0;
            self.r1 = 0;
            self.c1 = 0;
            return;
        }
        let clamp = |val: isize, hi: usize| -> usize { val.max(0).min(hi as isize) as usize };
        let mut a = clamp(row0, rows - 1);
        let mut b = clamp(col0, cols);
        let mut c = clamp(row1, rows - 1);
        let mut d = clamp(col1, cols);
        if block {
            self.r0 = a.min(c);
            self.r1 = a.max(c);
            self.c0 = b.min(d);
            self.c1 = b.max(d);
            return;
        }
        if a > c || (a == c && b > d) {
            std::mem::swap(&mut a, &mut c);
            std::mem::swap(&mut b, &mut d);
        }
        self.r0 = a;
        self.c0 = b;
        self.r1 = c;
        self.c1 = d;
        self.widen(screen);
    }

    /// Shift the selection upward when the screen scrolls up by `lines` rows.
    /// Clears and returns `false` if the top of the selection scrolls off row `0`.
    pub fn shift(&mut self, lines: isize) -> bool {
        if !self.active {
            return false;
        }
        let next_r0 = self.r0 as isize - lines;
        let next_r1 = self.r1 as isize - lines;
        self.anchor_y -= lines as f64;
        if next_r0 < 0 {
            self.clear();
            return false;
        }
        self.r0 = next_r0 as usize;
        self.r1 = next_r1.max(0) as usize;
        true
    }

    /// First selected column on row `y` (paired with [`Self::hi`]: selected
    /// columns are `[lo, hi)`, or none when `hi <= lo`).
    pub fn lo(&self, y: usize) -> usize {
        if self.is_empty() || y < self.r0 || y > self.r1 {
            return 0;
        }
        if self.block || y == self.r0 {
            self.c0
        } else {
            0
        }
    }

    /// Exclusive end column of the selection on row `y` for a row of width `cols`.
    pub fn hi(&self, y: usize, cols: usize) -> usize {
        if self.is_empty() || y < self.r0 || y > self.r1 {
            return 0;
        }
        if self.block || y == self.r1 {
            self.c1
        } else {
            cols
        }
    }

    /// Extract the selected text from `screen` (empty when [`Self::is_empty`]).
    pub fn text(&self, screen: &Screen) -> String {
        if self.is_empty() {
            return String::new();
        }
        let cols = screen.cols as usize;
        let mut out = String::new();
        let mut row_buf = String::new();

        for y in self.r0..=self.r1 {
            let a = self.lo(y);
            let b = self.hi(y, cols);
            row_buf.clear();
            if b > a {
                push_row_slice(screen, y, a, b, &mut row_buf);
            }
            if self.block {
                out.push_str(row_buf.trim_end_matches(char::is_whitespace));
                if y < self.r1 {
                    out.push('\n');
                }
                continue;
            }
            let joined = y < self.r1 && b >= cols && row_wraps_into_next(screen, y);
            let slice = if joined {
                row_buf.as_str()
            } else {
                row_buf.trim_end_matches([' ', '\t'])
            };
            out.push_str(slice);
            if y < self.r1 && !joined {
                out.push('\n');
            }
        }
        out
    }

    fn resolve_cells(&mut self, screen: &Screen, x: f64, y: f64) {
        let rows = screen.rows as usize;
        let cols = screen.cols as usize;
        let snap_point = |px: f64, py: f64| -> (usize, usize) {
            if py >= rows as f64 {
                (rows - 1, cols)
            } else {
                let row = if py > 0.0 {
                    (py.floor() as usize).min(rows - 1)
                } else {
                    0
                };
                let col = if px > 0.0 {
                    (px.round() as usize).min(cols)
                } else {
                    0
                };
                (row, col)
            }
        };
        let (ar, ac) = snap_point(self.anchor_x, self.anchor_y);
        let (hr, hc) = snap_point(x, y);

        if self.block {
            self.r0 = ar.min(hr);
            self.r1 = ar.max(hr);
            self.c0 = ac.min(hc);
            self.c1 = ac.max(hc);
            return;
        }
        if ar < hr || (ar == hr && ac <= hc) {
            self.r0 = ar;
            self.c0 = ac;
            self.r1 = hr;
            self.c1 = hc;
        } else {
            self.r0 = hr;
            self.c0 = hc;
            self.r1 = ar;
            self.c1 = ac;
        }
        self.widen(screen);
    }

    /// Expand stream endpoints so neither cuts through the middle of a wide character.
    fn widen(&mut self, screen: &Screen) {
        let cols = screen.cols as usize;
        if self.c0 > 0 && self.c0 < cols && cell_width(screen, self.r0, self.c0) == 0 {
            self.c0 -= 1;
        }
        if self.c1 > 0 && self.c1 < cols && cell_width(screen, self.r1, self.c1 - 1) == 2 {
            self.c1 += 1;
        }
    }

    fn resolve_units(&mut self, screen: &Screen, x: f64, y: f64) {
        let rows = screen.rows as usize;
        let cols = screen.cols as usize;
        let clamp_coord = |val: f64, max_idx: usize| -> usize {
            if val > 0.0 {
                (val.floor() as usize).min(max_idx)
            } else {
                0
            }
        };
        let ay = clamp_coord(self.anchor_y, rows - 1);
        let ax = clamp_coord(self.anchor_x, cols - 1);
        let hy = clamp_coord(y, rows - 1);
        let hx = clamp_coord(x, cols - 1);
        let is_line = self.mode == SelectionMode::Line;

        if is_line {
            self.select_line(screen, ay);
        } else {
            self.select_word(screen, ax, ay);
        }
        if hy == ay && (is_line || hx == ax) {
            return;
        }
        let (anchor_r0, anchor_c0, anchor_r1, anchor_c1) = (self.r0, self.c0, self.r1, self.c1);
        if is_line {
            self.select_line(screen, hy);
        } else {
            self.select_word(screen, hx, hy);
        }
        self.union_range(anchor_r0, anchor_c0, anchor_r1, anchor_c1);
    }

    fn union_range(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        if r0 < self.r0 || (r0 == self.r0 && c0 < self.c0) {
            self.r0 = r0;
            self.c0 = c0;
        }
        if r1 > self.r1 || (r1 == self.r1 && c1 > self.c1) {
            self.r1 = r1;
            self.c1 = c1;
        }
    }

    #[inline]
    fn is_boundary(&self, code: u32) -> bool {
        is_space(code) || self.boundary.contains(&code)
    }

    fn select_word(&mut self, screen: &Screen, mut x: usize, y: usize) {
        let cols = screen.cols as usize;
        let rows = screen.rows as usize;
        if x > 0 && cell_width(screen, y, x) == 0 {
            x -= 1;
        }
        let kind = self.is_boundary(cell_code(screen, y, x));
        let mut sy = y;
        let mut sx = x;
        loop {
            let (py, mut px) = if sx == 0 {
                if sy == 0 || !row_wraps_into_next(screen, sy - 1) {
                    break;
                }
                (sy - 1, cols - 1)
            } else {
                (sy, sx - 1)
            };
            if px > 0 && cell_width(screen, py, px) == 0 {
                px -= 1;
            }
            if self.is_boundary(cell_code(screen, py, px)) != kind {
                break;
            }
            sy = py;
            sx = px;
        }

        let mut ey = y;
        let mut ex = x + cell_width(screen, y, x).max(1) as usize;
        loop {
            let (ny, nx) = if ex >= cols {
                if ey + 1 >= rows || !row_wraps_into_next(screen, ey) {
                    break;
                }
                (ey + 1, 0)
            } else {
                (ey, ex)
            };
            if self.is_boundary(cell_code(screen, ny, nx)) != kind {
                break;
            }
            ey = ny;
            ex = nx + cell_width(screen, ny, nx).max(1) as usize;
        }

        self.r0 = sy;
        self.c0 = sx;
        self.r1 = ey;
        self.c1 = ex.min(cols);
    }

    fn select_line(&mut self, screen: &Screen, y: usize) {
        let cols = screen.cols as usize;
        let rows = screen.rows as usize;
        let mut first = y;
        let mut last = y;
        while first > 0 && row_wraps_into_next(screen, first - 1) {
            first -= 1;
        }
        while last + 1 < rows && row_wraps_into_next(screen, last) {
            last += 1;
        }

        let mut start: Option<(usize, usize)> = None;
        'find_start: for r in first..=last {
            for c in 0..cols {
                if !is_space(cell_code(screen, r, c)) && cell_width(screen, r, c) != 0 {
                    start = Some((r, c));
                    break 'find_start;
                }
            }
        }
        let Some((sy, sx)) = start else {
            self.r0 = first;
            self.c0 = 0;
            self.r1 = first;
            self.c1 = 0;
            return;
        };

        let mut ey = sy;
        let mut ex = sx + 1;
        'find_end: for r in (sy..=last).rev() {
            for c in (0..cols).rev() {
                let width = cell_width(screen, r, c);
                if width == 0 || is_space(cell_code(screen, r, c)) {
                    continue;
                }
                ey = r;
                ex = cols.min(c + width.max(1) as usize);
                break 'find_end;
            }
        }
        self.r0 = sy;
        self.c0 = sx;
        self.r1 = ey;
        self.c1 = ex;
    }
}

/// Build a [`Screen`] from plain row strings (truncating past `cols` and padding
/// shorter rows with [`Cell::BLANK`]), marking `wrapped_rows` as soft-wrapping
/// onto their following row (`Screen::wrapped[r] = true`) and giving characters
/// matching `is_wide` a two-column cell pair (ports `textSourceOf` for tests and tools).
pub fn screen_from_lines<S, F>(
    lines: &[S],
    cols: u16,
    wrapped_rows: &[usize],
    mut is_wide: F,
) -> Screen
where
    S: AsRef<str>,
    F: FnMut(char) -> bool,
{
    let mut screen = Screen::new(cols, lines.len() as u16);
    let cols_usize = cols as usize;
    for &row_idx in wrapped_rows {
        if let Some(flag) = screen.wrapped.get_mut(row_idx) {
            *flag = true;
        }
    }
    for (row_idx, line) in lines.iter().enumerate() {
        let row = screen.row_mut(row_idx as u16);
        let mut col = 0;
        for ch in line.as_ref().chars() {
            if col >= cols_usize {
                break;
            }
            if is_wide(ch) {
                if col + 2 > cols_usize {
                    break;
                }
                row[col] = Cell {
                    content: ch as u32,
                    width: 2,
                    ..Cell::BLANK
                };
                row[col + 1] = Cell {
                    content: 0,
                    width: 0,
                    ..Cell::BLANK
                };
                col += 2;
            } else {
                row[col] = Cell {
                    content: ch as u32,
                    width: 1,
                    ..Cell::BLANK
                };
                col += 1;
            }
        }
        screen.touch_row(row_idx as u16);
    }
    screen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_screen(lines: &[&str], cols: u16) -> Screen {
        screen_from_lines(lines, cols, &[], |_| false)
    }

    fn select_between(
        screen: &Screen,
        a: (f64, f64),
        b: (f64, f64),
        mode: SelectionMode,
        block: bool,
    ) -> TerminalSelection {
        let mut s = TerminalSelection::new();
        s.start(screen, a.0, a.1, mode, block);
        s.extend(screen, b.0, b.1);
        s
    }

    #[test]
    fn ghostty_word_boundaries_are_the_default() {
        assert_eq!(WORD_BOUNDARY_DEFAULT, "'\"│`|:;,()[]{}<>$");
        assert_eq!(TERMINAL_SELECTION_DEFAULTS.background, "#cccccc");
        assert_eq!(TERMINAL_SELECTION_DEFAULTS.foreground, "#0a0a1e");
        assert_eq!(TERMINAL_SELECTION_DEFAULTS.word_chars, WORD_BOUNDARY_DEFAULT);
    }

    #[test]
    fn stream_runs_between_nearest_boundaries_and_fills_middle_rows() {
        let src = plain_screen(&["hello world", "second line", "third"], 12);

        assert_eq!(
            select_between(&src, (0.4, 0.5), (4.6, 0.5), SelectionMode::Cell, false).text(&src),
            "hello"
        );
        assert_eq!(
            select_between(&src, (0.6, 0.5), (4.6, 0.5), SelectionMode::Cell, false).text(&src),
            "ello"
        );
        assert_eq!(
            select_between(&src, (4.6, 0.5), (0.4, 0.5), SelectionMode::Cell, false).text(&src),
            "hello"
        );

        let s = select_between(&src, (6.0, 0.5), (3.0, 2.5), SelectionMode::Cell, false);
        assert_eq!(s.text(&src), "world\nsecond line\nthi");
        assert_eq!(
            [
                s.lo(0),
                s.hi(0, 12),
                s.lo(1),
                s.hi(1, 12),
                s.lo(2),
                s.hi(2, 12)
            ],
            [6, 12, 0, 12, 0, 3]
        );

        let none = select_between(&src, (2.2, 0.5), (2.4, 0.5), SelectionMode::Cell, false);
        assert!(none.is_empty());
        assert_eq!(none.text(&src), "");

        assert_eq!(
            select_between(&src, (0.0, 2.5), (1.0, 3.0), SelectionMode::Cell, false).text(&src),
            "third"
        );
    }

    #[test]
    fn soft_wrap_joins_its_rows_while_hard_line_end_stays() {
        // Row 0 ("the quick ") soft-wraps onto row 1 ("brown fox"): Screen::wrapped[0] = true.
        let src = screen_from_lines(&["the quick ", "brown fox", "next"], 10, &[0], |_| false);
        assert_eq!(
            select_between(&src, (0.0, 0.5), (9.0, 1.5), SelectionMode::Cell, false).text(&src),
            "the quick brown fox"
        );
        assert_eq!(
            select_between(&src, (4.0, 0.5), (4.0, 2.5), SelectionMode::Cell, false).text(&src),
            "quick brown fox\nnext"
        );
    }

    #[test]
    fn block_selects_same_columns_on_every_row_each_trimmed() {
        let src = plain_screen(&["abcdef", "ghijkl", "mn"], 8);
        let s = select_between(&src, (1.0, 0.5), (4.0, 2.5), SelectionMode::Cell, true);
        assert_eq!(s.text(&src), "bcd\nhij\nn");
        assert_eq!([s.lo(1), s.hi(1, 8)], [1, 4]);
    }

    #[test]
    fn wide_character_is_selected_whole() {
        let src = screen_from_lines(&["a一b"], 6, &[], |ch| ch as u32 == 0x4E00);
        assert_eq!(
            select_between(&src, (0.0, 0.5), (2.0, 0.5), SelectionMode::Cell, false).text(&src),
            "a一"
        );
        let s = select_between(&src, (2.0, 0.5), (4.0, 0.5), SelectionMode::Cell, false);
        assert_eq!(s.text(&src), "一b");
        assert_eq!([s.lo(0), s.hi(0, 6)], [1, 4]);
    }

    #[test]
    fn double_click_word_selects_non_boundary_or_boundary_runs_and_crosses_soft_wrap() {
        let src = plain_screen(&["ls -la /usr/lib \"my file\" a,b", "foo(bar)  x"], 40);
        let word = |x: f64, y: f64| {
            let mut s = TerminalSelection::new();
            s.start(&src, x, y, SelectionMode::Word, false);
            s.text(&src)
        };

        assert_eq!(word(1.0, 0.5), "ls");
        assert_eq!(word(4.5, 0.5), "-la");
        assert_eq!(word(8.0, 0.5), "/usr/lib");
        assert_eq!(word(17.0, 0.5), "my");
        assert_eq!(word(16.0, 0.5), " \"");
        assert_eq!(word(26.0, 0.5), "a");
        assert_eq!(word(27.0, 0.5), ",");
        assert_eq!(word(2.0, 1.5), "foo");
        assert_eq!(word(8.5, 1.5), ")");

        // Row 0 wraps onto row 1 (Screen::wrapped[0] = true).
        let wrap = screen_from_lines(&["echo hello", "world done"], 10, &[0], |_| false);
        let mut s = TerminalSelection::new();
        s.start(&wrap, 7.0, 0.5, SelectionMode::Word, false);
        assert_eq!(s.text(&wrap), "helloworld");
        assert_eq!([s.r0, s.c0, s.r1, s.c1], [0, 5, 1, 5]);

        assert!(s.extend(&wrap, 7.0, 1.5));
        assert_eq!(s.text(&wrap), "helloworld done");

        let mut custom = TerminalSelection::with_word_chars("/");
        custom.start(&src, 8.0, 0.5, SelectionMode::Word, false);
        assert_eq!(custom.text(&src), "usr");
    }

    #[test]
    fn triple_click_line_joins_soft_wraps_and_leaves_outer_blanks_out() {
        // Row 1 ("a long one tha") soft-wraps onto row 2 ("t wraps"): Screen::wrapped[1] = true.
        let src = screen_from_lines(
            &["  first line  ", "a long one tha", "t wraps", "", "last"],
            14,
            &[1],
            |_| false,
        );
        let line = |y: f64| {
            let mut s = TerminalSelection::new();
            s.start(&src, 3.0, y, SelectionMode::Line, false);
            s
        };

        let l0 = line(0.5);
        assert_eq!(l0.text(&src), "first line");
        assert_eq!([l0.c0, l0.c1], [2, 12]);
        assert_eq!(line(2.5).text(&src), "a long one that wraps");
        assert_eq!(line(1.5).text(&src), "a long one that wraps");
        assert!(line(3.5).is_empty());

        let mut dragged = line(0.5);
        dragged.extend(&src, 0.0, 4.5);
        assert_eq!(dragged.text(&src), "first line\na long one that wraps\n\nlast");
    }

    #[test]
    fn scroll_shifts_selection_up_and_drops_when_off_top() {
        let src = plain_screen(&["a", "b", "c", "d"], 4);
        let mut s = select_between(&src, (0.0, 2.5), (1.0, 3.5), SelectionMode::Cell, false);
        assert!(s.shift(1));
        assert_eq!([s.r0, s.r1], [1, 2]);
        assert!(!s.shift(2));
        assert!(!s.active);
    }

    #[test]
    fn screen_wraps_wide_cells_and_clusters_work_together() {
        // Mirrors the emulator TextSource test ("the quick brown fox\r\n一x e\u{0301}!") on a real
        // term_cells::Screen of 10 cols x 4 rows.
        let mut screen = screen_from_lines(
            &["the quick ", "brown fox", "", ""],
            10,
            &[0],
            |ch| ch as u32 == 0x4E00,
        );
        screen.clusters.push("e\u{0301}".to_owned());
        {
            let row2 = screen.row_mut(2);
            row2[0] = Cell {
                content: 0x4E00,
                width: 2,
                ..Cell::BLANK
            };
            row2[1] = Cell {
                content: 0,
                width: 0,
                ..Cell::BLANK
            };
            row2[2] = Cell {
                content: 'x' as u32,
                width: 1,
                ..Cell::BLANK
            };
            row2[3] = Cell::BLANK; // blank cell (content 0, width 1) reads as ' '
            row2[4] = Cell {
                content: CLUSTER_BASE,
                width: 1,
                ..Cell::BLANK
            };
            row2[5] = Cell {
                content: '!' as u32,
                width: 1,
                ..Cell::BLANK
            };
        }
        screen.touch_row(2);

        assert_eq!(cell_code(&screen, 2, 0), 0x4E00);
        assert_eq!(cell_code(&screen, 2, 4), 0x65); // 'e' base of cluster "e\u{0301}"

        let s = select_between(&screen, (0.0, 0.5), (10.0, 2.5), SelectionMode::Cell, false);
        assert_eq!(s.text(&screen), "the quick brown fox\n一x e\u{0301}!");

        let mut w = TerminalSelection::new();
        w.start(&screen, 7.0, 0.5, SelectionMode::Word, false);
        assert_eq!(w.text(&screen), "quick");
        w.start(&screen, 2.0, 1.5, SelectionMode::Word, false);
        assert_eq!(w.text(&screen), "brown");
        w.start(&screen, 0.5, 1.5, SelectionMode::Line, false);
        assert_eq!(w.text(&screen), "the quick brown fox");
    }

    #[test]
    fn set_clamps_orders_and_widens_stream_or_block_and_extend_reports_changes() {
        let src = screen_from_lines(&["a一b", "wxyz"], 6, &[], |ch| ch as u32 == 0x4E00);
        let mut s = TerminalSelection::new();

        // Reversed stream endpoints swap into reading order and widen across '一'.
        s.set(&src, 4, 0, 2, 0, false);
        assert_eq!([s.r0, s.c0, s.r1, s.c1], [0, 1, 0, 4]);
        assert_eq!(s.text(&src), "一b");

        // Out-of-bounds coordinates clamp cleanly to screen limits.
        s.set(&src, -5, -2, 99, 99, true);
        assert_eq!([s.r0, s.c0, s.r1, s.c1], [0, 0, 1, 6]);

        // Same cell boundary on extend reports false; moving past a boundary reports true.
        s.start(&src, 0.2, 1.5, SelectionMode::Cell, false);
        assert!(!s.extend(&src, 0.4, 1.7));
        assert!(s.extend(&src, 2.6, 1.5));
        assert_eq!(s.text(&src), "wxy");
    }
}
