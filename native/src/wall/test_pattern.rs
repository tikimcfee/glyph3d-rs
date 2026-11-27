//! Deterministic test-pattern `Screen` builders for visual verification
//! (`--wall-test-pattern --screenshot OUT.png`).

use glam::Vec2;
use glyph_font::{FamilyId, FontAtlas};
use term_cells::{CLUSTER_BASE, Cell, Cursor, CursorShape, Screen, attrs, rgb};

use super::cell::CellSelection;
use super::labels::{Label, LabelHeight, measure_label_width};
use super::metrics::{CellMetrics, WorldRect};
use super::paper::PaperStyle;
use super::shapes::{DashStyle, LeaderShape, PolygonShape, RectShape, Shape};
use super::srgb_hex;

/// Colours and attribute flags for a styled run written into a test-pattern [`Screen`].
#[derive(Clone, Copy, Debug)]
pub struct RunStyle {
    pub fg: u32,
    pub bg: u32,
    pub ul: u32,
    pub attrs: u16,
}

impl RunStyle {
    pub const fn fg(fg: u32) -> Self {
        Self { fg, bg: 0, ul: 0, attrs: 0 }
    }

    pub const fn with_bg(mut self, bg: u32) -> Self {
        self.bg = bg;
        self
    }

    pub const fn with_ul(mut self, ul: u32) -> Self {
        self.ul = ul;
        self
    }

    pub const fn with_attrs(mut self, cell_attrs: u16) -> Self {
        self.attrs = cell_attrs;
        self
    }
}

/// Write an ASCII/UTF-8 string into `screen` starting at `(col, row)` with `style`.
pub fn write_run(screen: &mut Screen, row: u16, mut col: u16, text: &str, style: RunStyle) -> u16 {
    let cols = screen.cols;
    for ch in text.chars() {
        if col >= cols {
            break;
        }
        let wide = is_wide_test_char(ch);
        let width: u8 = if wide { 2 } else { 1 };
        if wide && col + 1 >= cols {
            break;
        }
        screen.row_mut(row)[col as usize] = Cell {
            content: ch as u32,
            fg: style.fg,
            bg: style.bg,
            ul: style.ul,
            attrs: style.attrs,
            width,
            _pad: 0,
        };
        if wide {
            screen.row_mut(row)[(col + 1) as usize] = Cell {
                content: 0,
                fg: 0,
                bg: style.bg,
                ul: 0,
                attrs: 0,
                width: 0,
                _pad: 0,
            };
        }
        col += u16::from(width);
    }
    screen.touch_row(row);
    col
}

/// Write a multi-codepoint cluster (e.g. ZWJ emoji sequence) as a 2-column cell.
pub fn write_cluster(screen: &mut Screen, row: u16, col: u16, cluster: &str, style: RunStyle) -> u16 {
    if col + 1 >= screen.cols {
        return col;
    }
    let idx = screen.clusters.len() as u32;
    screen.clusters.push(cluster.to_string());
    screen.row_mut(row)[col as usize] = Cell {
        content: CLUSTER_BASE + idx,
        fg: style.fg,
        bg: style.bg,
        ul: style.ul,
        attrs: style.attrs,
        width: 2,
        _pad: 0,
    };
    screen.row_mut(row)[(col + 1) as usize] = Cell {
        content: 0,
        fg: 0,
        bg: style.bg,
        ul: 0,
        attrs: 0,
        width: 0,
        _pad: 0,
    };
    screen.touch_row(row);
    col + 2
}

/// Write a 1-column multi-codepoint cluster (e.g. accented Latin base + combining marks).
pub fn write_narrow_cluster(
    screen: &mut Screen,
    row: u16,
    col: u16,
    cluster: &str,
    style: RunStyle,
) -> u16 {
    if col >= screen.cols {
        return col;
    }
    let idx = screen.clusters.len() as u32;
    screen.clusters.push(cluster.to_string());
    screen.row_mut(row)[col as usize] = Cell {
        content: CLUSTER_BASE + idx,
        fg: style.fg,
        bg: style.bg,
        ul: style.ul,
        attrs: style.attrs,
        width: 1,
        _pad: 0,
    };
    screen.touch_row(row);
    col + 1
}

fn is_wide_test_char(ch: char) -> bool {
    let cp = ch as u32;
    cp >= 0x1F300
}

/// Build the primary single-terminal test pattern exercising colours, fills,
/// every underline style, coloured underline, strikethrough, overline, dim,
/// wide emoji, cluster emoji, regular/bold/italic/bold-italic faces,
/// accented combining clusters, seamless box-drawing + blocks + powerline,
/// and a cursor.
pub fn build_primary_screen() -> Screen {
    let mut s = Screen::new(64, 24);
    s.default_fg = rgb(0xd8, 0xde, 0xe9);
    s.default_bg = rgb(0x1a, 0x1b, 0x26);
    s.title = "wall-test-pattern".to_string();

    let fg = s.default_fg;
    let cyan = rgb(0x7d, 0xcf, 0xff);
    let green = rgb(0x9e, 0xce, 0x6a);
    let yellow = rgb(0xe0, 0xaf, 0x68);
    let red = rgb(0xf7, 0x76, 0x8e);
    let magenta = rgb(0xbb, 0x9a, 0xf7);
    let orange = rgb(0xff, 0x9e, 0x64);
    let dark = rgb(0x12, 0x14, 0x1e);
    let blue = rgb(0x3d, 0x59, 0xa1);

    // Row 0: title banner
    write_run(&mut s, 0, 1, "glyph3d wall cell renderer ", RunStyle::fg(cyan).with_attrs(attrs::BOLD));
    write_run(&mut s, 0, 28, "--wall-test-pattern", RunStyle::fg(green));

    // Row 2: palette text colours
    let mut c = 1;
    c = write_run(&mut s, 2, c, "Palette: ", RunStyle::fg(fg));
    c = write_run(&mut s, 2, c, "red ", RunStyle::fg(red));
    c = write_run(&mut s, 2, c, "green ", RunStyle::fg(green));
    c = write_run(&mut s, 2, c, "yellow ", RunStyle::fg(yellow));
    c = write_run(&mut s, 2, c, "cyan ", RunStyle::fg(cyan));
    c = write_run(&mut s, 2, c, "magenta ", RunStyle::fg(magenta));
    write_run(&mut s, 2, c, "orange", RunStyle::fg(orange));

    // Row 4: background fills (including dark #1a1b26 vs bright fills)
    c = 1;
    c = write_run(&mut s, 4, c, "Fills: ", RunStyle::fg(fg));
    c = write_run(&mut s, 4, c, " ADDED ", RunStyle::fg(dark).with_bg(green));
    c = write_run(&mut s, 4, c, " ", RunStyle::fg(fg));
    c = write_run(&mut s, 4, c, " REMOVED ", RunStyle::fg(rgb(0xff, 0xff, 0xff)).with_bg(rgb(0xb8, 0x2d, 0x45)));
    c = write_run(&mut s, 4, c, " ", RunStyle::fg(fg));
    c = write_run(&mut s, 4, c, " INFO ", RunStyle::fg(dark).with_bg(cyan));
    c = write_run(&mut s, 4, c, " ", RunStyle::fg(fg));
    write_run(&mut s, 4, c, " #24283b ", RunStyle::fg(fg).with_bg(rgb(0x24, 0x28, 0x3b)));

    // Row 6: underline styles (single, double, curly)
    c = 1;
    c = write_run(&mut s, 6, c, "Underlines: ", RunStyle::fg(fg));
    c = write_run(
        &mut s,
        6,
        c,
        "single line",
        RunStyle::fg(fg).with_attrs(attrs::UNDERLINE_SINGLE << attrs::UNDERLINE_SHIFT),
    );
    c = write_run(&mut s, 6, c, "  ", RunStyle::fg(fg));
    c = write_run(
        &mut s,
        6,
        c,
        "double line",
        RunStyle::fg(yellow).with_attrs(attrs::UNDERLINE_DOUBLE << attrs::UNDERLINE_SHIFT),
    );
    c = write_run(&mut s, 6, c, "  ", RunStyle::fg(fg));
    write_run(
        &mut s,
        6,
        c,
        "curly spell",
        RunStyle::fg(fg)
            .with_ul(red)
            .with_attrs((attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR),
    );

    // Row 8: dotted, dashed, coloured underline
    c = 1;
    c = write_run(&mut s, 8, c, "More UL:    ", RunStyle::fg(fg));
    c = write_run(
        &mut s,
        8,
        c,
        "dotted run",
        RunStyle::fg(cyan).with_attrs(attrs::UNDERLINE_DOTTED << attrs::UNDERLINE_SHIFT),
    );
    c = write_run(&mut s, 8, c, "   ", RunStyle::fg(fg));
    c = write_run(
        &mut s,
        8,
        c,
        "dashed run",
        RunStyle::fg(magenta).with_attrs(attrs::UNDERLINE_DASHED << attrs::UNDERLINE_SHIFT),
    );
    c = write_run(&mut s, 8, c, "   ", RunStyle::fg(fg));
    write_run(
        &mut s,
        8,
        c,
        "sgr58 cyan",
        RunStyle::fg(fg)
            .with_ul(cyan)
            .with_attrs((attrs::UNDERLINE_SINGLE << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR),
    );

    // Row 10: strikethrough, overline, dim
    c = 1;
    c = write_run(&mut s, 10, c, "Styles: ", RunStyle::fg(fg));
    c = write_run(&mut s, 10, c, "strikethrough", RunStyle::fg(red).with_attrs(attrs::STRIKE));
    c = write_run(&mut s, 10, c, "  ", RunStyle::fg(fg));
    c = write_run(&mut s, 10, c, "overline run", RunStyle::fg(green).with_attrs(attrs::OVERLINE));
    c = write_run(&mut s, 10, c, "  ", RunStyle::fg(fg));
    write_run(&mut s, 10, c, "dim faint text", RunStyle::fg(fg).with_attrs(attrs::DIM));

    // Row 12: varied sparse emoji (single, adjacent full-bleed squares, flag, skin tone,
    // keycap, VS16 vs VS15 heart, ZWJ family + curly underline, adjacent cell fill)
    c = 1;
    c = write_run(&mut s, 12, c, "Emoji: ", RunStyle::fg(fg));
    c = write_run(&mut s, 12, c, "🚀", RunStyle::fg(fg).with_bg(rgb(0x28, 0x34, 0x57)));
    c = write_run(&mut s, 12, c, "X", RunStyle::fg(dark).with_bg(green));
    c = write_run(&mut s, 12, c, " 🐀🔥🎉🦀🌈🍕🟥🟦 ", RunStyle::fg(fg));
    c = write_cluster(&mut s, 12, c, "\u{1F1FA}\u{1F1F8}", RunStyle::fg(fg));
    c = write_cluster(&mut s, 12, c, "\u{1F44D}\u{1F3FD}", RunStyle::fg(fg));
    c = write_cluster(&mut s, 12, c, "1\u{FE0F}\u{20E3}", RunStyle::fg(fg));
    c = write_cluster(&mut s, 12, c, "\u{2764}\u{FE0F}", RunStyle::fg(fg));
    c = write_narrow_cluster(&mut s, 12, c, "\u{2764}\u{FE0E}", RunStyle::fg(red));
    c = write_run(&mut s, 12, c, " ", RunStyle::fg(fg));
    c = write_cluster(
        &mut s,
        12,
        c,
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        RunStyle::fg(fg)
            .with_bg(rgb(0x1f, 0x3b, 0x32))
            .with_ul(yellow)
            .with_attrs((attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR),
    );
    write_run(&mut s, 12, c, " zwj", RunStyle::fg(fg));

    // Row 14: regular, bold, italic, and bold-italic side by side + accented clusters
    c = 1;
    c = write_run(&mut s, 14, c, "Faces: ", RunStyle::fg(fg));
    c = write_run(&mut s, 14, c, "Regular ", RunStyle::fg(fg));
    c = write_run(&mut s, 14, c, "Bold ", RunStyle::fg(cyan).with_attrs(attrs::BOLD));
    c = write_run(&mut s, 14, c, "Italic ", RunStyle::fg(yellow).with_attrs(attrs::ITALIC));
    c = write_run(
        &mut s,
        14,
        c,
        "BoldItalic ",
        RunStyle::fg(magenta).with_attrs(attrs::BOLD | attrs::ITALIC),
    );
    c = write_run(&mut s, 14, c, "caf", RunStyle::fg(green));
    c = write_narrow_cluster(&mut s, 14, c, "e\u{0301}", RunStyle::fg(green));
    c = write_run(&mut s, 14, c, " ", RunStyle::fg(fg));
    c = write_narrow_cluster(&mut s, 14, c, "x\u{0301}", RunStyle::fg(orange).with_attrs(attrs::BOLD));
    c = write_run(&mut s, 14, c, " ", RunStyle::fg(fg));
    c = write_narrow_cluster(&mut s, 14, c, "q\u{0308}\u{0304}", RunStyle::fg(cyan));
    c = write_run(&mut s, 14, c, " ", RunStyle::fg(fg));
    write_narrow_cluster(&mut s, 14, c, "n\u{0303}", RunStyle::fg(red).with_attrs(attrs::ITALIC));

    // Rows 16..18: seamless box drawing (light/double/heavy), blocks, shades, and powerline glyphs
    c = 1;
    c = write_run(&mut s, 16, c, "╭───┬───╮ ╔═══╗ ┏━━━┓ ", RunStyle::fg(cyan));
    c = write_run(&mut s, 16, c, "████", RunStyle::fg(green));
    c = write_run(&mut s, 16, c, "▓▒░ ", RunStyle::fg(yellow));
    c = write_run(&mut s, 16, c, " main ", RunStyle::fg(dark).with_bg(cyan).with_attrs(attrs::BOLD));
    c = write_run(&mut s, 16, c, "\u{E0B0}", RunStyle::fg(cyan).with_bg(blue));
    c = write_run(&mut s, 16, c, " wall ", RunStyle::fg(fg).with_bg(blue));
    c = write_run(&mut s, 16, c, "\u{E0B0}", RunStyle::fg(blue).with_bg(green));
    c = write_run(&mut s, 16, c, " ok ", RunStyle::fg(dark).with_bg(green).with_attrs(attrs::BOLD));
    write_run(&mut s, 16, c, "\u{E0B0}", RunStyle::fg(green));

    c = 1;
    c = write_run(&mut s, 17, c, "│Box│Lgt│ ║Dbl║ ┃Hvy┃ ", RunStyle::fg(cyan));
    c = write_run(&mut s, 17, c, "▀▄▌▐", RunStyle::fg(magenta));
    c = write_run(&mut s, 17, c, "▖▗▘▝ ", RunStyle::fg(orange));
    c = write_run(&mut s, 17, c, "▁▂▃▄▅▆▇█ ", RunStyle::fg(green));
    c = write_run(&mut s, 17, c, "\u{E0B2}", RunStyle::fg(magenta));
    c = write_run(&mut s, 17, c, " neon ", RunStyle::fg(dark).with_bg(magenta));
    c = write_run(&mut s, 17, c, "\u{E0B3}", RunStyle::fg(dark).with_bg(magenta));
    c = write_run(&mut s, 17, c, " utf8 ", RunStyle::fg(dark).with_bg(magenta));
    write_run(&mut s, 17, c, "\u{E0B0}", RunStyle::fg(magenta));

    c = 1;
    c = write_run(&mut s, 18, c, "╰───┴───╯ ╚═══╝ ┗━━━┛ ", RunStyle::fg(cyan));
    c = write_run(&mut s, 18, c, "████", RunStyle::fg(green));
    c = write_run(&mut s, 18, c, "░▒▓ ", RunStyle::fg(yellow));
    write_run(&mut s, 18, c, "zero-seam sprites \u{E0B1} powerline", RunStyle::fg(fg));

    // Row 20: prompt + cursor on 'w'
    c = 1;
    c = write_run(&mut s, 20, c, "user@wall", RunStyle::fg(green));
    c = write_run(&mut s, 20, c, ":", RunStyle::fg(fg));
    c = write_run(&mut s, 20, c, "~/glyph3d-rs", RunStyle::fg(cyan));
    c = write_run(&mut s, 20, c, "$ ", RunStyle::fg(yellow));
    let cursor_col = c;
    write_run(&mut s, 20, c, "well hello there!", RunStyle::fg(fg));

    // Row 22: contiguous fill gradient bar (verifies snapped zero-seam cell edges)
    for col in 1..63u16 {
        let t = (col - 1) as f32 / 61.0;
        let r = (30.0 + 180.0 * t) as u8;
        let g = (70.0 + 120.0 * (1.0 - (t - 0.5).abs() * 2.0)) as u8;
        let b = (210.0 - 150.0 * t) as u8;
        s.row_mut(22)[col as usize] = Cell {
            content: ' ' as u32,
            fg,
            bg: rgb(r, g, b),
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };
    }
    s.touch_row(22);

    s.cursor = Cursor {
        col: cursor_col,
        row: 20,
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };

    s
}

/// One placed terminal in the three-terminal chrome test pattern.
pub struct TestPatternTerminal {
    pub id: u64,
    pub screen: Screen,
    pub title: String,
    pub position: Vec2,
    pub z: f32,
    pub scale: f32,
    pub focused: bool,
    pub border_color: Option<u32>,
    pub selection: Option<CellSelection>,
}

/// Terminal layout metadata needed to synthesize the test pattern's chrome
/// (shared between offscreen screenshots and the live `--wall --wall-test-pattern` window).
pub struct ChromeTerminalInfo<'a> {
    pub id: u64,
    pub title: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub position: Vec2,
    pub z: f32,
    pub scale: f32,
    pub focused: bool,
}

/// Owned chrome layers for one frame of the D3 wall test pattern.
pub struct TestPatternChrome {
    pub paper: PaperStyle,
    pub shapes: Vec<Shape>,
    pub labels: Vec<Label<'static>>,
    /// World bounding box enclosing all chrome marks (for camera framing).
    pub chrome_bounds: WorldRect,
}

/// Title bar on-screen height in physical pixels (matches `TitleBar.js`).
const TITLE_BAR_PX: f32 = 22.0;
/// Cap on a title bar's world height when zoomed far out.
const TITLE_BAR_MAX_WORLD: f32 = 2.0;
/// Title text height as a fraction of its bar height.
const TITLE_TEXT_FRACTION: f32 = 0.62;
/// Left/right text inset as a fraction of bar height.
const TITLE_TEXT_INSET: f32 = 0.45;

/// Build the three-terminal overlapping wall test pattern (D1 + D3):
/// - Terminal 1 (`id = 1`, `z = 1.0`, `scale = 1.0`, unfocused, selected): inside
///   group `build-fleet`, slate border, hollow cursor, overlapped lower-right by Terminal 3.
/// - Terminal 2 (`id = 2`, `z = 1.5`, `scale = 0.5`, half scale): inside group
///   `build-fleet`, amber border override, bar cursor, `dim+strike+curly`, carrying a
///   detached map label with its leader + anchor dot.
/// - Terminal 3 (`id = 3`, `z = 3.0`, `scale = 1.0`, focused): front full showcase
///   covering both the group frame and Terminal 1 behind it.
pub fn build_overlapping_test_pattern(metrics: CellMetrics) -> Vec<TestPatternTerminal> {
    let cw = metrics.cell_width;
    let ch = metrics.cell_height;

    // Terminal 1: back-left, inside group, unfocused (hollow cursor + slate hairline border).
    let mut back = Screen::new(46, 13);
    back.default_fg = rgb(0xc8, 0xd3, 0xf5);
    back.default_bg = rgb(0x13, 0x17, 0x24);
    back.title = "agent-1 — build-worker".to_string();
    let fg1 = back.default_fg;
    let blue = rgb(0x82, 0xaa, 0xff);
    let teal = rgb(0x4f, 0xd6, 0xbe);
    let coral = rgb(0xff, 0x75, 0x7f);
    write_run(&mut back, 0, 1, "Terminal 1 (z = 1.0, selected, in group)", RunStyle::fg(blue));
    write_run(&mut back, 2, 1, "Hollow cursor -> ", RunStyle::fg(fg1));
    write_run(&mut back, 2, 18, "unfocused cell", RunStyle::fg(teal));
    write_run(&mut back, 4, 1, "Overlapped lower-right by z=3 primary:", RunStyle::fg(fg1).with_attrs(attrs::DIM));
    for r in 6..12u16 {
        let bg = if r % 2 == 0 { rgb(0x22, 0x2c, 0x45) } else { rgb(0x2c, 0x1f, 0x38) };
        write_run(
            &mut back,
            r,
            1,
            "BACKGROUND-TEXT-SHOULD-BE-OCCLUDED-BY-NEARER",
            RunStyle::fg(coral).with_bg(bg),
        );
    }
    back.cursor = Cursor {
        col: 18,
        row: 2,
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };

    // Terminal 2: upper-right inside group, HALF SCALE (0.5x), Bar cursor & amber border.
    let mut mid = Screen::new(44, 13);
    mid.default_fg = rgb(0xe2, 0xe8, 0xf0);
    mid.default_bg = rgb(0x18, 0x1e, 0x29);
    mid.title = "worker-02 (0.5x scale)".to_string();
    let fg2 = mid.default_fg;
    let amber = rgb(0xf0, 0xb4, 0x5a);
    let mint = rgb(0x6e, 0xe7, 0xa0);
    write_run(&mut mid, 0, 1, "Terminal 2 (scale = 0.5, in group)", RunStyle::fg(amber));
    write_run(&mut mid, 2, 1, "Half-scale cells, fills, lines & cursor", RunStyle::fg(fg2));
    write_run(&mut mid, 4, 1, "Cursor: ", RunStyle::fg(fg2));
    write_run(&mut mid, 4, 9, "half-scale hollow", RunStyle::fg(mint));
    write_run(&mut mid, 6, 1, "Mixed: ", RunStyle::fg(fg2));
    write_run(
        &mut mid,
        6,
        8,
        "dim+strike+curly",
        RunStyle::fg(rgb(0xf7, 0x76, 0x8e))
            .with_ul(amber)
            .with_attrs(
                attrs::DIM
                    | attrs::STRIKE
                    | (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT)
                    | attrs::UNDERLINE_COLOR,
            ),
    );
    for r in 8..12u16 {
        write_run(
            &mut mid,
            r,
            1,
            "HALF-SCALE-TERMINAL-WITH-MAP-LABEL-LEADER",
            RunStyle::fg(amber).with_bg(rgb(0x28, 0x24, 0x1c)),
        );
    }
    mid.cursor = Cursor {
        col: 9,
        row: 4,
        visible: true,
        shape: CursorShape::Bar,
        blink: false,
    };

    // Terminal 3: front, focused, full primary showcase.
    let mut front = build_primary_screen();
    front.title = "wall-test-pattern — primary".to_string();

    // Return in scrambled Z order [1.5, 3.0, 1.0] to exercise Z sorting + depth.
    vec![
        TestPatternTerminal {
            id: 2,
            title: mid.title.clone(),
            screen: mid,
            position: Vec2::new(50.0 * cw, 13.5 * ch),
            z: 1.5,
            scale: 0.5,
            focused: false,
            border_color: Some(rgb(0xf0, 0xb4, 0x5a)),
            selection: None,
        },
        TestPatternTerminal {
            id: 3,
            title: front.title.clone(),
            screen: front,
            position: Vec2::new(14.0 * cw, 2.5 * ch),
            z: 3.0,
            scale: 1.0,
            focused: true,
            border_color: None,
            // Linear selection across "green yellow cyan" on row 2.
            selection: Some(CellSelection::linear(2, 14, 2, 31)),
        },
        TestPatternTerminal {
            id: 1,
            title: back.title.clone(),
            screen: back,
            position: Vec2::new(0.0, 14.0 * ch),
            z: 1.0,
            scale: 1.0,
            focused: false,
            border_color: None,
            // Rectangular column-block selection over existing ANSI fills.
            selection: Some(CellSelection::block(6, 10, 8, 22)),
        },
    ]
}

struct TitleBarSpec<'a> {
    box_rect: WorldRect,
    bar_h: f32,
    z: f32,
    title: &'a str,
    fill: [f32; 4],
    stroke: Option<[f32; 4]>,
    text_color: [f32; 4],
}

fn push_title_bar(
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
    metrics: CellMetrics,
    atlas: &mut FontAtlas,
    family: FamilyId,
    spec: TitleBarSpec<'_>,
) -> WorldRect {
    let strip = WorldRect::new(
        Vec2::new(spec.box_rect.min.x, spec.box_rect.max.y),
        Vec2::new(spec.box_rect.max.x, spec.box_rect.max.y + spec.bar_h),
    );
    shapes.push(Shape::Rect(RectShape {
        rect: strip,
        z: spec.z,
        fill: Some(spec.fill),
        stroke: spec.stroke,
        stroke_px: if spec.stroke.is_some() { 1.0 } else { 0.0 },
        dash: DashStyle::SOLID,
        corner_radius_px: 0.0,
    }));

    let inset = spec.bar_h * TITLE_TEXT_INSET;
    let room = (strip.width() - 2.0 * inset).max(0.0);
    let natural_h = spec.bar_h * TITLE_TEXT_FRACTION;
    let full_w = measure_label_width(metrics, atlas, family, spec.title, natural_h);
    let text_h = if full_w > room && full_w > 1e-4 {
        natural_h * (room / full_w)
    } else {
        natural_h
    };
    if text_h > 1e-4 && room > 1e-4 {
        let top_y = strip.center().y + 0.5 * text_h;
        labels.push(
            Label::new(
                spec.title.to_string(),
                Vec2::new(strip.min.x + inset, top_y),
                LabelHeight::World(text_h),
                spec.text_color,
                spec.z,
            )
            .with_clip(strip),
        );
    }
    strip
}

/// Build the full D3 canvas chrome around the 3 test-pattern terminals:
/// drafting paper, title bars on all 3 terminals, a group frame + header round
/// terminals 1 & 2, a selection outline round terminal 1, a closed dashed lasso,
/// a dashed red refused drop outline, and a screen-pixel map label with leader
/// pointing to the half-scale terminal 2.
pub fn build_test_pattern_chrome(
    metrics: CellMetrics,
    atlas: &mut FontAtlas,
    family: FamilyId,
    panel_padding: f32,
    zoom: f32,
    terminals: &[ChromeTerminalInfo<'_>],
) -> TestPatternChrome {
    let paper = PaperStyle::default();
    let safe_zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 20.0 };
    let bar_h = (TITLE_BAR_PX / safe_zoom).min(TITLE_BAR_MAX_WORLD);

    let mut shapes = Vec::new();
    let mut labels = Vec::new();
    let mut chrome_bounds: Option<WorldRect> = None;
    let mut include_rect = |r: WorldRect| {
        chrome_bounds = Some(match chrome_bounds {
            Some(acc) => acc.union(r),
            None => r,
        });
    };

    let mut group_members_bounds: Option<WorldRect> = None;
    let mut group_focused = false;
    let mut term1_full: Option<WorldRect> = None;
    let mut term2_panel: Option<WorldRect> = None;
    let mut term3_panel: Option<WorldRect> = None;

    for t in terminals {
        let panel = metrics.terminal_panel_rect(t.position, t.cols, t.rows, panel_padding, t.scale);
        let fill = if t.focused {
            srgb_hex(0x16402c, 1.0)
        } else {
            srgb_hex(0x1f2633, 1.0)
        };
        let text_col = if t.focused {
            srgb_hex(0x8ef0b8, 1.0)
        } else {
            srgb_hex(0xc4ccd8, 1.0)
        };
        let strip = push_title_bar(
            &mut shapes,
            &mut labels,
            metrics,
            atlas,
            family,
            TitleBarSpec {
                box_rect: panel,
                bar_h,
                z: t.z,
                title: t.title,
                fill,
                stroke: None,
                text_color: text_col,
            },
        );
        let full = panel.union(strip);
        include_rect(full);

        if t.id == 1 || t.id == 2 {
            group_focused |= t.focused;
            group_members_bounds = Some(match group_members_bounds {
                Some(acc) => acc.union(full),
                None => full,
            });
        }
        match t.id {
            1 => term1_full = Some(full),
            2 => term2_panel = Some(panel),
            3 => term3_panel = Some(panel),
            _ => {}
        }
    }

    // Group frame + header around terminals 1 & 2 (z = 0.0 backdrop, z = 0.2 header).
    if let Some(members) = group_members_bounds {
        let backdrop = members.expand_xy(1.5, 1.2);
        shapes.push(Shape::Rect(RectShape {
            rect: backdrop,
            z: 0.0,
            fill: Some(srgb_hex(0x0d1017, 1.0)),
            stroke: Some(srgb_hex(0x262d3a, 1.0)),
            stroke_px: 1.0,
            dash: DashStyle::SOLID,
            corner_radius_px: 0.0,
        }));
        let header_fill = if group_focused {
            srgb_hex(0x12261d, 1.0)
        } else {
            srgb_hex(0x121722, 1.0)
        };
        let header_text = if group_focused {
            srgb_hex(0x8ef0b8, 1.0)
        } else {
            srgb_hex(0x8b95a7, 1.0)
        };
        let header = push_title_bar(
            &mut shapes,
            &mut labels,
            metrics,
            atlas,
            family,
            TitleBarSpec {
                box_rect: backdrop,
                bar_h,
                z: 0.2,
                title: "group: build-fleet (2)",
                fill: header_fill,
                stroke: Some(srgb_hex(0x262d3a, 1.0)),
                text_color: header_text,
            },
        );
        include_rect(backdrop.union(header));
    }

    // Selection outline round terminal 1 (SelectionOverlay.js style, 3px outside its frame).
    if let Some(t1) = term1_full {
        let gap = 3.0 / safe_zoom;
        let sel_rect = t1.expand(gap);
        shapes.push(Shape::Rect(RectShape {
            rect: sel_rect,
            z: 10.0,
            fill: None,
            stroke: Some(srgb_hex(0x8ab4f8, 0.9)),
            stroke_px: 1.5,
            dash: DashStyle::SOLID,
            corner_radius_px: 3.0,
        }));
        include_rect(sel_rect);
    }

    // Map label with leader pointing to the half-scale terminal 2 (MapLabels.js style).
    if let Some(t2) = term2_panel {
        let text = "worker-02 (0.5x)";
        let label_px = 12.0;
        let pill_h_px = 20.0;
        let pad_x_px = 7.0;
        let text_w_world = measure_label_width(metrics, atlas, family, text, label_px / safe_zoom);
        let pill_w = text_w_world + 2.0 * (pad_x_px / safe_zoom);
        let pill_h = pill_h_px / safe_zoom;
        let pill_min = Vec2::new(t2.max.x + 3.2, t2.max.y - pill_h * 0.4);
        let pill = WorldRect::new(pill_min, pill_min + Vec2::new(pill_w, pill_h));

        let leader_from = Vec2::new(pill.min.x, pill.center().y);
        let leader_to = Vec2::new(t2.max.x, (t2.min.y + t2.max.y) * 0.55);
        shapes.push(Shape::Leader(LeaderShape {
            from: leader_from,
            to: leader_to,
            z: 11.5,
            color: srgb_hex(0x8c9bb4, 0.85),
            width_px: 1.25,
            dot_radius_px: 2.5,
            dash: DashStyle::SOLID,
        }));
        shapes.push(Shape::Rect(RectShape {
            rect: pill,
            z: 12.0,
            fill: Some(srgb_hex(0x181d27, 0.94)),
            stroke: Some(srgb_hex(0x323b4c, 1.0)),
            stroke_px: 1.0,
            dash: DashStyle::SOLID,
            corner_radius_px: 4.0,
        }));
        let text_h_world = label_px / safe_zoom;
        labels.push(
            Label::new(
                text,
                Vec2::new(pill.min.x + pad_x_px / safe_zoom, pill.center().y + 0.5 * text_h_world),
                LabelHeight::ScreenPx(label_px),
                srgb_hex(0xc4ccd8, 1.0),
                12.0,
            )
            .with_clip(pill),
        );
        include_rect(pill);
    }

    // Dashed red refused drop outline (SelectionOverlay.js style, overlapping right edge of front terminal).
    if let Some(t3) = term3_panel {
        let drop_rect = WorldRect::new(
            Vec2::new(t3.max.x - 5.5, t3.max.y - 7.5),
            Vec2::new(t3.max.x + 7.5, t3.max.y + 3.0),
        );
        shapes.push(Shape::Rect(RectShape {
            rect: drop_rect,
            z: 10.0,
            fill: Some(srgb_hex(0xf08070, 0.035)),
            stroke: Some(srgb_hex(0xf08070, 0.90)),
            stroke_px: 2.0,
            dash: DashStyle::new(8.0, 5.0),
            corner_radius_px: 3.0,
        }));
        include_rect(drop_rect);

        // Closed dashed lasso polygon sweeping across empty paper and lower-left corners.
        let lasso_pts = vec![
            Vec2::new(-3.2, -1.5),
            Vec2::new(-0.5, 3.2),
            Vec2::new(5.8, 3.9),
            Vec2::new(9.6, 0.8),
            Vec2::new(8.4, -5.6),
            Vec2::new(2.2, -8.2),
            Vec2::new(-2.8, -5.8),
        ];
        for &pt in &lasso_pts {
            include_rect(WorldRect::new(pt, pt));
        }
        shapes.push(Shape::Polygon(PolygonShape {
            points: lasso_pts,
            z: 10.0,
            fill: Some(srgb_hex(0x8ab4f8, 0.025)),
            stroke: srgb_hex(0x8ab4f8, 0.90),
            stroke_px: 1.5,
            dash: DashStyle::new(6.0, 4.0),
        }));
    }

    TestPatternChrome {
        paper,
        shapes,
        labels,
        chrome_bounds: chrome_bounds.unwrap_or(WorldRect::new(Vec2::ZERO, Vec2::ONE)),
    }
}
