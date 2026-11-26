//! Deterministic seeded-PRNG fuzz suite for `glyph_vt`.
//!
//! Exercises 10,000+ adversarial cases across:
//! 1. Pure random byte streams and random mid-stream resizes (including `1x1`,
//!    `1xN`, `2x2`, `3x3`, `80x24`, `120x40`).
//! 2. Structured escape-sequence fragments (CSI with mixed `;`/`:` and extreme
//!    params, OSC with invalid base64 / >4 KiB overflow / truncated UTF-8, DCS,
//!    APC, ESC, DECSTBM/DECSLRM/Origin/ReverseWrap/Mode-2027/AltScreen/DECSTR/RIS)
//!    interleaved with wide CJK, emoji + VS15/VS16, ZWJ, regional indicators,
//!    long combining-mark floods, and ill-formed/truncated UTF-8.
//! 3. Random chunk-boundary splitting equivalence (`whole` vs random `1..=5`
//!    byte slices) across 2,000 generated streams.

use glyph_vt::{Theme, Vt};
use term_cells::{Screen, CLUSTER_BASE};

/// Small, fast Xoshiro256** PRNG seeded via SplitMix64 (zero external crates).
#[derive(Clone)]
struct Rng([u64; 4]);

impl Rng {
    fn from_seed(mut seed: u64) -> Self {
        let mut next_sm = || {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        Self([next_sm(), next_sm(), next_sm(), next_sm()])
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let result = (self.0[1].wrapping_mul(5)).rotate_left(7).wrapping_mul(9);
        let t = self.0[1] << 17;
        self.0[2] ^= self.0[0];
        self.0[3] ^= self.0[1];
        self.0[1] ^= self.0[2];
        self.0[0] ^= self.0[3];
        self.0[2] ^= t;
        self.0[3] = self.0[3].rotate_left(45);
        result
    }

    #[inline]
    fn gen_range(&mut self, upper_exclusive: usize) -> usize {
        if upper_exclusive <= 1 {
            return 0;
        }
        (self.next_u64() as usize) % upper_exclusive
    }

    #[inline]
    fn gen_u8(&mut self) -> u8 {
        self.next_u64() as u8
    }

    fn pick_grid_size(&mut self) -> (u16, u16) {
        // Heavily weight tiny corner-case geometries (1x1, 1x4, 2x1, 2x2, 3x3, 5x4)
        // alongside typical terminal dimensions.
        const SIZES: &[(u16, u16)] = &[
            (1, 1),
            (1, 3),
            (2, 1),
            (2, 2),
            (2, 4),
            (3, 2),
            (3, 3),
            (4, 3),
            (5, 4),
            (8, 6),
            (13, 7),
            (40, 12),
            (80, 24),
            (120, 40),
        ];
        if self.gen_range(4) < 3 {
            SIZES[self.gen_range(SIZES.len())]
        } else {
            (
                (1 + self.gen_range(120)) as u16,
                (1 + self.gen_range(45)) as u16,
            )
        }
    }
}

fn assert_screen_invariants(case_id: usize, vt: &Vt, screen: &Screen) {
    let cols = vt.cols();
    let rows = vt.rows();
    assert_eq!(screen.cols, cols, "case {case_id}: cols mismatch");
    assert_eq!(screen.rows, rows, "case {case_id}: rows mismatch");
    assert_eq!(
        screen.cells.len(),
        usize::from(cols) * usize::from(rows),
        "case {case_id}: cells.len() mismatch"
    );
    assert_eq!(screen.row_gen.len(), usize::from(rows), "case {case_id}");
    assert_eq!(screen.wrapped.len(), usize::from(rows), "case {case_id}");
    assert!(
        screen.cursor.col < cols,
        "case {case_id}: cursor.col {} >= cols {}",
        screen.cursor.col,
        cols
    );
    assert!(
        screen.cursor.row < rows,
        "case {case_id}: cursor.row {} >= rows {}",
        screen.cursor.row,
        rows
    );
    assert_eq!(
        screen.cursor_color,
        vt.cursor_color(),
        "case {case_id}: cursor_color mismatch"
    );
    let cursor_alpha = screen.cursor_color & 0xff00_0000;
    assert!(
        cursor_alpha == 0 || cursor_alpha == 0xff00_0000,
        "case {case_id}: invalid cursor_color alpha {cursor_alpha:#x}"
    );
    let delta = vt.last_fill_delta();
    assert_eq!(
        delta.rewritten_cols().len(),
        usize::from(rows),
        "case {case_id}: delta.rewritten_cols().len() mismatch"
    );
    for (r_idx, &(lo, hi)) in delta.rewritten_cols().iter().enumerate() {
        assert!(
            lo <= hi && hi <= cols,
            "case {case_id}: invalid rewritten span ({lo}, {hi}) on row {r_idx} for cols {cols}"
        );
    }

    let cols_usize = usize::from(cols);
    for r in 0..rows {
        let row = screen.row(r);
        for c in 0..cols_usize {
            let cell = &row[c];
            assert!(
                cell.width <= 2,
                "case {case_id}: invalid cell width {} at ({c}, {r})",
                cell.width
            );
            assert_eq!(
                cell.fg & 0xff00_0000,
                0xff00_0000,
                "case {case_id}: unresolved fg alpha at ({c}, {r})"
            );
            let bg_alpha = cell.bg & 0xff00_0000;
            assert!(
                bg_alpha == 0 || bg_alpha == 0xff00_0000,
                "case {case_id}: invalid bg alpha {bg_alpha:#x} at ({c}, {r})"
            );

            if cell.width == 2 {
                assert!(
                    c + 1 < cols_usize,
                    "case {case_id}: dangling wide head at right edge ({c}, {r}) on width {cols}"
                );
                let tail = &row[c + 1];
                assert_eq!(
                    tail.width, 0,
                    "case {case_id}: wide head at ({c}, {r}) not followed by width-0 tail"
                );
                assert_eq!(
                    tail.content, 0,
                    "case {case_id}: wide tail at ({}, {r}) has non-zero content",
                    c + 1
                );
                assert_eq!(
                    tail.bg, cell.bg,
                    "case {case_id}: wide tail bg differs from head at ({c}, {r})"
                );
            } else if cell.width == 0 {
                assert!(
                    c > 0 && row[c - 1].width == 2,
                    "case {case_id}: orphan width-0 tail at ({c}, {r}) without wide head at left"
                );
            }

            if cell.content >= CLUSTER_BASE {
                let idx = (cell.content - CLUSTER_BASE) as usize;
                assert!(
                    idx < screen.clusters.len(),
                    "case {case_id}: out-of-bounds cluster index {idx} (len {})",
                    screen.clusters.len()
                );
                assert!(
                    !screen.clusters[idx].is_empty(),
                    "case {case_id}: empty cluster string at {idx}"
                );
            }
        }
    }
}

fn push_adversarial_fragment(rng: &mut Rng, out: &mut Vec<u8>) {
    match rng.gen_range(12) {
        // 0: Printable ASCII & controls (CR, LF, BS, HT, BEL, SO, SI, CAN, SUB, DEL).
        0 => {
            const BYTES: &[u8] = b"abcdefXYZ019 \t\r\n\x07\x08\x0e\x0f\x18\x1a\x7f";
            let len = 1 + rng.gen_range(16);
            for _ in 0..len {
                out.push(BYTES[rng.gen_range(BYTES.len())]);
            }
        }
        // 1: Wide CJK, emoji, VS15/VS16, ZWJ, skin tones, regional indicators.
        1 => {
            const GRAPHEMES: &[&str] = &[
                "漢",
                "語",
                "⌚",
                "❤️",
                "⌚\u{fe0e}",
                "\u{2764}\u{fe0f}",
                "👨\u{200d}👩\u{200d}👧",
                "👋\u{1f3ff}",
                "🇦🇧🇨",
                "#\u{fe0f}\u{20e3}",
                "e\u{0301}\u{0308}",
            ];
            let count = 1 + rng.gen_range(6);
            for _ in 0..count {
                out.extend_from_slice(GRAPHEMES[rng.gen_range(GRAPHEMES.len())].as_bytes());
            }
        }
        // 2: Combining mark flood (tests cluster length cap and O(1) bounded work).
        2 => {
            out.push(b'a');
            let marks = 1 + rng.gen_range(96);
            for _ in 0..marks {
                out.extend_from_slice("\u{0301}".as_bytes());
            }
        }
        // 3: Truncated / overlong / surrogate / lone-continuation UTF-8 bytes.
        3 => {
            const BAD_UTF8: &[&[u8]] = &[
                &[0x80],
                &[0xbf],
                &[0xc0, 0xaf],
                &[0xc2],
                &[0xe0, 0x80, 0xaf],
                &[0xe2, 0x82],
                &[0xed, 0xa0, 0x80],
                &[0xf0, 0x9f, 0x91],
                &[0xf4, 0x90, 0x80, 0x80],
                &[0xff, 0xfe],
            ];
            out.extend_from_slice(BAD_UTF8[rng.gen_range(BAD_UTF8.len())]);
        }
        // 4: Random CSI sequence (possibly truncated mid-sequence).
        4 => {
            out.extend_from_slice(b"\x1b[");
            const PREFIXES: &[&[u8]] = &[b"", b"?", b">", b"=", b"<", b"!"];
            out.extend_from_slice(PREFIXES[rng.gen_range(PREFIXES.len())]);
            let n_params = rng.gen_range(10);
            const PARAMS: &[&[u8]] = &[
                b"", b"0", b"1", b"2", b"4", b"5", b"6", b"7", b"25", b"38", b"48", b"58", b"69",
                b"1000", b"1006", b"1016", b"1049", b"2004", b"2026", b"2027", b"65535", b"999999",
            ];
            for idx in 0..n_params {
                if idx > 0 {
                    out.push(if rng.gen_range(3) == 0 { b':' } else { b';' });
                }
                out.extend_from_slice(PARAMS[rng.gen_range(PARAMS.len())]);
            }
            const INTERS: &[&[u8]] = &[b"", b" ", b"!", b"$", b"?$", b">"];
            out.extend_from_slice(INTERS[rng.gen_range(INTERS.len())]);
            if rng.gen_range(8) != 0 {
                const FINALS: &[u8] = b"ABCDEFGHJKLMPSTXZbcdfghlmnprstuq@`";
                out.push(FINALS[rng.gen_range(FINALS.len())]);
            }
        }
        // 5: SGR stress sequence (colon + semicolon extended colors + underlines).
        5 => {
            const SGRS: &[&[u8]] = &[
                b"\x1b[0m",
                b"\x1b[1;2;3;4:3;5;7;8;9;53m",
                b"\x1b[38;5;201;48:2::10:20:30;58:5:196m",
                b"\x1b[58:4:m",
                b"\x1b[38:2:1:2:3:4:5:6m",
                b"\x1b[22;23;24;25;27;28;29;39;49;55;59m",
            ];
            out.extend_from_slice(SGRS[rng.gen_range(SGRS.len())]);
        }
        // 6: Mode & margin combos (Origin, DECLRMM, DECSTBM, ReverseWrap, AltScreen, Mode 2027).
        6 => {
            const MODES: &[&[u8]] = &[
                b"\x1b[?6h",
                b"\x1b[?6l",
                b"\x1b[?7h",
                b"\x1b[?7l",
                b"\x1b[?45h",
                b"\x1b[?1045h",
                b"\x1b[?69h\x1b[2;4s",
                b"\x1b[?69l",
                b"\x1b[2;4r",
                b"\x1b[r",
                b"\x1b[?2027h",
                b"\x1b[?2027l",
                b"\x1b[?1049h",
                b"\x1b[?1049l",
                b"\x1b[?47h",
                b"\x1b[?47l",
                b"\x1b[?2026h",
                b"\x1b[?2026l",
                b"\x1b[?1003h\x1b[?1016h",
                b"\x1b[>5u",
                b"\x1b[<2u",
                b"\x1b[=3;2u",
                b"\x1b[?6s\x1b[?69s",
                b"\x1b[?6r\x1b[?69r",
            ];
            out.extend_from_slice(MODES[rng.gen_range(MODES.len())]);
        }
        // 7: OSC commands (titles, colors, hyperlinks, OSC 52, OSC 7, OSC 133, >4KB overflow, truncated).
        7 => {
            out.extend_from_slice(b"\x1b]");
            match rng.gen_range(8) {
                0 => out.extend_from_slice(b"2;Title \xe2\x82\xac"),
                1 => out.extend_from_slice(b"4;1;#123456;2;rgb:ff/00/80"),
                2 => out.extend_from_slice(b"8;id=link1;https://example.com/a"),
                3 => out.extend_from_slice(b"8;;"),
                4 => out.extend_from_slice(b"52;c;SGVsbG8="),
                5 => out.extend_from_slice(b"52;p;?"),
                6 => out.extend_from_slice(b"7;file://localhost/Users/test"),
                _ => {
                    out.extend_from_slice(b"0;");
                    let len = if rng.gen_range(6) == 0 { 4200 } else { 12 };
                    out.resize(out.len() + len, b'x');
                }
            }
            match rng.gen_range(5) {
                0 => out.push(0x07),
                1 => out.extend_from_slice(b"\x1b\\"),
                2 => out.push(0x9c),
                3 => out.push(0x18), // CAN abort
                _ => {}              // leave unclosed / truncated!
            }
        }
        // 8: DCS & APC sequences (possibly unclosed).
        8 => {
            const DCS: &[&[u8]] = &[
                b"\x1bP$qm\x1b\\",
                b"\x1bP+q544e\x1b\\",
                b"\x1bP1;2|abcdef",
                b"\x1b_Gi=1;aaaa\x1b\\",
                b"\x1b_unterminated_apc",
            ];
            out.extend_from_slice(DCS[rng.gen_range(DCS.len())]);
        }
        // 9: ESC sequences (DECSC/DECRC, IND, NEL, RI, SS2/SS3, charsets, DECALN, DECSTR, RIS).
        9 => {
            const ESCS: &[&[u8]] = &[
                b"\x1b7",
                b"\x1b8",
                b"\x1bD",
                b"\x1bE",
                b"\x1bH",
                b"\x1bM",
                b"\x1bN",
                b"\x1bO",
                b"\x1b#8",
                b"\x1b(0",
                b"\x1b)B",
                b"\x1b[!p",
                b"\x1bc",
            ];
            out.extend_from_slice(ESCS[rng.gen_range(ESCS.len())]);
        }
        // 10: Queries (DSR/CPR under origin, DA, DECRQM, XTWINOPS, OSC ?).
        10 => {
            out.extend_from_slice(
                b"\x1b[6n\x1b[?6n\x1b[c\x1b[?25$p\x1b[18t\x1b]10;?\x07\x1b[?u",
            );
        }
        // 11: Raw random noise burst.
        _ => {
            let len = 1 + rng.gen_range(24);
            for _ in 0..len {
                out.push(rng.gen_u8());
            }
        }
    }
}

#[test]
fn fuzz_random_bytes_and_resizes_3000_cases() {
    let mut rng = Rng::from_seed(0x5741_4c4c_5654_0001);
    let mut themes = [Theme::default(), Theme::default()];
    themes[1].bold_is_bright = true;
    let mut buf = Vec::with_capacity(512);

    for case_id in 0..3_000 {
        let (cols, rows) = rng.pick_grid_size();
        let theme = &themes[case_id & 1];
        let mut vt = Vt::new(cols, rows, theme);
        let mut screen = Screen::new(cols, rows);

        let steps = 1 + rng.gen_range(6);
        for _ in 0..steps {
            buf.clear();
            let len = 1 + rng.gen_range(256);
            for _ in 0..len {
                buf.push(rng.gen_u8());
            }
            vt.feed(&buf);

            if rng.gen_range(3) == 0 {
                let (nc, nr) = rng.pick_grid_size();
                vt.resize(nc, nr);
            }

            vt.fill(&mut screen);
            assert_screen_invariants(case_id, &vt, &screen);

            let c0 = rng.gen_range(usize::from(vt.cols()) + 1) as u16;
            let r0 = rng.gen_range(usize::from(vt.rows())) as u16;
            let c1 = rng.gen_range(usize::from(vt.cols()) + 1) as u16;
            let r1 = rng.gen_range(usize::from(vt.rows())) as u16;
            let _ = vt.text(c0, r0, c1, r1);
            let _ = vt.block_text(c0, r0, c1, r1);
            let _ = vt.row_text(r0);
        }
    }
}

#[test]
fn fuzz_escape_fragments_utf8_and_resizes_5000_cases() {
    let mut rng = Rng::from_seed(0x5741_4c4c_5654_0002);
    let theme = Theme::default();
    let mut buf = Vec::with_capacity(1024);

    for case_id in 0..5_000 {
        let (cols, rows) = rng.pick_grid_size();
        let mut vt = Vt::new(cols, rows, &theme);
        let mut screen = Screen::new(cols, rows);

        let steps = 2 + rng.gen_range(6);
        for _ in 0..steps {
            buf.clear();
            let frags = 1 + rng.gen_range(8);
            for _ in 0..frags {
                push_adversarial_fragment(&mut rng, &mut buf);
            }
            vt.feed(&buf);

            if rng.gen_range(3) == 0 {
                let (nc, nr) = rng.pick_grid_size();
                vt.resize(nc, nr);
            }

            vt.fill(&mut screen);
            assert_screen_invariants(case_id, &vt, &screen);

            let c0 = rng.gen_range(usize::from(vt.cols()) + 1) as u16;
            let r0 = rng.gen_range(usize::from(vt.rows())) as u16;
            let c1 = rng.gen_range(usize::from(vt.cols()) + 1) as u16;
            let r1 = rng.gen_range(usize::from(vt.rows())) as u16;
            let _ = vt.text(c0, r0, c1, r1);
            let _ = vt.block_text(c0, r0, c1, r1);
            let _ = vt.hyperlink(c0.min(vt.cols() - 1), r0);
            let _ = vt.pwd();
            let _ = vt.modes();
            if case_id & 1 == 0 {
                let _ = vt.drain_events().count();
            }
        }
    }
}

#[test]
fn fuzz_chunk_boundary_invariance_2000_cases() {
    let mut rng = Rng::from_seed(0x5741_4c4c_5654_0003);
    let theme = Theme::default();
    let mut stream = Vec::with_capacity(1024);

    for case_id in 0..2_000 {
        let (cols, rows) = rng.pick_grid_size();
        stream.clear();
        let frags = 2 + rng.gen_range(10);
        for _ in 0..frags {
            push_adversarial_fragment(&mut rng, &mut stream);
        }

        let mut vt_whole = Vt::new(cols, rows, &theme);
        vt_whole.feed(&stream);
        let mut s_whole = Screen::new(cols, rows);
        vt_whole.fill(&mut s_whole);
        assert_screen_invariants(case_id, &vt_whole, &s_whole);

        let mut vt_split = Vt::new(cols, rows, &theme);
        let mut s_split = Screen::new(cols, rows);
        let mut offset = 0;
        while offset < stream.len() {
            let step = (1 + rng.gen_range(5)).min(stream.len() - offset);
            vt_split.feed(&stream[offset..offset + step]);
            if rng.gen_range(4) == 0 {
                vt_split.fill(&mut s_split);
            }
            offset += step;
        }
        vt_split.fill(&mut s_split);

        assert_eq!(s_whole.cursor, s_split.cursor, "case {case_id}: cursor");
        assert_eq!(s_whole.modes, s_split.modes, "case {case_id}: modes");
        assert_eq!(s_whole.title, s_split.title, "case {case_id}: title");
        assert_eq!(s_whole.wrapped, s_split.wrapped, "case {case_id}: wrapped");
        for r in 0..rows {
            for c in 0..usize::from(cols) {
                let a = &s_whole.row(r)[c];
                let b = &s_split.row(r)[c];
                assert_eq!(
                    s_whole.content_str(a),
                    s_split.content_str(b),
                    "case {case_id}: content text at ({c}, {r})"
                );
                assert_eq!(
                    (a.fg, a.bg, a.ul, a.attrs, a.width),
                    (b.fg, b.bg, b.ul, b.attrs, b.width),
                    "case {case_id}: cell attrs at ({c}, {r})"
                );
            }
        }
    }
}
