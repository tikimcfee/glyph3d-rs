//! A terminal screen: active grid + scrollback + cursor + margins.
//!
//! Semantics ported from ghostty `src/terminal/Screen.zig` / `PageList.zig`
//! on a plain rows-in-a-VecDeque store: the active area is the last
//! `rows` entries conceptually; here it is a separate `Vec<Row>` with
//! scrollback rows moving into a `VecDeque` as lines scroll off the top.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::term::charsets::CharsetState;
use crate::term::page::{Cell, Row, SemanticPrompt};
use crate::term::style::Style;

/// DECSCUSR cursor styles (ghostty ansi.zig CursorStyle + cursor.zig).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    #[default]
    Default,
    BlinkingBlock,
    SteadyBlock,
    BlinkingUnderline,
    SteadyUnderline,
    BlinkingBar,
    SteadyBar,
}

#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    /// The current pen.
    pub style: Style,
    /// Set when a char was written in the last column; the next print
    /// wraps first (deferred/pending wrap, the classic VT quirk).
    pub pending_wrap: bool,
}

/// Everything DECSC saves (ghostty saved cursor: position, pen, charsets,
/// origin mode, pending wrap).
#[derive(Clone, Debug)]
pub struct SavedCursor {
    pub x: usize,
    pub y: usize,
    pub style: Style,
    pub pending_wrap: bool,
    pub origin: bool,
    pub charsets: CharsetState,
}

/// Receives every row as it leaves the active grid for scrollback (after
/// trimming), in order, on the thread that feeds the terminal. An owner
/// that keeps its own copy of the history (a log, an index, a zoomed-out
/// view) appends from here instead of polling `scrollback`.
///
/// Only a history screen (`max_scrollback > 0`) calls it, and only for rows
/// that enter scrollback by scrolling or by a shrinking height. A height
/// that grows pulls rows back out of scrollback; they are sent again when
/// they leave again. A width change re-wraps the history in place
/// (`resize_reflow`) and sends nothing; neither does `clear_all`.
pub type ScrollbackSink = Arc<dyn Fn(&Row) + Send + Sync>;

/// Scroll activity accumulated on a [`Screen`] since the last `Vt::fill`.
#[derive(Clone, Debug, Default)]
pub struct PendingScroll {
    /// Rows scrolled off row `0` (`scroll_top == 0 && full_width_margins()`,
    /// excluding `CSI M` line deletions).
    pub scrolled_top: u32,
    /// True if at least one full-screen upward scroll (`0..rows`, full width,
    /// not `CSI M`) occurred since the last fill.
    pub saw_full_screen_up: bool,
    /// True if any margin-bounded scroll (`DECSTBM` / `DECSLRM`), downward
    /// scroll (`RI` / `CSI T` / `CSI L`), or `CSI M` line deletion occurred.
    pub saw_margin_scroll: bool,
    /// Per-row half-open column span `(lo, hi)` (`lo < hi`) of cells moved by
    /// margin/downward/delete-line scrolls since the last fill.
    pub margin_moved_cols: Vec<(u16, u16)>,
}

impl PendingScroll {
    pub fn reset(&mut self) {
        self.scrolled_top = 0;
        self.saw_full_screen_up = false;
        self.saw_margin_scroll = false;
        self.margin_moved_cols.fill((0, 0));
    }

    pub fn record_margin_move(
        &mut self,
        rows: usize,
        cols: usize,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    ) {
        if self.margin_moved_cols.len() != rows {
            self.margin_moved_cols.resize(rows, (0, 0));
        }
        let full_hi = cols.min(u16::MAX as usize) as u16;
        if self.saw_full_screen_up {
            // Earlier in the same batch the whole screen scrolled up and now a
            // sub-region or downward scroll moved rows non-uniformly: mark all
            // rows moved in place.
            self.margin_moved_cols.fill((0, full_hi));
            self.saw_full_screen_up = false;
        }
        self.saw_margin_scroll = true;
        let lo = left.min(cols).min(u16::MAX as usize) as u16;
        let hi = (right.saturating_add(1)).min(cols).min(u16::MAX as usize) as u16;
        if lo < hi && rows > 0 {
            let end = bottom.min(rows - 1);
            for slot in &mut self.margin_moved_cols[top.min(end)..=end] {
                if slot.0 >= slot.1 {
                    *slot = (lo, hi);
                } else {
                    slot.0 = slot.0.min(lo);
                    slot.1 = slot.1.max(hi);
                }
            }
        }
    }
}

pub struct Screen {
    pub cols: usize,
    pub rows: usize,

    /// The active grid, exactly `rows` entries.
    pub active: Vec<Row>,
    /// Scrollback, oldest first. Empty for the alternate screen.
    pub scrollback: VecDeque<Row>,
    pub max_scrollback: usize,
    /// Rows ever evicted off the front of scrollback; makes
    /// `evicted + index` a stable absolute row id for selection.
    pub evicted: u64,

    pub cursor: Cursor,
    pub saved_cursor: Option<SavedCursor>,
    pub charsets: CharsetState,

    /// Scroll region, 0-based inclusive.
    pub scroll_top: usize,
    pub scroll_bottom: usize,
    /// Left/right margins (DECSLRM), 0-based inclusive.
    pub left_margin: usize,
    pub right_margin: usize,

    /// Grapheme-cluster state for the print path (mode 2027): x/y of the
    /// previously printed cell, to append combining input.
    pub previous_char: Option<(usize, usize)>,

    /// Scroll tracking accumulated between `Vt::fill` calls.
    pub pending_scroll: PendingScroll,

    /// See [`ScrollbackSink`]; none by default.
    pub scrollback_sink: Option<ScrollbackSink>,
}

impl Screen {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            active: (0..rows).map(|_| Row::new()).collect(),
            scrollback: VecDeque::new(),
            max_scrollback,
            evicted: 0,
            cursor: Cursor::default(),
            saved_cursor: None,
            charsets: CharsetState::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            left_margin: 0,
            right_margin: cols - 1,
            previous_char: None,
            pending_scroll: PendingScroll::default(),
            scrollback_sink: None,
        }
    }

    // ------------------------------------------------------------------
    // Row access
    // ------------------------------------------------------------------

    #[inline]
    pub fn row(&self, y: usize) -> &Row {
        &self.active[y]
    }

    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut Row {
        &mut self.active[y]
    }

    // ------------------------------------------------------------------
    // Margins
    // ------------------------------------------------------------------

    /// True when the horizontal margins cover the full width.
    #[inline]
    pub fn full_width_margins(&self) -> bool {
        self.left_margin == 0 && self.right_margin == self.cols - 1
    }

    pub fn reset_margins(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.left_margin = 0;
        self.right_margin = self.cols - 1;
    }

    // ------------------------------------------------------------------
    // Scrolling
    // ------------------------------------------------------------------

    /// Scroll the scroll region up by `count`: rows leave at the top. When
    /// the region is the full screen width and starts at row 0 and this is
    /// a scrollback screen, evicted rows go to history. Blank rows appear
    /// at the bottom carrying the pen's bg (`style`).
    pub fn scroll_up(&mut self, count: usize, style: &Style) {
        self.scroll_up_inner(count, style, false);
    }

    /// Scroll up for `CSI M` (`DL`, delete lines): shifts `scroll_top..=scroll_bottom`
    /// upward without counting as output scrolled off the top of the screen.
    pub fn scroll_up_delete_lines(&mut self, count: usize, style: &Style) {
        self.scroll_up_inner(count, style, true);
    }

    fn scroll_up_inner(&mut self, count: usize, style: &Style, is_delete_lines: bool) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top) + 1);
        if count == 0 {
            return;
        }

        if is_delete_lines {
            self.pending_scroll.record_margin_move(
                self.rows,
                self.cols,
                self.scroll_top,
                self.scroll_bottom,
                self.left_margin,
                self.right_margin,
            );
        } else if self.scroll_top == 0
            && self.scroll_bottom + 1 == self.rows
            && self.full_width_margins()
        {
            self.pending_scroll.scrolled_top = self
                .pending_scroll
                .scrolled_top
                .saturating_add(count as u32);
            if self.pending_scroll.saw_margin_scroll {
                self.pending_scroll.record_margin_move(
                    self.rows,
                    self.cols,
                    0,
                    self.rows.saturating_sub(1),
                    0,
                    self.cols.saturating_sub(1),
                );
            } else {
                self.pending_scroll.saw_full_screen_up = true;
            }
        } else {
            if self.scroll_top == 0 && self.full_width_margins() {
                self.pending_scroll.scrolled_top = self
                    .pending_scroll
                    .scrolled_top
                    .saturating_add(count as u32);
            }
            self.pending_scroll.record_margin_move(
                self.rows,
                self.cols,
                self.scroll_top,
                self.scroll_bottom,
                self.left_margin,
                self.right_margin,
            );
        }

        if self.full_width_margins() {
            let to_history = self.scroll_top == 0 && self.max_scrollback > 0;
            if to_history {
                for _ in 0..count {
                    let mut row = std::mem::take(&mut self.active[self.scroll_top]);
                    for y in self.scroll_top..self.scroll_bottom {
                        self.active.swap(y, y + 1);
                    }
                    row.trim();
                    if let Some(sink) = &self.scrollback_sink {
                        sink(&row);
                    }
                    self.scrollback.push_back(row);
                    self.active[self.scroll_bottom].clear(style, self.cols);
                }
                for y in self.scroll_top..=self.scroll_bottom {
                    self.active[y].dirty = true;
                }
                if self.scrollback.len() > self.max_scrollback {
                    let overflow = self.scrollback.len() - self.max_scrollback;
                    for _ in 0..overflow {
                        self.scrollback.pop_front();
                    }
                    self.evicted += overflow as u64;
                }
            } else {
                // Recycle the evicted rows' Vec<Cell> capacity in place and shift via a
                // single slice rotation (`memmove`). Full-screen scrolls already disable
                // per-row dirty skipping via `pending_scroll`, avoiding walking every row
                // on each `\n`.
                for row in &mut self.active[self.scroll_top..self.scroll_top + count] {
                    row.clear(style, self.cols);
                }
                self.active[self.scroll_top..=self.scroll_bottom].rotate_left(count);
                if self.scroll_top > 0 || self.scroll_bottom + 1 < self.rows {
                    for y in self.scroll_top..=self.scroll_bottom {
                        self.active[y].dirty = true;
                    }
                }
            }
        } else {
            // Margin-limited scroll: move cell spans, never touches history.
            let (l, r) = (self.left_margin, self.right_margin);
            for y in self.scroll_top..=self.scroll_bottom {
                let src_y = y + count;
                if src_y <= self.scroll_bottom {
                    let mut src: Vec<Cell> = (l..=r)
                        .map(|x| self.active[src_y].cell(x).cloned().unwrap_or_default())
                        .collect();
                    if let Some(first) = src.first_mut() {
                        if first.content == crate::term::page::CellContent::WideTail {
                            *first = Cell::blank_with_bg(&first.style);
                        }
                    }
                    if let Some(last) = src.last_mut() {
                        if last.content.is_wide_head() {
                            *last = Cell::blank_with_bg(&last.style);
                        }
                    }
                    let dst = &mut self.active[y];
                    dst.split_wide_at(l, style);
                    dst.split_wide_at(r, style);
                    for (i, cell) in src.into_iter().enumerate() {
                        *dst.cell_mut(l + i) = cell;
                    }
                    dst.dirty = true;
                } else {
                    let blank = Cell::blank_with_bg(style);
                    let dst = &mut self.active[y];
                    dst.split_wide_at(l, style);
                    dst.split_wide_at(r, style);
                    for x in l..=r {
                        *dst.cell_mut(x) = blank.clone();
                    }
                    dst.dirty = true;
                }
            }
        }
    }

    /// Scroll the scroll region down by `count`: blank rows appear at the
    /// top. Never touches scrollback.
    pub fn scroll_down(&mut self, count: usize, style: &Style) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top) + 1);
        if count == 0 {
            return;
        }
        self.pending_scroll.record_margin_move(
            self.rows,
            self.cols,
            self.scroll_top,
            self.scroll_bottom,
            self.left_margin,
            self.right_margin,
        );

        if self.full_width_margins() {
            for _ in 0..count {
                let mut recycled = self.active.remove(self.scroll_bottom);
                recycled.clear(style, self.cols);
                self.active.insert(self.scroll_top, recycled);
            }
            for y in self.scroll_top..=self.scroll_bottom {
                self.active[y].dirty = true;
            }
        } else {
            let (l, r) = (self.left_margin, self.right_margin);
            for y in (self.scroll_top..=self.scroll_bottom).rev() {
                if y >= self.scroll_top + count {
                    let src_y = y - count;
                    let mut src: Vec<Cell> = (l..=r)
                        .map(|x| self.active[src_y].cell(x).cloned().unwrap_or_default())
                        .collect();
                    if let Some(first) = src.first_mut() {
                        if first.content == crate::term::page::CellContent::WideTail {
                            *first = Cell::blank_with_bg(&first.style);
                        }
                    }
                    if let Some(last) = src.last_mut() {
                        if last.content.is_wide_head() {
                            *last = Cell::blank_with_bg(&last.style);
                        }
                    }
                    let dst = &mut self.active[y];
                    dst.split_wide_at(l, style);
                    dst.split_wide_at(r, style);
                    for (i, cell) in src.into_iter().enumerate() {
                        *dst.cell_mut(l + i) = cell;
                    }
                    dst.dirty = true;
                } else {
                    let blank = Cell::blank_with_bg(style);
                    let dst = &mut self.active[y];
                    dst.split_wide_at(l, style);
                    dst.split_wide_at(r, style);
                    for x in l..=r {
                        *dst.cell_mut(x) = blank.clone();
                    }
                    dst.dirty = true;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Resize
    // ------------------------------------------------------------------

    /// Resize without reflow: clamp/pad rows and columns, clamp cursor.
    /// Used for the alternate screen (TUIs fully redraw on SIGWINCH).
    pub fn resize_clamped(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if rows > self.rows {
            for _ in self.rows..rows {
                self.active.push(Row::new());
            }
        } else {
            self.active.truncate(rows);
        }
        for row in &mut self.active {
            if row.cells.len() > cols {
                row.cells.truncate(cols);
                row.wrapped = false;
            }
            // Never leave half a wide char at the right edge (incl. cols == 1).
            if row.cells.len() == cols {
                if let Some(last) = row.cells.last_mut() {
                    if last.content.is_wide_head() {
                        *last = Cell::blank_with_bg(&last.style);
                    }
                }
            }
            row.dirty = true;
        }
        self.cols = cols;
        self.rows = rows;
        self.cursor.x = self.cursor.x.min(cols - 1);
        self.cursor.y = self.cursor.y.min(rows - 1);
        self.cursor.pending_wrap = false;
        if let Some(saved) = &mut self.saved_cursor {
            saved.x = saved.x.min(cols - 1);
            saved.y = saved.y.min(rows - 1);
            saved.pending_wrap = false;
        }
        self.reset_margins();
        self.previous_char = None;
    }

    /// Resize with reflow (primary screen). Logical lines (joined on the
    /// soft-wrap flag) are re-wrapped at the new width; the cursor tracks
    /// its position within its logical line, ghostty-style.
    pub fn resize_reflow(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols {
            self.resize_rows_only(rows);
            return;
        }

        // 1. Collect logical lines over scrollback + active, tracking the
        //    cursor as (logical line index, cell offset within the line).
        let cursor_virtual = self.scrollback.len() + self.cursor.y;
        let mut logical: Vec<(Vec<Cell>, SemanticPrompt)> = Vec::new();
        let mut cursor_pos: Option<(usize, usize)> = None;

        let scrollback = std::mem::take(&mut self.scrollback);
        let active = std::mem::take(&mut self.active);
        let all_rows = scrollback.into_iter().chain(active);

        let mut current: Vec<Cell> = Vec::new();
        let mut current_semantic = SemanticPrompt::Output;
        let mut current_start = true;
        for (virt_idx, mut row) in all_rows.enumerate() {
            if current_start {
                current_semantic = row.semantic;
                current_start = false;
            }
            row.trim();
            let is_cursor_row = virt_idx == cursor_virtual;
            if is_cursor_row {
                // Cursor offset within the logical line. May point past
                // the trimmed content; store the raw offset.
                cursor_pos = Some((logical.len(), current.len() + self.cursor.x));
            }
            // Drop a spacer head at the join point: it only existed
            // because of the old width.
            let mut cells = row.cells;
            if row.wrapped {
                if cells
                    .last()
                    .map(|c| c.content == crate::term::page::CellContent::WideSpacerHead)
                    .unwrap_or(false)
                {
                    cells.pop();
                }
                current.extend(cells);
            } else {
                current.extend(cells);
                logical.push((std::mem::take(&mut current), current_semantic));
                current_start = true;
            }
        }
        if !current.is_empty() {
            logical.push((current, current_semantic));
        }
        if logical.is_empty() {
            logical.push((Vec::new(), SemanticPrompt::Output));
        }

        // 2. Re-wrap each logical line at the new width.
        let mut new_rows: Vec<Row> = Vec::new();
        let mut new_cursor: Option<(usize, usize)> = None;
        for (line_idx, (cells, semantic)) in logical.into_iter().enumerate() {
            let cursor_off = match cursor_pos {
                Some((l, off)) if l == line_idx => Some(off),
                _ => None,
            };
            let first_new_row = new_rows.len();
            let mut row = Row::new();
            row.semantic = semantic;
            let mut x = 0usize;
            let mut cell_index = 0usize;
            let total = cells.len();
            let mut iter = cells.into_iter();
            loop {
                if let Some(off) = cursor_off {
                    if off == cell_index {
                        new_cursor = Some((new_rows.len(), x.min(cols - 1)));
                    }
                }
                let Some(cell) = iter.next() else { break };
                if cell.content == crate::term::page::CellContent::WideTail {
                    // WideTail cells are regenerated directly alongside their
                    // wide head below; skip old tails instead of emitting a
                    // third column or an orphan tail at column 0.
                    cell_index += 1;
                    continue;
                }
                let width = cell.content.width().max(1) as usize;
                if width == 2 && cols == 1 {
                    // A 1-column screen cannot fit a 2-column glyph.
                    if x >= 1 {
                        row.wrapped = true;
                        new_rows.push(std::mem::take(&mut row));
                        row.semantic = semantic;
                    }
                    *row.cell_mut(0) = Cell::blank_with_bg(&cell.style);
                    x = 1;
                    cell_index += 1;
                    continue;
                }
                if x + width > cols {
                    // Wrap. A wide char that doesn't fit leaves a spacer.
                    if width == 2 && x < cols {
                        let mut spacer = Cell::blank_with_bg(&cell.style);
                        spacer.content = crate::term::page::CellContent::WideSpacerHead;
                        *row.cell_mut(x) = spacer;
                    }
                    row.wrapped = true;
                    new_rows.push(std::mem::take(&mut row));
                    row.semantic = semantic;
                    x = 0;
                }
                let head_style = cell.style;
                let head_link = cell.hyperlink;
                *row.cell_mut(x) = cell;
                if width == 2 {
                    *row.cell_mut(x + 1) = Cell {
                        content: crate::term::page::CellContent::WideTail,
                        style: head_style,
                        hyperlink: head_link,
                    };
                }
                x += width;
                cell_index += 1;
                let _ = total;
            }
            // Cursor past the end of the line's content.
            if let Some(off) = cursor_off {
                if off >= cell_index && new_cursor.is_none() {
                    let extra = off - cell_index;
                    let cx = (x + extra).min(cols - 1);
                    new_cursor = Some((new_rows.len(), cx));
                }
            }
            new_rows.push(row);
            let _ = first_new_row;
        }

        // Trailing blank rows are dropped, not history: without this a
        // mostly-empty screen would push its content into scrollback on
        // every narrow-resize.
        let mut keep = new_rows.len();
        while keep > 1 && new_rows[keep - 1].cells.is_empty() {
            keep -= 1;
        }
        if let Some((cy, _)) = new_cursor {
            keep = keep.max(cy + 1);
        }
        new_rows.truncate(keep);

        // 3. Split into scrollback + active, bottom-anchored, keeping the
        //    cursor inside the active area.
        let total = new_rows.len();
        let mut start = total.saturating_sub(rows);
        if let Some((cy, _)) = new_cursor {
            if cy < start {
                start = cy;
            }
        }
        let mut scrollback: VecDeque<Row> = VecDeque::new();
        let mut active: Vec<Row> = Vec::new();
        for (i, row) in new_rows.into_iter().enumerate() {
            if i < start {
                scrollback.push_back(row);
            } else if active.len() < rows {
                active.push(row);
            } else {
                // Rows below the kept window (only possible when the
                // cursor forced `start` up). Content below the cursor is
                // preserved by dropping history instead.
                scrollback.push_back(active.remove(0));
                active.push(row);
                start += 1;
            }
        }
        while active.len() < rows {
            active.push(Row::new());
        }
        if self.max_scrollback == 0 {
            scrollback.clear();
        } else if scrollback.len() > self.max_scrollback {
            let overflow = scrollback.len() - self.max_scrollback;
            for _ in 0..overflow {
                scrollback.pop_front();
            }
            self.evicted += overflow as u64;
        }

        self.cols = cols;
        self.rows = rows;
        self.scrollback = scrollback;
        self.active = active;
        if let Some((cy, cx)) = new_cursor {
            self.cursor.y = cy.saturating_sub(start).min(rows - 1);
            self.cursor.x = cx.min(cols - 1);
        } else {
            self.cursor.y = self.cursor.y.min(rows - 1);
            self.cursor.x = self.cursor.x.min(cols - 1);
        }
        self.cursor.pending_wrap = false;
        // Saved cursor: clamp (precision here is not worth the complexity).
        if let Some(saved) = &mut self.saved_cursor {
            saved.x = saved.x.min(cols - 1);
            saved.y = saved.y.min(rows - 1);
            saved.pending_wrap = false;
        }
        self.reset_margins();
        self.previous_char = None;
    }

    /// Height-only resize for the scrollback screen: grow pulls rows back
    /// out of history (bottom-anchored, like ghostty/macOS Terminal),
    /// shrink drops blank rows below the cursor first then pushes rows
    /// into history.
    fn resize_rows_only(&mut self, rows: usize) {
        use std::cmp::Ordering;
        match rows.cmp(&self.rows) {
            Ordering::Equal => {}
            Ordering::Greater => {
                let mut add = rows - self.rows;
                while add > 0 {
                    if let Some(row) = self.scrollback.pop_back() {
                        self.active.insert(0, row);
                        self.cursor.y += 1;
                        if let Some(s) = &mut self.saved_cursor {
                            s.y = (s.y + 1).min(rows - 1);
                        }
                    } else {
                        self.active.push(Row::new());
                    }
                    add -= 1;
                }
            }
            Ordering::Less => {
                let mut remove = self.rows - rows;
                while remove > 0 {
                    let last_used = self.last_used_row();
                    if self.active.len() - 1 > last_used && self.active.len() - 1 > self.cursor.y {
                        // Blank row below all content and the cursor.
                        self.active.pop();
                    } else {
                        let mut row = self.active.remove(0);
                        if self.max_scrollback > 0 {
                            row.trim();
                            if let Some(sink) = &self.scrollback_sink {
                                sink(&row);
                            }
                            self.scrollback.push_back(row);
                            if self.scrollback.len() > self.max_scrollback {
                                self.scrollback.pop_front();
                                self.evicted += 1;
                            }
                        }
                        self.cursor.y = self.cursor.y.saturating_sub(1);
                        if let Some(s) = &mut self.saved_cursor {
                            s.y = s.y.saturating_sub(1);
                        }
                    }
                    remove -= 1;
                }
            }
        }
        for row in &mut self.active {
            row.dirty = true;
        }
        self.rows = rows;
        self.cursor.y = self.cursor.y.min(rows - 1);
        if let Some(s) = &mut self.saved_cursor {
            s.y = s.y.min(rows - 1);
        }
        self.reset_margins();
        self.previous_char = None;
    }

    /// Index of the last row with any content.
    pub fn last_used_row(&self) -> usize {
        for y in (0..self.active.len()).rev() {
            if !self.active[y].cells.is_empty() {
                return y;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::page::CellContent;

    fn put(screen: &mut Screen, y: usize, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            *screen.row_mut(y).cell_mut(i) = Cell {
                content: CellContent::Char(ch),
                ..Default::default()
            };
        }
    }

    #[test]
    fn scrollback_sink_sees_rows_leaving_the_grid() {
        use std::sync::Mutex;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut s = Screen::new(3, 2, 10);
        let sink_seen = seen.clone();
        s.scrollback_sink = Some(Arc::new(move |row: &Row| sink_seen.lock().unwrap().push(row.text())));
        let style = Style::default();
        put(&mut s, 0, "a");
        put(&mut s, 1, "b");
        s.scroll_up(1, &style);
        assert_eq!(*seen.lock().unwrap(), vec!["a".to_string()]);
        // A margin-limited region never feeds history, nor the sink.
        s.scroll_top = 1;
        s.scroll_up(1, &style);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn scroll_up_moves_to_history() {
        let mut s = Screen::new(10, 3, 100);
        put(&mut s, 0, "aaa");
        put(&mut s, 1, "bbb");
        put(&mut s, 2, "ccc");
        s.scroll_up(1, &Style::default());
        assert_eq!(s.scrollback.len(), 1);
        assert_eq!(s.scrollback[0].text(), "aaa");
        assert_eq!(s.row(0).text(), "bbb");
        assert_eq!(s.row(2).text(), "");
    }

    #[test]
    fn scroll_region_does_not_touch_history() {
        let mut s = Screen::new(10, 4, 100);
        put(&mut s, 0, "top");
        put(&mut s, 1, "aaa");
        put(&mut s, 2, "bbb");
        put(&mut s, 3, "bot");
        s.scroll_top = 1;
        s.scroll_bottom = 2;
        s.scroll_up(1, &Style::default());
        assert_eq!(s.scrollback.len(), 0);
        assert_eq!(s.row(0).text(), "top");
        assert_eq!(s.row(1).text(), "bbb");
        assert_eq!(s.row(2).text(), "");
        assert_eq!(s.row(3).text(), "bot");
    }

    #[test]
    fn reflow_wider_unwraps() {
        let mut s = Screen::new(5, 3, 100);
        put(&mut s, 0, "abcde");
        s.row_mut(0).wrapped = true;
        put(&mut s, 1, "fgh");
        s.cursor.x = 3;
        s.cursor.y = 1;
        s.resize_reflow(10, 3);
        assert_eq!(s.row(0).text(), "abcdefgh");
        assert!(!s.row(0).wrapped);
        assert_eq!((s.cursor.x, s.cursor.y), (8, 0));
    }

    #[test]
    fn reflow_narrower_rewraps() {
        let mut s = Screen::new(10, 3, 100);
        put(&mut s, 0, "abcdefgh");
        s.cursor.x = 8;
        s.cursor.y = 0;
        s.resize_reflow(5, 3);
        assert_eq!(s.row(0).text(), "abcde");
        assert!(s.row(0).wrapped);
        assert_eq!(s.row(1).text(), "fgh");
        assert_eq!((s.cursor.x, s.cursor.y), (3, 1));
    }

    #[test]
    fn rows_grow_pulls_history() {
        let mut s = Screen::new(10, 2, 100);
        put(&mut s, 0, "aaa");
        put(&mut s, 1, "bbb");
        s.scroll_up(1, &Style::default());
        put(&mut s, 1, "ccc");
        // history: aaa; active: bbb, ccc
        s.resize_reflow(10, 3);
        assert_eq!(s.scrollback.len(), 0);
        assert_eq!(s.row(0).text(), "aaa");
        assert_eq!(s.row(1).text(), "bbb");
        assert_eq!(s.row(2).text(), "ccc");
    }

    #[test]
    fn rows_shrink_drops_blank_bottom_first() {
        let mut s = Screen::new(10, 4, 100);
        put(&mut s, 0, "aaa");
        put(&mut s, 1, "bbb");
        s.cursor.y = 1;
        s.resize_reflow(10, 2);
        assert_eq!(s.scrollback.len(), 0);
        assert_eq!(s.row(0).text(), "aaa");
        assert_eq!(s.row(1).text(), "bbb");
        assert_eq!(s.cursor.y, 1);
    }
}
