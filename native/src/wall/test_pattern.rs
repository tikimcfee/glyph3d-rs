//! Deterministic test-pattern `Screen` builders for visual verification
//! (`--wall-test-pattern --screenshot OUT.png`).

use term_cells::{CLUSTER_BASE, Cell, Cursor, CursorShape, Screen, attrs, rgb};

/// Write an ASCII/UTF-8 string into `screen` starting at `(col, row)` with
/// the given foreground, background (`0` = panel shows through), underline
/// colour, and attribute flags.
pub fn write_run(
    screen: &mut Screen,
    row: u16,
    mut col: u16,
    text: &str,
    fg: u32,
    bg: u32,
    ul: u32,
    cell_attrs: u16,
) -> u16 {
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
            fg,
            bg,
            ul,
            attrs: cell_attrs,
            width,
            _pad: 0,
        };
        if wide {
            screen.row_mut(row)[(col + 1) as usize] = Cell {
                content: 0,
                fg: 0,
                bg,
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
pub fn write_cluster(
    screen: &mut Screen,
    row: u16,
    col: u16,
    cluster: &str,
    fg: u32,
    bg: u32,
    ul: u32,
    cell_attrs: u16,
) -> u16 {
    if col + 1 >= screen.cols {
        return col;
    }
    let idx = screen.clusters.len() as u32;
    screen.clusters.push(cluster.to_string());
    screen.row_mut(row)[col as usize] = Cell {
        content: CLUSTER_BASE + idx,
        fg,
        bg,
        ul,
        attrs: cell_attrs,
        width: 2,
        _pad: 0,
    };
    screen.row_mut(row)[(col + 1) as usize] = Cell {
        content: 0,
        fg: 0,
        bg,
        ul: 0,
        attrs: 0,
        width: 0,
        _pad: 0,
    };
    screen.touch_row(row);
    col + 2
}

fn is_wide_test_char(ch: char) -> bool {
    let cp = ch as u32;
    cp >= 0x1F300
}

/// Build the primary single-terminal test pattern exercising colours, fills,
/// every underline style, coloured underline, strikethrough, overline, dim,
/// wide emoji, cluster emoji, and a cursor.
pub fn build_primary_screen() -> Screen {
    let mut s = Screen::new(62, 18);
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

    // Row 0: title banner
    write_run(&mut s, 0, 1, "glyph3d wall cell renderer ", cyan, 0, 0, attrs::BOLD);
    write_run(&mut s, 0, 28, "--wall-test-pattern", green, 0, 0, 0);

    // Row 2: palette text colours
    let mut c = 1;
    c = write_run(&mut s, 2, c, "Palette: ", fg, 0, 0, 0);
    c = write_run(&mut s, 2, c, "red ", red, 0, 0, 0);
    c = write_run(&mut s, 2, c, "green ", green, 0, 0, 0);
    c = write_run(&mut s, 2, c, "yellow ", yellow, 0, 0, 0);
    c = write_run(&mut s, 2, c, "cyan ", cyan, 0, 0, 0);
    c = write_run(&mut s, 2, c, "magenta ", magenta, 0, 0, 0);
    write_run(&mut s, 2, c, "orange", orange, 0, 0, 0);

    // Row 4: background fills (including dark #1a1b26 vs bright fills)
    c = 1;
    c = write_run(&mut s, 4, c, "Fills: ", fg, 0, 0, 0);
    c = write_run(&mut s, 4, c, " ADDED ", dark, green, 0, 0);
    c = write_run(&mut s, 4, c, " ", fg, 0, 0, 0);
    c = write_run(&mut s, 4, c, " REMOVED ", rgb(0xff, 0xff, 0xff), rgb(0xb8, 0x2d, 0x45), 0, 0);
    c = write_run(&mut s, 4, c, " ", fg, 0, 0, 0);
    c = write_run(&mut s, 4, c, " INFO ", dark, cyan, 0, 0);
    c = write_run(&mut s, 4, c, " ", fg, 0, 0, 0);
    write_run(&mut s, 4, c, " #24283b ", fg, rgb(0x24, 0x28, 0x3b), 0, 0);

    // Row 6: underline styles (single, double, curly)
    c = 1;
    c = write_run(&mut s, 6, c, "Underlines: ", fg, 0, 0, 0);
    c = write_run(
        &mut s,
        6,
        c,
        "single line",
        fg,
        0,
        0,
        attrs::UNDERLINE_SINGLE << attrs::UNDERLINE_SHIFT,
    );
    c = write_run(&mut s, 6, c, "  ", fg, 0, 0, 0);
    c = write_run(
        &mut s,
        6,
        c,
        "double line",
        yellow,
        0,
        0,
        attrs::UNDERLINE_DOUBLE << attrs::UNDERLINE_SHIFT,
    );
    c = write_run(&mut s, 6, c, "  ", fg, 0, 0, 0);
    write_run(
        &mut s,
        6,
        c,
        "curly spell",
        fg,
        0,
        red,
        (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR,
    );

    // Row 8: dotted, dashed, coloured underline
    c = 1;
    c = write_run(&mut s, 8, c, "More UL:    ", fg, 0, 0, 0);
    c = write_run(
        &mut s,
        8,
        c,
        "dotted run",
        cyan,
        0,
        0,
        attrs::UNDERLINE_DOTTED << attrs::UNDERLINE_SHIFT,
    );
    c = write_run(&mut s, 8, c, "   ", fg, 0, 0, 0);
    c = write_run(
        &mut s,
        8,
        c,
        "dashed run",
        magenta,
        0,
        0,
        attrs::UNDERLINE_DASHED << attrs::UNDERLINE_SHIFT,
    );
    c = write_run(&mut s, 8, c, "   ", fg, 0, 0, 0);
    write_run(
        &mut s,
        8,
        c,
        "sgr58 cyan",
        fg,
        0,
        cyan,
        (attrs::UNDERLINE_SINGLE << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR,
    );

    // Row 10: strikethrough, overline, dim
    c = 1;
    c = write_run(&mut s, 10, c, "Styles: ", fg, 0, 0, 0);
    c = write_run(&mut s, 10, c, "strikethrough", red, 0, 0, attrs::STRIKE);
    c = write_run(&mut s, 10, c, "  ", fg, 0, 0, 0);
    c = write_run(&mut s, 10, c, "overline run", green, 0, 0, attrs::OVERLINE);
    c = write_run(&mut s, 10, c, "  ", fg, 0, 0, 0);
    write_run(&mut s, 10, c, "dim faint text", fg, 0, 0, attrs::DIM);

    // Row 12: wide emoji + cluster emoji + adjacent cell fill (proves fill never covers ink)
    c = 1;
    c = write_run(&mut s, 12, c, "Emoji: ", fg, 0, 0, 0);
    c = write_run(&mut s, 12, c, "🚀", fg, rgb(0x28, 0x34, 0x57), 0, 0);
    c = write_run(&mut s, 12, c, "X", dark, green, 0, 0);
    c = write_run(&mut s, 12, c, " rocket  ", fg, 0, 0, 0);
    c = write_run(&mut s, 12, c, "🐀", fg, rgb(0x3b, 0x2a, 0x45), 0, 0);
    c = write_run(&mut s, 12, c, " rat  ", fg, 0, 0, 0);
    c = write_cluster(
        &mut s,
        12,
        c,
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        fg,
        rgb(0x1f, 0x3b, 0x32),
        yellow,
        (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR,
    );
    write_run(&mut s, 12, c, " zwj+curly", fg, 0, 0, 0);

    // Row 14: prompt + cursor on 'w'
    c = 1;
    c = write_run(&mut s, 14, c, "lugo@wall", green, 0, 0, 0);
    c = write_run(&mut s, 14, c, ":", fg, 0, 0, 0);
    c = write_run(&mut s, 14, c, "~/glyph3d-rs", cyan, 0, 0, 0);
    c = write_run(&mut s, 14, c, "$ ", yellow, 0, 0, 0);
    let cursor_col = c;
    write_run(&mut s, 14, c, "well hello there!", fg, 0, 0, 0);

    // Row 16: contiguous fill gradient bar (verifies snapped zero-seam cell edges)
    for col in 1..61u16 {
        let t = (col - 1) as f32 / 59.0;
        let r = (30.0 + 180.0 * t) as u8;
        let g = (70.0 + 120.0 * (1.0 - (t - 0.5).abs() * 2.0)) as u8;
        let b = (210.0 - 150.0 * t) as u8;
        s.row_mut(16)[col as usize] = Cell {
            content: ' ' as u32,
            fg,
            bg: rgb(r, g, b),
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };
    }
    s.touch_row(16);

    s.cursor = Cursor {
        col: cursor_col,
        row: 14,
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };

    s
}

/// One placed terminal in the three-terminal overlapping test pattern.
pub struct TestPatternTerminal {
    pub id: u64,
    pub screen: Screen,
    pub position: glam::Vec2,
    pub z: f32,
    pub focused: bool,
    pub border_color: Option<u32>,
}

/// Build the three-terminal overlapping wall test pattern:
/// - Terminal 1 (`z = 0.0`, back-left, unfocused): slate border, hollow cursor,
///   background content overlapped on its lower-right by Terminals 2 and 3.
/// - Terminal 2 (`z = 1.0`, middle-right, focused): amber accent border, bar
///   cursor, overlaps Terminal 1 and is overlapped on its lower-left by Terminal 3.
/// - Terminal 3 (`z = 2.0`, front, focused): full feature test pattern (palette,
///   fills, all 5 underline styles, SGR 58, strike, overline, dim, wide + ZWJ
///   emoji, block cursor, seamless gradient bar) covering both behind it.
pub fn build_overlapping_test_pattern(metrics: super::metrics::CellMetrics) -> Vec<TestPatternTerminal> {
    let cw = metrics.cell_width;
    let ch = metrics.cell_height;

    // Terminal 1: back-left, unfocused (hollow cursor + slate hairline border).
    let mut back = Screen::new(46, 13);
    back.default_fg = rgb(0xc8, 0xd3, 0xf5);
    back.default_bg = rgb(0x13, 0x17, 0x24);
    back.title = "back-unfocused".to_string();
    let fg1 = back.default_fg;
    let blue = rgb(0x82, 0xaa, 0xff);
    let teal = rgb(0x4f, 0xd6, 0xbe);
    let coral = rgb(0xff, 0x75, 0x7f);
    write_run(&mut back, 0, 1, "Terminal 1 (z = 0.0, unfocused)", blue, 0, 0, 0);
    write_run(&mut back, 2, 1, "Hollow cursor -> ", fg1, 0, 0, 0);
    write_run(&mut back, 2, 18, "unfocused cell", teal, 0, 0, 0);
    write_run(&mut back, 4, 1, "Overlapped below-right by z=1 and z=2:", fg1, 0, 0, attrs::DIM);
    for r in 6..12u16 {
        let bg = if r % 2 == 0 { rgb(0x22, 0x2c, 0x45) } else { rgb(0x2c, 0x1f, 0x38) };
        write_run(
            &mut back,
            r,
            1,
            "BACKGROUND-TEXT-SHOULD-BE-OCCLUDED-BY-NEARER",
            coral,
            bg,
            0,
            0,
        );
    }
    back.cursor = Cursor {
        col: 18,
        row: 2,
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };

    // Terminal 2: middle-right, focused with Bar cursor & amber border override.
    let mut mid = Screen::new(44, 13);
    mid.default_fg = rgb(0xe2, 0xe8, 0xf0);
    mid.default_bg = rgb(0x18, 0x1e, 0x29);
    mid.title = "middle-bar-cursor".to_string();
    let fg2 = mid.default_fg;
    let amber = rgb(0xf0, 0xb4, 0x5a);
    let mint = rgb(0x6e, 0xe7, 0xa0);
    write_run(&mut mid, 0, 1, "Terminal 2 (z = 1.0, bar cursor)", amber, 0, 0, 0);
    write_run(&mut mid, 2, 1, "Covers z=0; covered by z=2 front", fg2, 0, 0, 0);
    write_run(&mut mid, 4, 1, "Bar cursor: ", fg2, 0, 0, 0);
    write_run(&mut mid, 4, 13, "insert mode here", mint, 0, 0, 0);
    write_run(
        &mut mid,
        6,
        1,
        "Mixed: ",
        fg2,
        0,
        0,
        0,
    );
    write_run(
        &mut mid,
        6,
        8,
        "dim+strike+curly",
        rgb(0xf7, 0x76, 0x8e),
        0,
        amber,
        attrs::DIM
            | attrs::STRIKE
            | (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT)
            | attrs::UNDERLINE_COLOR,
    );
    for r in 8..12u16 {
        write_run(
            &mut mid,
            r,
            1,
            "MIDDLE-LAYER-TEXT-OCCLUDED-BY-FRONT-TERM",
            amber,
            rgb(0x28, 0x24, 0x1c),
            0,
            0,
        );
    }
    mid.cursor = Cursor {
        col: 13,
        row: 4,
        visible: true,
        shape: CursorShape::Bar,
        blink: false,
    };

    // Terminal 3: front, focused, full primary showcase.
    let front = build_primary_screen();

    // Return in scrambled Z order [1.0, 2.0, 0.0] to exercise Z sorting + depth.
    vec![
        TestPatternTerminal {
            id: 2,
            screen: mid,
            position: glam::Vec2::new(34.0 * cw, 5.0 * ch),
            z: 1.0,
            focused: true,
            border_color: Some(rgb(0xf0, 0xb4, 0x5a)),
        },
        TestPatternTerminal {
            id: 3,
            screen: front,
            position: glam::Vec2::new(10.0 * cw, -3.5 * ch),
            z: 2.0,
            focused: true,
            border_color: None,
        },
        TestPatternTerminal {
            id: 1,
            screen: back,
            position: glam::Vec2::new(0.0, 6.5 * ch),
            z: 0.0,
            focused: false,
            border_color: None,
        },
    ]
}
