//! A terminal's screen as plain data: what an emulator hands a renderer.
//!
//! This crate is the seam between the two (2026-11-26, the wall): `glyph-vt`
//! fills a [`Screen`] from a terminal's bytes, and the cell renderer in
//! `native/src/wall` draws one. Neither knows the other. Nothing here does
//! any work beyond holding and indexing the data, so it has no dependencies.
//!
//! A [`Cell`] is drawn as its producer resolved it: colours are final sRGB,
//! with inverse, conceal and "bold is bright" already applied. A renderer
//! never looks at a palette.

/// A cell's attributes, one bit per SGR attribute. The layout is the JS
/// renderer's `core/cellAttrs.js`, which is also the layout termd's packed
/// cells are planned to carry, so a word can be copied across unchanged.
pub mod attrs {
    /// SGR 1. Picks the bold face; changes no colour.
    pub const BOLD: u16 = 1 << 0;
    /// SGR 2 (faint).
    pub const DIM: u16 = 1 << 1;
    /// SGR 3. Picks the italic face.
    pub const ITALIC: u16 = 1 << 2;
    /// SGR 5/6. Held, not drawn.
    pub const BLINK: u16 = 1 << 3;
    /// SGR 7. Already applied to `fg`/`bg`; kept so a reader can tell.
    pub const INVERSE: u16 = 1 << 4;
    /// SGR 8. Already applied (the content is blank); kept so a reader can tell.
    pub const CONCEAL: u16 = 1 << 5;
    /// SGR 9.
    pub const STRIKE: u16 = 1 << 6;
    /// SGR 53.
    pub const OVERLINE: u16 = 1 << 7;
    /// Bits 8..10: the underline style, one of the `UNDERLINE_*` values.
    pub const UNDERLINE_SHIFT: u16 = 8;
    pub const UNDERLINE_MASK: u16 = 7 << UNDERLINE_SHIFT;
    /// SGR 58 named a colour for the underline: it is the cell's `ul`.
    /// Without this bit the underline takes the text's colour.
    pub const UNDERLINE_COLOR: u16 = 1 << 11;

    pub const UNDERLINE_NONE: u16 = 0;
    pub const UNDERLINE_SINGLE: u16 = 1;
    pub const UNDERLINE_DOUBLE: u16 = 2;
    pub const UNDERLINE_CURLY: u16 = 3;
    pub const UNDERLINE_DOTTED: u16 = 4;
    pub const UNDERLINE_DASHED: u16 = 5;

    /// The underline style of an attributes word (`UNDERLINE_NONE` ..).
    pub const fn underline(attrs: u16) -> u16 {
        (attrs & UNDERLINE_MASK) >> UNDERLINE_SHIFT
    }

    /// The face a cell draws in: bit 0 bold, bit 1 italic ([`super::Face`]).
    pub const fn face(attrs: u16) -> u8 {
        ((attrs & BOLD != 0) as u8) | (((attrs & ITALIC != 0) as u8) << 1)
    }
}

/// Which of a font family's four faces draws a glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Face {
    #[default]
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

impl Face {
    pub const fn from_bits(bits: u8) -> Face {
        match bits & 3 {
            0 => Face::Regular,
            1 => Face::Bold,
            2 => Face::Italic,
            _ => Face::BoldItalic,
        }
    }
}

/// A colour as the engine's colour lanes hold it: sRGB, 8 bits a channel,
/// red in the low byte, alpha in the high one.
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16) | ((a as u32) << 24)
}

/// An opaque colour ([`rgba`] with alpha 255).
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    rgba(r, g, b, 255)
}

/// A `Cell::content` at or above this is a cluster: `content -
/// CLUSTER_BASE` indexes [`Screen::clusters`]. Below it, content is a
/// Unicode scalar value (every one is under 0x11_0000).
pub const CLUSTER_BASE: u32 = 0x11_0000;

/// One cell of the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Cell {
    /// What the cell shows: 0 for nothing, a Unicode scalar value, or a
    /// cluster (a base and its combining marks, or an emoji sequence) as
    /// `CLUSTER_BASE + index` into [`Screen::clusters`].
    pub content: u32,
    /// The colour the text draws in ([`rgba`] packing), always resolved.
    pub fg: u32,
    /// The cell's fill. Alpha 0 means no fill: the terminal's own
    /// background shows.
    pub bg: u32,
    /// The underline's own colour; read only under `attrs::UNDERLINE_COLOR`.
    pub ul: u32,
    /// The `attrs` flags.
    pub attrs: u16,
    /// How many columns the content spans: 1, or 2 for a wide character's
    /// first cell. Its second cell has width 0 and content 0, and carries
    /// the same `bg`.
    pub width: u8,
    pub _pad: u8,
}

impl Cell {
    /// A blank cell that draws nothing and fills nothing.
    pub const BLANK: Cell = Cell { content: 0, fg: 0, bg: 0, ul: 0, attrs: 0, width: 1, _pad: 0 };
}

/// The cursor's shape (DECSCUSR).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
}

/// Where the cursor is and how it draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    /// False while the program hid it (DECTCEM off) or a scrollback view
    /// is showing.
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

/// Which pointer events the program asked to hear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseTracking {
    #[default]
    Off,
    /// DECSET 9: presses only.
    X10,
    /// DECSET 1000: presses and releases.
    Normal,
    /// DECSET 1002: and moves with a button held.
    Button,
    /// DECSET 1003: and every move.
    Any,
}

/// The modes a host needs to route input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Modes {
    pub alt_screen: bool,
    pub mouse: MouseTracking,
    pub bracketed_paste: bool,
    pub app_cursor: bool,
    pub focus_events: bool,
}

/// A terminal's visible screen.
///
/// Rows are top to bottom, cells left to right, `cols * rows` of them.
/// `row_gen[r]` is the value of `generation` when row `r` last changed, so
/// a renderer that remembers the generation it last drew uploads only the
/// rows above it.
#[derive(Clone, Debug, Default)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    /// Per row: `generation` when it last changed.
    pub row_gen: Vec<u64>,
    /// Per row: the row continues on the next one (a soft wrap). Selection
    /// joins such rows into one line.
    pub wrapped: Vec<bool>,
    /// Bumped by every change to the cells, the cursor, the modes, the
    /// title or the size.
    pub generation: u64,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: String,
    /// The texts `Cell::content` refers to at `CLUSTER_BASE + index`.
    pub clusters: Vec<String>,
    /// The terminal's own colours: what an unstyled cell draws in, and the
    /// panel behind every cell with no fill.
    pub default_fg: u32,
    pub default_bg: u32,
}

impl Screen {
    /// A blank screen of `cols` by `rows`.
    pub fn new(cols: u16, rows: u16) -> Screen {
        let mut s = Screen { default_fg: rgb(0xd8, 0xde, 0xe9), default_bg: rgb(0x0d, 0x10, 0x17), ..Screen::default() };
        s.resize(cols, rows);
        s
    }

    /// Change the size, blanking every cell. Bumps `generation` and marks
    /// every row changed.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.generation += 1;
        self.cells.clear();
        self.cells.resize(cols as usize * rows as usize, Cell::BLANK);
        self.row_gen.clear();
        self.row_gen.resize(rows as usize, self.generation);
        self.wrapped.clear();
        self.wrapped.resize(rows as usize, false);
    }

    /// Row `r`'s cells.
    pub fn row(&self, r: u16) -> &[Cell] {
        let c = self.cols as usize;
        &self.cells[r as usize * c..(r as usize + 1) * c]
    }

    /// Row `r`'s cells, to write. The caller marks the row changed
    /// ([`Screen::touch_row`]) if it changed one.
    pub fn row_mut(&mut self, r: u16) -> &mut [Cell] {
        let c = self.cols as usize;
        &mut self.cells[r as usize * c..(r as usize + 1) * c]
    }

    /// Mark row `r` changed: bumps `generation` and stamps the row with it.
    pub fn touch_row(&mut self, r: u16) {
        self.generation += 1;
        self.row_gen[r as usize] = self.generation;
    }

    /// The text of a cell's content: the scalar as a string, the cluster's
    /// text, or empty.
    pub fn content_str(&self, cell: &Cell) -> std::borrow::Cow<'_, str> {
        if cell.content == 0 {
            std::borrow::Cow::Borrowed("")
        } else if cell.content >= CLUSTER_BASE {
            match self.clusters.get((cell.content - CLUSTER_BASE) as usize) {
                Some(s) => std::borrow::Cow::Borrowed(s.as_str()),
                None => std::borrow::Cow::Borrowed(""),
            }
        } else {
            match char::from_u32(cell.content) {
                Some(c) => std::borrow::Cow::Owned(c.to_string()),
                None => std::borrow::Cow::Borrowed(""),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_indexes_its_rows_and_stamps_what_changed() {
        let mut s = Screen::new(4, 3);
        assert_eq!(s.cells.len(), 12);
        let before = s.generation;
        s.row_mut(1)[2].content = 'x' as u32;
        s.touch_row(1);
        assert_eq!(s.row(1)[2].content, 'x' as u32);
        assert!(s.row_gen[1] > before);
        assert_eq!(s.row_gen[0], before);
        assert_eq!(s.content_str(&s.row(1)[2]), "x");
    }

    #[test]
    fn attributes_give_their_face_and_underline() {
        let a = attrs::BOLD | attrs::ITALIC | (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT);
        assert_eq!(Face::from_bits(attrs::face(a)), Face::BoldItalic);
        assert_eq!(attrs::underline(a), attrs::UNDERLINE_CURLY);
        assert_eq!(rgb(1, 2, 3), 0xff03_0201);
    }
}
