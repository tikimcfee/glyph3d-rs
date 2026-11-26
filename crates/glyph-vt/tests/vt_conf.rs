//! Conformance fixture suite (`tests/fixtures/vt-conf/*.vt`, copied from
//! `glyph3d-js/tools/fixtures/vt-conf/`).
//!
//! Verifies that for all 29 fixtures:
//! 1. Feeding the entire byte stream in one call, feeding 1 byte at a time, and
//!    feeding 7-byte chunks with incremental `Vt::fill` calls between chunks
//!    all converge to the exact same `term_cells::Screen` without panicking.
//! 2. Following any fixture with `g3d-termd`'s `REPAINT` preamble resets the
//!    viewer cleanly back to the primary screen.

use std::fs;
use std::path::{Path, PathBuf};

use glyph_vt::{Theme, Vt};
use term_cells::Screen;

const EXPECTED_FIXTURE_COUNT: usize = 29;
const COLS: u16 = 40;
const ROWS: u16 = 12;

fn fixture_paths() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vt-conf");
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read fixture dir {}: {err}", dir.display()));
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("vt") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    paths.sort();
    paths
}

fn assert_screens_equal(name: &str, mode_label: &str, expected: &Screen, actual: &Screen) {
    assert_eq!(
        (expected.cols, expected.rows),
        (actual.cols, actual.rows),
        "{name} ({mode_label}): grid dimensions differ"
    );
    assert_eq!(
        expected.cursor, actual.cursor,
        "{name} ({mode_label}): cursor differs"
    );
    assert_eq!(
        expected.modes, actual.modes,
        "{name} ({mode_label}): modes differ"
    );
    assert_eq!(
        expected.title, actual.title,
        "{name} ({mode_label}): title differs"
    );
    assert_eq!(
        expected.default_fg, actual.default_fg,
        "{name} ({mode_label}): default_fg differs"
    );
    assert_eq!(
        expected.default_bg, actual.default_bg,
        "{name} ({mode_label}): default_bg differs"
    );
    assert_eq!(
        expected.wrapped, actual.wrapped,
        "{name} ({mode_label}): soft-wrap flags differ"
    );

    for row in 0..expected.rows {
        let exp_row = expected.row(row);
        let act_row = actual.row(row);
        for col in 0..usize::from(expected.cols) {
            let e = &exp_row[col];
            let a = &act_row[col];
            assert_eq!(
                expected.content_str(e),
                actual.content_str(a),
                "{name} ({mode_label}): content text differs at ({col}, {row})"
            );
            assert_eq!(
                (e.fg, e.bg, e.ul, e.attrs, e.width),
                (a.fg, a.bg, a.ul, a.attrs, a.width),
                "{name} ({mode_label}): cell style/width differs at ({col}, {row})"
            );
        }
    }
}

#[test]
fn all_29_vt_conformance_fixtures_are_chunk_invariant() {
    let paths = fixture_paths();
    assert_eq!(
        paths.len(),
        EXPECTED_FIXTURE_COUNT,
        "expected {EXPECTED_FIXTURE_COUNT} .vt fixtures, found {}",
        paths.len()
    );

    let theme = Theme::default();
    const REPAINT_PREAMBLE: &[u8] = b"\x1b[?1049l\x1b[!p\x1b[?7h\
\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\
\x1b[?1004l\x1b[?2004l\x1b[=0;1u\x1b[H\x1b[2J";

    for path in &paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>");
        let bytes = fs::read(path)
            .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()));

        // 1. Whole buffer in one call.
        let mut vt_whole = Vt::new(COLS, ROWS, &theme);
        vt_whole.feed(&bytes);
        let mut screen_whole = Screen::new(COLS, ROWS);
        vt_whole.fill(&mut screen_whole);

        // 2. Strictly 1 byte at a time (splits every UTF-8 sequence, CSI, OSC, DCS).
        let mut vt_single = Vt::new(COLS, ROWS, &theme);
        for byte in &bytes {
            vt_single.feed(std::slice::from_ref(byte));
        }
        let mut screen_single = Screen::new(COLS, ROWS);
        vt_single.fill(&mut screen_single);

        assert_screens_equal(name, "1-byte vs whole", &screen_whole, &screen_single);
        assert_eq!(
            vt_whole.text(0, 0, COLS, ROWS - 1),
            vt_single.text(0, 0, COLS, ROWS - 1),
            "{name}: extracted stream text differs between whole and 1-byte feed"
        );

        // 3. Prime-sized 7-byte chunks with incremental `fill` after every chunk,
        //    proving incremental dirty-row updates converge to the same Screen.
        let mut vt_chunked = Vt::new(COLS, ROWS, &theme);
        let mut screen_chunked = Screen::new(COLS, ROWS);
        for chunk in bytes.chunks(7) {
            vt_chunked.feed(chunk);
            vt_chunked.fill(&mut screen_chunked);
        }
        assert_screens_equal(
            name,
            "7-byte incremental fill vs whole",
            &screen_whole,
            &screen_chunked,
        );

        // 4. Subsequent `fill` with no new bytes is a strict no-op.
        let prev_gen = screen_chunked.generation;
        let prev_rows = screen_chunked.row_gen.clone();
        assert!(!vt_chunked.fill(&mut screen_chunked), "{name}: idle fill reported change");
        assert_eq!(screen_chunked.generation, prev_gen);
        assert_eq!(screen_chunked.row_gen, prev_rows);

        // 5. Feeding a termd REPAINT preamble after any fixture returns cleanly to blank primary.
        vt_chunked.feed(REPAINT_PREAMBLE);
        vt_chunked.fill(&mut screen_chunked);
        assert!(
            !screen_chunked.modes.alt_screen,
            "{name}: stayed on alternate screen after REPAINT preamble"
        );
        for row in 0..ROWS {
            assert_eq!(
                vt_chunked.row_text(row),
                "",
                "{name}: row {row} not blank after REPAINT preamble"
            );
        }
    }
}

// ============================================================================
// NOTE FOR INTEGRATION (Work Package A + Work Package B daemon round-trip):
//
// Once `crates/termd-client` (Work Package A) is merged into the `wall` branch,
// wire the live `g3d-termd` repaint fidelity check either as an ignored/opt-in
// integration test here (`crates/glyph-vt/tests/termd_roundtrip.rs` with
// `termd-client` as a `[dev-dependencies]` entry) or in `crates/wall-core` /
// `crates/termd-client/tests/`:
//
//   1. Connect `Termd::connect_or_start` against an isolated scratch namespace
//      (`wall-vt-conf-<pid>`, NEVER `wall`, `agents`, or `default`), and stop
//      that scratch daemon on drop.
//   2. Create an idle terminal at `COLS x ROWS` (`argv: ["cat"]` with `stty raw
//      -echo`, or feed raw bytes via `termd`'s fixture harness).
//   3. For each `.vt` snippet in `crates/glyph-vt/tests/fixtures/vt-conf/`:
//      - Reset the daemon terminal (`ESC c` + clear), feed the snippet, wait
//        for output quiescence, then request `termd.repaint(handle)` and feed
//        the resulting `Event::Repaint { bytes, .. }` into a fresh `Vt` (and a
//        second time into an already-dirty `Vt` to test non-fresh re-repaints).
//      - Feed the same snippet directly into a reference `Vt::new(COLS, ROWS, &theme)`.
//      - Compare the two `term_cells::Screen`s with `assert_screens_equal`
//        (matching `glyph3d-js/tools/termd-fidelity-check.mjs`).
// ============================================================================
