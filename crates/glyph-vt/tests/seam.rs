//! Integration tests exercising `glyph_vt` strictly through its public seam
//! (`Theme`, `Vt`, and `term_cells::Screen`).

use glyph_vt::{Theme, Vt, VtEvent};
use term_cells::{
    attrs, rgb, CursorShape, Face, MouseFormat, MouseTracking, Screen, CLUSTER_BASE,
};

fn setup(cols: u16, rows: u16) -> (Vt, Screen, Theme) {
    let theme = Theme::default();
    let vt = Vt::new(cols, rows, &theme);
    let screen = Screen::new(cols, rows);
    (vt, screen, theme)
}

#[test]
fn sgr_colors_palette_cube_gray_truecolor_and_osc_overrides() {
    let (mut vt, mut screen, theme) = setup(20, 4);

    // Default fg/bg, standard 16, bright 16, 256 cube, 256 gray, 24-bit RGB (semicolon + colon).
    vt.feed(
        b"d\x1b[31;44ms\x1b[91;104mb\x1b[38;5;196;48;5;21mc\x1b[38;5;232;48;5;255mg\x1b[38;2;18;52;86;48;2;200;100;50mt\x1b[38:2::12:34:56;49mk\x1b[0m",
    );
    assert!(vt.fill(&mut screen));

    let row = screen.row(0);
    // 'd': default fg, unset bg (alpha 0).
    assert_eq!(row[0].content, 'd' as u32);
    assert_eq!(row[0].fg, theme.fg);
    assert_eq!(row[0].bg, 0);

    // 's': standard red fg (1), standard blue bg (4).
    assert_eq!(row[1].fg, theme.palette[1]);
    assert_eq!(row[1].bg, theme.palette[4]);

    // 'b': bright red fg (9), bright blue bg (12).
    assert_eq!(row[2].fg, theme.palette[9]);
    assert_eq!(row[2].bg, theme.palette[12]);

    // 'c': 256 cube 196 = (5,0,0) -> (255, 0, 0); 21 = (0,0,5) -> (0, 0, 255).
    assert_eq!(row[3].fg, rgb(255, 0, 0));
    assert_eq!(row[3].bg, rgb(0, 0, 255));

    // 'g': grayscale 232 -> (8,8,8); 255 -> (238,238,238).
    assert_eq!(row[4].fg, rgb(8, 8, 8));
    assert_eq!(row[4].bg, rgb(238, 238, 238));

    // 't': truecolour semicolon form.
    assert_eq!(row[5].fg, rgb(18, 52, 86));
    assert_eq!(row[5].bg, rgb(200, 100, 50));

    // 'k': truecolour colon ITU T.416 form with SGR 49 default bg reset.
    assert_eq!(row[6].fg, rgb(12, 34, 56));
    assert_eq!(row[6].bg, 0);

    // OSC 4/10/11/12 dynamic colour overrides and OSC 104/110/111/112 resets.
    vt.feed(
        b"\x1b]4;1;#112233\x07\x1b]10;#abcdef\x07\x1b]11;#102030\x07\x1b]12;#fedcba\x07",
    );
    assert!(vt.fill(&mut screen));
    assert_eq!(screen.default_fg, rgb(0xab, 0xcd, 0xef));
    assert_eq!(screen.default_bg, rgb(0x10, 0x20, 0x30));
    assert_eq!(vt.cursor_color(), rgb(0xfe, 0xdc, 0xba));
    assert_eq!(screen.row(0)[0].fg, rgb(0xab, 0xcd, 0xef));
    assert_eq!(screen.row(0)[1].fg, rgb(0x11, 0x22, 0x33));

    // Updating the baseline theme preserves explicit OSC overrides until reset.
    let mut alt_theme = theme.clone();
    alt_theme.palette[4] = rgb(0x33, 0x44, 0x55);
    alt_theme.fg = rgb(0xee, 0xee, 0xee);
    vt.set_theme(&alt_theme);
    assert!(vt.fill(&mut screen));
    assert_eq!(screen.default_fg, rgb(0xab, 0xcd, 0xef));
    assert_eq!(screen.row(0)[1].bg, rgb(0x33, 0x44, 0x55));

    // Resetting dynamic colours via OSC 104/110/111/112 restores the current theme baseline.
    vt.feed(b"\x1b]104;1\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07");
    assert!(vt.fill(&mut screen));
    assert_eq!(screen.default_fg, alt_theme.fg);
    assert_eq!(screen.default_bg, alt_theme.bg);
    assert_eq!(vt.cursor_color(), alt_theme.cursor);
    assert_eq!(screen.row(0)[1].fg, alt_theme.palette[1]);
}

#[test]
fn bold_is_bright_policy_matches_js_wall_rules() {
    let default_theme = Theme::default();
    assert!(!default_theme.bold_is_bright);

    // With bold_is_bright = false, SGR 1 sets Face::Bold without changing standard palette colour.
    let mut vt_off = Vt::new(8, 2, &default_theme);
    let mut screen_off = Screen::new(8, 2);
    vt_off.feed(b"\x1b[1;31ma\x1b[1;38;5;2mb\x1b[0m");
    vt_off.fill(&mut screen_off);
    assert_eq!(screen_off.row(0)[0].fg, default_theme.palette[1]);
    assert_eq!(
        Face::from_bits(attrs::face(screen_off.row(0)[0].attrs)),
        Face::Bold
    );
    assert_eq!(screen_off.row(0)[1].fg, default_theme.palette[2]);

    // With bold_is_bright = true, standard indices 0..8 brighten to 8..16 (both SGR 30..37 and 38;5;0..7).
    let bright_theme = Theme {
        bold_is_bright: true,
        ..Theme::default()
    };
    let mut vt_on = Vt::new(8, 2, &bright_theme);
    let mut screen_on = Screen::new(8, 2);
    vt_on.feed(b"\x1b[1;31ma\x1b[1;38;5;2mb\x1b[1;91mc\x1b[1;38;2;10;20;30md\x1b[0m");
    vt_on.fill(&mut screen_on);
    assert_eq!(screen_on.row(0)[0].fg, bright_theme.palette[9]);
    assert_eq!(screen_on.row(0)[1].fg, bright_theme.palette[10]);
    assert_eq!(screen_on.row(0)[2].fg, bright_theme.palette[9]);
    assert_eq!(screen_on.row(0)[3].fg, rgb(10, 20, 30));
}

#[test]
fn attributes_map_bit_for_bit_and_support_all_five_underlines_and_color() {
    let (mut vt, mut screen, theme) = setup(20, 3);

    // Row 0: each attribute individually so swapped bit positions (especially DIM vs ITALIC) fail loudly.
    vt.feed(
        b"\x1b[1mB\x1b[0;2mD\x1b[0;3mI\x1b[0;5mK\x1b[0;7mV\x1b[0;8mC\x1b[0;9mS\x1b[0;53mO\x1b[0;1;3mX\x1b[0m",
    );
    // Row 1: all 5 underline styles + reset.
    vt.feed(
        b"\r\n\x1b[4m1\x1b[21m2\x1b[4:3m3\x1b[4:4m4\x1b[4:5m5\x1b[4:0m0\x1b[0m",
    );
    // Row 2: underline colour with RGB, with palette, after SGR 59 reset, and SGR 58 WITHOUT underline.
    vt.feed(
        b"\r\n\x1b[4:3;58;2;255;128;64mu\x1b[58;5;2mp\x1b[59mr\x1b[24;58;2;1;2;3mn\x1b[0m",
    );
    vt.fill(&mut screen);

    let r0 = screen.row(0);
    assert_eq!(r0[0].attrs, attrs::BOLD);
    assert_eq!(r0[1].attrs, attrs::DIM);
    assert_eq!(r0[2].attrs, attrs::ITALIC);
    assert_eq!(r0[3].attrs, attrs::BLINK);
    assert_eq!(r0[4].attrs, attrs::INVERSE);
    assert_eq!(r0[5].attrs, attrs::CONCEAL);
    assert_eq!(r0[6].attrs, attrs::STRIKE);
    assert_eq!(r0[7].attrs, attrs::OVERLINE);
    assert_eq!(r0[8].attrs, attrs::BOLD | attrs::ITALIC);
    assert_eq!(Face::from_bits(attrs::face(r0[8].attrs)), Face::BoldItalic);

    let r1 = screen.row(1);
    assert_eq!(attrs::underline(r1[0].attrs), attrs::UNDERLINE_SINGLE);
    assert_eq!(attrs::underline(r1[1].attrs), attrs::UNDERLINE_DOUBLE);
    assert_eq!(attrs::underline(r1[2].attrs), attrs::UNDERLINE_CURLY);
    assert_eq!(attrs::underline(r1[3].attrs), attrs::UNDERLINE_DOTTED);
    assert_eq!(attrs::underline(r1[4].attrs), attrs::UNDERLINE_DASHED);
    assert_eq!(attrs::underline(r1[5].attrs), attrs::UNDERLINE_NONE);

    let r2 = screen.row(2);
    assert_eq!(attrs::underline(r2[0].attrs), attrs::UNDERLINE_CURLY);
    assert_ne!(r2[0].attrs & attrs::UNDERLINE_COLOR, 0);
    assert_eq!(r2[0].ul, rgb(255, 128, 64));

    assert_ne!(r2[1].attrs & attrs::UNDERLINE_COLOR, 0);
    assert_eq!(r2[1].ul, theme.palette[2]);

    // SGR 59 resets underline colour while keeping curly underline.
    assert_eq!(attrs::underline(r2[2].attrs), attrs::UNDERLINE_CURLY);
    assert_eq!(r2[2].attrs & attrs::UNDERLINE_COLOR, 0);
    assert_eq!(r2[2].ul, 0);

    // SGR 58 with no active underline must NOT set UNDERLINE_COLOR.
    assert_eq!(attrs::underline(r2[3].attrs), attrs::UNDERLINE_NONE);
    assert_eq!(r2[3].attrs & attrs::UNDERLINE_COLOR, 0);
    assert_eq!(r2[3].ul, 0);
}

#[test]
fn inverse_and_conceal_resolve_colors_and_blank_rendered_content() {
    let (mut vt, mut screen, theme) = setup(12, 2);

    // Default inverse, explicit-fg inverse, explicit-fg+bg inverse, and concealed with bg + underline.
    vt.feed(
        b"\x1b[7ma\x1b[31mb\x1b[44mc\x1b[0;8;4;32;41ms\x1b[0m",
    );
    vt.fill(&mut screen);

    let row = screen.row(0);
    // 'a': default fg/bg inverted -> text in theme.bg, cell filled with theme.fg.
    assert_eq!(row[0].fg, theme.bg);
    assert_eq!(row[0].bg, theme.fg);
    assert_ne!(row[0].attrs & attrs::INVERSE, 0);

    // 'b': red fg + default bg inverted -> text in theme.bg, cell filled with red.
    assert_eq!(row[1].fg, theme.bg);
    assert_eq!(row[1].bg, theme.palette[1]);

    // 'c': red fg + blue bg inverted -> text in blue, cell filled with red.
    assert_eq!(row[2].fg, theme.palette[4]);
    assert_eq!(row[2].bg, theme.palette[1]);

    // 's': concealed -> blank content on Screen (`0`), fill and underline colour/style preserved,
    // while `vt.text` still reads the underlying character for copy.
    assert_eq!(row[3].content, 0);
    assert_ne!(row[3].attrs & attrs::CONCEAL, 0);
    assert_eq!(attrs::underline(row[3].attrs), attrs::UNDERLINE_SINGLE);
    assert_eq!(row[3].fg, theme.palette[2]);
    assert_eq!(row[3].bg, theme.palette[1]);
    assert_eq!(vt.text(0, 0, 4, 0), "abcs");
}

#[test]
fn wide_characters_edge_spacers_and_combining_and_zwj_clusters() {
    let (mut vt, mut screen, theme) = setup(5, 4);

    // Row 0..1: 'abcd' then wide '漢' at col 4 -> spacer at (4,0), wraps '漢' to (0,1..2).
    vt.feed("abcd\x1b[44m漢\x1b[0m".as_bytes());
    // Row 2: combining cluster 'e' + acute + diaeresis, then 'z'.
    vt.feed("\r\ne\u{0301}\u{0308}z".as_bytes());
    // Row 3: mode 2027 ZWJ family emoji cluster with inverse fill.
    vt.feed("\r\n\x1b[?2027h\x1b[7;33m👨\u{200d}👩\u{200d}👧\x1b[0m".as_bytes());
    vt.fill(&mut screen);

    // Spacer cell at (4, 0) is blank width-1, and row 0 is marked soft-wrapped.
    assert!(screen.wrapped[0]);
    assert_eq!(screen.row(0)[4].content, 0);
    assert_eq!(screen.row(0)[4].width, 1);

    // Wide '漢' at (0, 1) has width 2; tail at (1, 1) has width 0, content 0, and identical bg.
    let head = screen.row(1)[0];
    let tail = screen.row(1)[1];
    assert_eq!(head.content, '漢' as u32);
    assert_eq!(head.width, 2);
    assert_eq!(head.bg, theme.palette[4]);
    assert_eq!(tail.content, 0);
    assert_eq!(tail.width, 0);
    assert_eq!(tail.bg, theme.palette[4]);

    // Joining across the wrapped wide spacer inserts no spurious space.
    assert_eq!(vt.text(0, 0, 5, 1), "abcd漢");
    // Starting or ending inside a wide character widens to include the whole character.
    assert_eq!(vt.text(1, 1, 2, 1), "漢");
    assert_eq!(vt.text(0, 1, 1, 1), "漢");

    // Combining cluster on row 2.
    let comb = screen.row(2)[0];
    assert!(comb.content >= CLUSTER_BASE);
    assert_eq!(comb.width, 1);
    assert_eq!(screen.content_str(&comb), "e\u{0301}\u{0308}");
    assert_eq!(screen.row(2)[1].content, 'z' as u32);
    assert_eq!(vt.text(0, 2, 5, 2), "e\u{0301}\u{0308}z");

    // ZWJ cluster on row 3 (width 2 + tail carrying inverse yellow bg).
    let zwj_head = screen.row(3)[0];
    let zwj_tail = screen.row(3)[1];
    assert!(zwj_head.content >= CLUSTER_BASE);
    assert_eq!(zwj_head.width, 2);
    assert_eq!(screen.content_str(&zwj_head), "👨\u{200d}👩\u{200d}👧");
    assert_eq!(zwj_head.bg, theme.palette[3]);
    assert_eq!(zwj_tail.width, 0);
    assert_eq!(zwj_tail.content, 0);
    assert_eq!(zwj_tail.bg, theme.palette[3]);
}

#[test]
fn cursor_shapes_modes_alt_screen_mouse_priority_and_title() {
    let (mut vt, mut screen, _theme) = setup(16, 6);

    // Initial state: visible steady block at (0, 0).
    vt.fill(&mut screen);
    assert_eq!(screen.cursor.col, 0);
    assert_eq!(screen.cursor.row, 0);
    assert!(screen.cursor.visible);
    assert_eq!(screen.cursor.shape, CursorShape::Block);
    assert!(!screen.cursor.blink);

    // Move cursor, set blinking bar (DECSCUSR 5), set title, enable mouse modes (last set wins).
    vt.feed(b"primary\x1b[3;7H\x1b[5 q\x1b]2;Wall Shell\x07\x1b[?1000h\x1b[?1003h\x1b[?2004h\x1b[?1h\x1b[?1004h");
    vt.fill(&mut screen);
    assert_eq!(screen.cursor.col, 6);
    assert_eq!(screen.cursor.row, 2);
    assert_eq!(screen.cursor.shape, CursorShape::Bar);
    assert!(screen.cursor.blink);
    assert_eq!(screen.title, "Wall Shell");
    assert_eq!(screen.modes.mouse, MouseTracking::Any);
    assert!(screen.modes.bracketed_paste);
    assert!(screen.modes.app_cursor);
    assert!(screen.modes.focus_events);
    assert!(!screen.modes.alt_screen);

    // Enabling ?1002h overrides ?1003h; disabling ?1002l returns mouse tracking to Off.
    vt.feed(b"\x1b[?1002h");
    vt.fill(&mut screen);
    assert_eq!(screen.modes.mouse, MouseTracking::Button);
    vt.feed(b"\x1b[?1002l");
    vt.fill(&mut screen);
    assert_eq!(screen.modes.mouse, MouseTracking::Off);

    // Steady underline (DECSCUSR 4) and hide cursor (?25l).
    vt.feed(b"\x1b[4 q\x1b[?25l");
    vt.fill(&mut screen);
    assert_eq!(screen.cursor.shape, CursorShape::Underline);
    assert!(!screen.cursor.blink);
    assert!(!screen.cursor.visible);

    // Enter alternate screen (?1049h): clears grid, homes cursor, sets alt_screen = true.
    vt.feed(b"\x1b[?1049h\x1b[?25halt-tui");
    vt.fill(&mut screen);
    assert!(screen.modes.alt_screen);
    assert_eq!(vt.row_text(0), "alt-tui");

    // Leave alternate screen (?1049l): restores primary grid and saved cursor position (6, 2).
    vt.feed(b"\x1b[?1049l");
    vt.fill(&mut screen);
    assert!(!screen.modes.alt_screen);
    assert_eq!(vt.row_text(0), "primary");
    assert_eq!(screen.cursor.col, 6);
    assert_eq!(screen.cursor.row, 2);
}

#[test]
fn resize_reflow_visible_only_scrollback_and_text_selection() {
    let (mut vt, mut screen, _theme) = setup(5, 4);

    // Soft-wrapped line with interior spaces at wrap boundary + hard second line.
    vt.feed(b"abc  def\r\n12345");
    vt.fill(&mut screen);
    assert!(screen.wrapped[0]);
    assert!(!screen.wrapped[1]);
    // Stream selection preserves spaces across soft wrap and trims trailing blanks at hard ends.
    assert_eq!(vt.text(0, 0, 5, 2), "abc  def\n12345");
    // Reversed endpoints produce the exact same stream text.
    assert_eq!(vt.text(5, 2, 0, 0), "abc  def\n12345");
    // Rectangular block selection takes columns [1..4) on each row independently:
    // row 0 ("abc  ") -> "bc", row 1 ("def  ") -> "ef", row 2 ("12345") -> "234".
    assert_eq!(vt.block_text(1, 0, 4, 2), "bc\nef\n234");

    // Widen to 10x4: soft-wrapped line unwraps onto row 0!
    vt.resize(10, 4);
    vt.fill(&mut screen);
    assert_eq!(screen.cols, 10);
    assert_eq!(screen.rows, 4);
    assert!(!screen.wrapped[0]);
    assert_eq!(vt.row_text(0), "abc  def");
    assert_eq!(vt.row_text(1), "12345");

    // Scroll many lines past top (including after RIS ESC c) — visible screen only, no history retained.
    vt.feed(b"\x1bc");
    for idx in 0..50 {
        vt.feed(format!("line-{idx:02}\r\n").as_bytes());
    }
    vt.resize(10, 6);
    vt.fill(&mut screen);
    // Because max_scrollback == 0, growing height pads blank rows rather than pulling scrollback.
    assert_eq!(vt.row_text(0), "line-47");
    assert_eq!(vt.row_text(1), "line-48");
    assert_eq!(vt.row_text(2), "line-49");
    assert_eq!(vt.row_text(3), "");
}

#[test]
fn soft_reset_resets_everything_a_termd_repaint_relies_on() {
    let (mut vt, mut screen, theme) = setup(20, 6);

    // Leave messy state in every subsystem before receiving a termd REPAINT:
    // alt screen, hidden blinking bar cursor, insert mode, origin mode, autowrap OFF,
    // reverse wrap, DECLRMM (?69h) + DECSLRM margins, DECSTBM scroll region, app cursor/keypad,
    // DEC Special Graphics charset, OSC 4 palette mutation, OSC 10 dynamic fg, SGR pen,
    // saved cursor, modifyOtherKeys, Kitty pushed stack, and unclosed OSC 8 hyperlink.
    vt.feed(
        b"\x1b]4;1;#010203\x07\x1b]10;#aabbcc\x07\x1b[?1049h\x1b[?25l\x1b[5 q\x1b[4h\
          \x1b[2;5r\x1b[?69h\x1b[3;10s\x1b[?6h\x1b[?7l\x1b[?45h\x1b[?1h\x1b[?66h\
          \x1b(0\x1b[1;3;4;31m\x1b7\x1b[>4;2m\x1b[>5u\x1b]8;;https://example.com\x07qqq",
    );
    vt.fill(&mut screen);
    assert!(screen.modes.alt_screen);

    // Feed g3d-termd's exact REPAINT preamble + repainted content using CSI s/u (which would
    // be hijacked by DECSLRM if ?69 had not been reset by CSI ! p!) and palette 1.
    const PREAMBLE: &[u8] = b"\x1b[?1049l\x1b[!p\x1b[?7h\
\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\
\x1b[?1004l\x1b[?2004l\x1b[=0;1u\x1b[H\x1b[2J";

    vt.feed(PREAMBLE);
    vt.feed(b"\x1b[31mqq\x1b[s\x1b[2;1Hwrap_check_1234567890123\x1b[u!");
    vt.fill(&mut screen);

    assert!(!screen.modes.alt_screen);
    assert!(screen.cursor.visible);
    assert_eq!(screen.cursor.shape, CursorShape::Block);
    assert!(!screen.cursor.blink);
    assert!(!screen.modes.app_cursor);
    assert!(!screen.modes.bracketed_paste);
    assert!(!screen.modes.focus_events);
    assert_eq!(screen.modes.mouse, MouseTracking::Off);

    // Charset reset to ASCII ('q' prints as 'q', not '─'), CSI s/u worked as SCOSC/SCORC,
    // palette[1] reset to theme baseline, while OSC 10 dynamic fg stayed as xterm/Ghostty specify.
    assert_eq!(vt.row_text(0), "qq!");
    assert_eq!(screen.row(0)[0].fg, theme.palette[1]);
    assert_eq!(screen.default_fg, rgb(0xaa, 0xbb, 0xcc));
    assert!(screen.wrapped[1]);
    assert_eq!(vt.row_text(2), "0123");
}

#[test]
fn query_replies_and_side_events_are_discarded_without_disturbing_display() {
    let (mut vt, mut screen, _theme) = setup(16, 4);

    for _ in 0..200 {
        vt.feed(
            b"\x1b[c\x1b[>c\x1b[=c\x1b[5n\x1b[6n\x1b[?6n\x1b[?4m\x1b[1$p\x1b[?25$p\
              \x1b[>q\x1b[14t\x1b[16t\x1b[18t\x1b[?u\x1b]4;1;?\x07\x1b]10;?\x07\
              \x1b]11;?\x07\x1b]12;?\x07\x1b]52;c;?\x07\x1b]52;c;SGVsbG8=\x07\
              \x1bP$qm\x1b\\\x1bP+q544e\x1b\\\x07\x1b]8;;https://example.com\x07\
              \x1b]8;;\x07\x1b]7;file://host/tmp\x07\x1b[?2048h",
        );
    }
    vt.resize(16, 5);
    vt.feed(b"ok");
    vt.fill(&mut screen);
    assert_eq!(vt.row_text(0), "ok");
}

#[test]
fn fill_tracks_changed_rows_precisely_and_unchanged_fill_is_a_noop() {
    let (mut vt, mut screen, _theme) = setup(10, 4);

    // Initial sync brings screen up to Vt's initial state.
    assert!(vt.fill(&mut screen));
    let gen_after_init = screen.generation;
    let row_gens_after_init = screen.row_gen.clone();

    // Unchanged emulator: returns false, bumps neither generation nor any row_gen.
    assert!(!vt.fill(&mut screen));
    assert_eq!(screen.generation, gen_after_init);
    assert_eq!(screen.row_gen, row_gens_after_init);

    // No-op byte sequences (\r at col 0, SGR 0 with no print) also leave Screen untouched.
    vt.feed(b"\r\x1b[0m");
    assert!(!vt.fill(&mut screen));
    assert_eq!(screen.generation, gen_after_init);
    assert_eq!(screen.row_gen, row_gens_after_init);

    // Write only on row 2: ONLY row_gen[2] advances; rows 0, 1, 3 keep their exact stamps.
    vt.feed(b"\x1b[3;2Hhello");
    assert!(vt.fill(&mut screen));
    assert!(screen.row_gen[2] > row_gens_after_init[2]);
    assert_eq!(screen.row_gen[0], row_gens_after_init[0]);
    assert_eq!(screen.row_gen[1], row_gens_after_init[1]);
    assert_eq!(screen.row_gen[3], row_gens_after_init[3]);

    // Cursor-only move: bumps Screen::generation, touches ZERO rows.
    let row_gens_before_move = screen.row_gen.clone();
    let gen_before_move = screen.generation;
    vt.feed(b"\x1b[1;5H");
    assert!(vt.fill(&mut screen));
    assert!(screen.generation > gen_before_move);
    assert_eq!(screen.row_gen, row_gens_before_move);

    // Title-only change: bumps Screen::generation, touches ZERO rows.
    let gen_before_title = screen.generation;
    vt.feed(b"\x1b]2;Quiet Title\x07");
    assert!(vt.fill(&mut screen));
    assert!(screen.generation > gen_before_title);
    assert_eq!(screen.row_gen, row_gens_before_move);
    assert_eq!(screen.title, "Quiet Title");

    // Passing a brand-new Screen to an already-clean Vt still populates it completely.
    let mut second_screen = Screen::new(10, 4);
    assert!(vt.fill(&mut second_screen));
    assert_eq!(second_screen.title, "Quiet Title");
    assert_eq!(second_screen.content_str(&second_screen.row(2)[1]), "h");
}

#[test]
fn host_events_bell_clipboard_pwd_hyperlinks_and_extended_modes() {
    let (mut vt, mut screen, _theme) = setup(30, 6);

    // 1. Bell, OSC 52 write ('c') + read ('p'), OSC 7 pwd, OSC 9 notification.
    vt.feed(
        b"\x07\x1b]52;c;d2VsbCBoZWxsbyB0aGVyZSE=\x07\x1b]52;p;?\x1b\\\
          \x1b]7;file://laptop-2/Users/lugo/work\x07\x1b]9;build done\x07",
    );
    assert!(vt.has_events());
    assert_eq!(vt.pwd(), "file://laptop-2/Users/lugo/work");
    let events = vt.take_events();
    assert_eq!(
        events,
        vec![
            VtEvent::Bell,
            VtEvent::ClipboardWrite {
                selection: b'c',
                text: "well hello there!".to_string(),
            },
            VtEvent::ClipboardRead { selection: b'p' },
            VtEvent::PwdChanged("file://laptop-2/Users/lugo/work".to_string()),
            VtEvent::Notification {
                title: String::new(),
                body: "build done".to_string(),
            },
        ]
    );
    assert!(!vt.has_events());

    // Event ring is strictly bounded (<= 256) if never drained.
    for _ in 0..600 {
        vt.feed(b"\x07");
    }
    let capped = vt.take_events();
    assert_eq!(capped.len(), 256);

    // 2. OSC 8 hyperlinks on narrow and wide cells + GC across overwrites.
    vt.feed("\x1b]8;;https://glyph3d.dev/docs\x07ab漢\x1b]8;;\x07z".as_bytes());
    vt.fill(&mut screen);
    assert_eq!(vt.hyperlink(0, 0), Some("https://glyph3d.dev/docs"));
    assert_eq!(vt.hyperlink(1, 0), Some("https://glyph3d.dev/docs"));
    // Both columns of wide '漢' (head at 2, tail at 3) resolve the hyperlink.
    assert_eq!(vt.hyperlink(2, 0), Some("https://glyph3d.dev/docs"));
    assert_eq!(vt.hyperlink(3, 0), Some("https://glyph3d.dev/docs"));
    assert_eq!(vt.hyperlink(4, 0), None);
    assert_eq!(vt.hyperlink(99, 99), None);

    // Churn > 600 distinct transient hyperlinks overwritten at (0, 1) to exercise GC.
    for idx in 0..650 {
        vt.feed(format!("\x1b[2;1H\x1b]8;;https://example.com/{idx}\x07L\x1b]8;;\x07").as_bytes());
    }
    assert_eq!(vt.hyperlink(0, 1), Some("https://example.com/649"));
    assert_eq!(vt.hyperlink(0, 0), Some("https://glyph3d.dev/docs"));

    // 3. Synchronized output (?2026), mouse format (1005/1006/1015/1016), focus (?1004),
    //    Kitty keyboard flags, and modifyOtherKeys.
    vt.feed(b"\x1b[?2026h\x1b[?1006h\x1b[?1016h\x1b[?1004h\x1b[>11u\x1b[>4;2m");
    assert!(vt.synchronized_output());
    assert_eq!(vt.mouse_format(), MouseFormat::SgrPixels);
    assert!(vt.focus_events());
    assert_eq!(vt.kitty_keyboard(), 11);
    assert_eq!(vt.modify_other_keys(), 2);

    vt.fill(&mut screen);
    assert!(screen.modes.synchronized_output);
    assert_eq!(screen.modes.mouse_format, MouseFormat::SgrPixels);
    assert!(screen.modes.focus_events);
    assert_eq!(screen.modes.kitty_keyboard, 11);

    // Disabling active mouse format (?1016l) returns to default X10; enabling ?1006h sets Sgr.
    vt.feed(b"\x1b[?1016l\x1b[?1006h");
    vt.fill(&mut screen);
    assert_eq!(screen.modes.mouse_format, MouseFormat::Sgr);

    // Resize ends an open synchronized-output hold (matching Ghostty).
    vt.resize(32, 6);
    assert!(!vt.synchronized_output());
    vt.fill(&mut screen);
    assert!(!screen.modes.synchronized_output);
}

#[test]
fn cursor_only_controls_and_wide_char_reflow_preserve_invariants() {
    let (mut vt, mut screen, _theme) = setup(10, 3);

    vt.feed("漢x".as_bytes());
    vt.fill(&mut screen);
    assert_eq!(screen.cursor.col, 3);

    // Backspace / CUB / CUF / HT without printing must mark Vt dirty and move screen.cursor.
    vt.feed(b"\x08");
    assert!(vt.fill(&mut screen));
    assert_eq!(screen.cursor.col, 2);
    vt.feed(b"\t");
    assert!(vt.fill(&mut screen));
    assert_eq!(screen.cursor.col, 8);

    // Reflowing a wide character across width changes keeps it 2 cells wide (not 3)
    // and survives shrinking to 1 column without dangling heads or orphan tails.
    vt.resize(6, 3);
    vt.fill(&mut screen);
    assert_eq!(screen.row(0)[0].content, '漢' as u32);
    assert_eq!(screen.row(0)[0].width, 2);
    assert_eq!(screen.row(0)[1].width, 0);
    assert_eq!(screen.row(0)[2].content, 'x' as u32);

    vt.resize(1, 5);
    vt.fill(&mut screen);
    for r in 0..screen.rows {
        assert_eq!(screen.row(r)[0].width, 1);
    }
}

