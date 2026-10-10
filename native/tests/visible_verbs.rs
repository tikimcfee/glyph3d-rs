//! M3 witness (2026-10-10): the Visible field's verbs are keyed by
//! (item, byte) where the stored fields key by slot — and both keys name the
//! SAME glyph. Proven in pixels, the only thing that sees the transient
//! buffer: `g-pick-repo` is rendered offscreen in `derived` and in `visible`
//! mode, plain and with one scripted pick + verb (`recolor-glyph`,
//! `recolor-line`, `set-glyph-background`), through the release binary
//! exactly as `pick-oracle` and the golden runner drive it. For each verb:
//!
//! - the verbed frame differs from the plain frame of the SAME mode in a
//!   small region only (a glyph's few hundred pixels, a row's band — never
//!   the frame), and the changed pixels take the verb's colour;
//! - the set of pixels the verb changed in `visible` coincides with the set it
//!   changed in `derived` (within a few pixels) — the (item, byte) key found
//!   the glyph the slot did.
//!
//! The camera is `repo-zoom`'s (alpha.rs at zoom 3), where one glyph is a
//! few hundred pixels; the colours are chosen so no channel can be right by
//! accident. Needs a GPU, like every other render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "fixtures/g-pick-repo";
/// Row 3 col 8 of alpha.rs is the `a` of `alpha` on `    let alpha = 1;` — a
/// real glyph (row 1 is the empty line, whose only record is the blank
/// newline) on a row with several, so `recolor-line` has a band to paint.
const PICK: [&str; 6] = ["--pick-file", "alpha.rs", "--pick-row", "3", "--pick-col", "8"];
/// Pure channels with no 0/255 neighbour: a wrong channel order or a
/// background bleed reads as a different colour, not as a near miss.
const GLYPH_RGB: &str = "e01010";
const LINE_RGB: &str = "10e010";
const BG_RGB: &str = "1010e0";

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glyph-visible-verbs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Render one frame of `g-pick-repo` from the repo-zoom camera in `mode`,
/// with `ops` (picks/verbs) applied before it; returns the PNG and stdout.
fn render(mode: &str, out: &Path, ops: &[&str]) -> (image::RgbaImage, String) {
    let bin = env!("CARGO_BIN_EXE_glyph3d-native");
    let output = Command::new(bin)
        .current_dir(native_dir())
        .args(["--load-repo", REPO, "--frames", "2", "--focus-file", "alpha.rs", "--zoom", "3", "--field-mode", mode])
        .args(ops)
        .arg("--screenshot")
        .arg(out)
        .output()
        .expect("spawn glyph3d-native");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "glyph3d-native failed ({mode}, {ops:?}):\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let img = image::open(out).expect("read the screenshot").to_rgba8();
    (img, stdout)
}

/// The pixels that differ between two frames of the same size.
fn changed_pixels(a: &image::RgbaImage, b: &image::RgbaImage) -> Vec<(u32, u32)> {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut out = Vec::new();
    for (x, y, pa) in a.enumerate_pixels() {
        if pa != b.get_pixel(x, y) {
            out.push((x, y));
        }
    }
    out
}

fn bbox(px: &[(u32, u32)]) -> (u32, u32, u32, u32) {
    px.iter().fold((u32::MAX, u32::MAX, 0, 0), |(x0, y0, x1, y1), &(x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)))
}

fn hex(rgb: &str) -> [u8; 3] {
    let v = u32::from_str_radix(rgb, 16).unwrap();
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// The channel a colour is dominated by (the verb colours are pure).
fn dominant(rgb: [u8; 3]) -> usize {
    (0..3).max_by_key(|&c| rgb[c]).unwrap()
}

/// Among `px` in `img`, the pixel whose `channel` stands furthest above the
/// other two: `(spread, pixel)`. The glyph shader blends the slot colour with
/// the group tint and its own minification dials, so a verb colour never
/// arrives byte-exact (measured: `#e01010` on alpha.rs renders ≈ (255,167,95)
/// where the plain cream is (255,249,204)); what survives the blend is which
/// channel leads, and by how much — the plain cream's red leads by 6.
fn best_spread(img: &image::RgbaImage, px: &[(u32, u32)], channel: usize) -> (i32, [u8; 4]) {
    px.iter()
        .map(|&(x, y)| {
            let p = img.get_pixel(x, y).0;
            let others = (0..3).filter(|&c| c != channel).map(|c| i32::from(p[c])).max().unwrap();
            (i32::from(p[channel]) - others, p)
        })
        .max_by_key(|(s, _)| *s)
        .unwrap_or((i32::MIN, [0; 4]))
}

/// The symmetric difference of two pixel sets.
fn sym_diff(a: &[(u32, u32)], b: &[(u32, u32)]) -> usize {
    let sa: std::collections::HashSet<_> = a.iter().copied().collect();
    let sb: std::collections::HashSet<_> = b.iter().copied().collect();
    sa.symmetric_difference(&sb).count()
}

struct VerbCase {
    /// The `--verb` to apply after the pick — or "selection", the case with
    /// no verb at all: the frame WITH the pick against the frame without it,
    /// which witnesses the mask pass (the picked glyph's highlight; offscreen
    /// frames carry it, since the mask machinery is built for both drivers).
    verb: &'static str,
    /// The verb's colour argument; None for a verb without one (then the
    /// colour assertion is skipped and only the region and the coincidence
    /// are held).
    rgb: Option<&'static str>,
    /// The changed region's bounding box must fit in this many pixels wide
    /// and tall; a glyph cell at zoom 3 measures 134×145 px, a row is a band.
    max_w: u32,
    max_h: u32,
    /// And hold at most this many changed pixels (the frame is 1600×1000).
    max_changed: usize,
    /// Some changed pixel must carry the verb's dominant channel at least
    /// this far above the other two.
    min_spread: i32,
}

fn run_case(case: &VerbCase, dir: &Path) {
    let verb_arg = match case.rgb {
        Some(rgb) => format!("{} {rgb}", case.verb),
        None => case.verb.to_string(),
    };
    let selection_case = case.verb == "selection";
    let (plain_ops, ops, reply_prefix): (Vec<&str>, Vec<&str>, String) = if selection_case {
        (Vec::new(), PICK.to_vec(), "pick:".to_string())
    } else {
        let mut ops = PICK.to_vec();
        ops.extend(["--verb", verb_arg.as_str()]);
        (PICK.to_vec(), ops, format!("verb {}", case.verb))
    };
    let mut masks: Vec<Vec<(u32, u32)>> = Vec::new();
    for mode in ["derived", "visible"] {
        let (plain, _) = render(mode, &dir.join(format!("{mode}-plain{}.png", if selection_case { "-nopick" } else { "" })), &plain_ops);
        let (verbed, log) = render(mode, &dir.join(format!("{mode}-{}.png", case.verb)), &ops);
        let reply = log.lines().find(|l| l.starts_with(&reply_prefix)).unwrap_or("").to_string();
        let changed = changed_pixels(&plain, &verbed);
        let (x0, y0, x1, y1) = bbox(&changed);
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let colour = case.rgb.map(|rgb| {
            let channel = dominant(hex(rgb));
            let (spread, px) = best_spread(&verbed, &changed, channel);
            (rgb, channel, spread, px)
        });
        println!(
            "{mode:8} {:22} changed {:6} px, bbox {w}x{h} at ({x0},{y0}), {} — {reply}",
            case.verb,
            changed.len(),
            match colour {
                Some((_, channel, spread, px)) => format!("{} leads by {spread} at best ({:?})", ["red", "green", "blue"][channel], &px[..3]),
                None => "no colour to check".to_string(),
            }
        );
        if mode == "visible" && !selection_case {
            assert!(reply.contains("item "), "the Visible reply names its item: {reply:?}");
            if case.rgb.is_some() {
                assert!(reply.contains("byte"), "the Visible reply names its byte: {reply:?}");
            }
        }
        if selection_case {
            assert!(reply.contains("byte=35"), "the pick resolved the `a` at byte 35: {reply:?}");
        }
        assert!(!changed.is_empty(), "{mode}: {} changed no pixel", case.verb);
        assert!(
            changed.len() <= case.max_changed && w <= case.max_w && h <= case.max_h,
            "{mode}: {} changed {} px in a {w}x{h} box — more than one {} ({} px, {}x{})",
            case.verb,
            changed.len(),
            if case.verb == "recolor-line" { "row" } else { "glyph" },
            case.max_changed,
            case.max_w,
            case.max_h
        );
        if let Some((rgb, channel, spread, _)) = colour {
            assert!(
                spread >= case.min_spread,
                "{mode}: no changed pixel took #{rgb}'s colour — {} leads by at most {spread}, want {}",
                ["red", "green", "blue"][channel],
                case.min_spread
            );
        }
        masks.push(changed);
    }
    // The two keys found the same glyph: the changed sets coincide.
    let (derived, visible) = (&masks[0], &masks[1]);
    let diff = sym_diff(derived, visible);
    let (bd, bv) = (bbox(derived), bbox(visible));
    println!(
        "{:22} derived vs visible: {} px in one mask only (of {} ∪ {}); bboxes {bd:?} vs {bv:?}",
        case.verb,
        diff,
        derived.len(),
        visible.len()
    );
    let budget = (derived.len().max(visible.len()) / 20).max(8);
    assert!(diff <= budget, "{}: the derived and visible masks differ by {diff} px (budget {budget})", case.verb);
    for (d, v) in [(bd.0, bv.0), (bd.1, bv.1), (bd.2, bv.2), (bd.3, bv.3)] {
        assert!(d.abs_diff(v) <= 2, "{}: mask bounding boxes differ by more than 2 px: {bd:?} vs {bv:?}", case.verb);
    }
}

#[test]
fn visible_verbs_address_the_glyph_the_slot_did() {
    let dir = scratch();
    // One cell at this zoom is 134×145 px (measured); the `a` inks ~9,200 of
    // them, the row's 18 glyphs ~57,700 in a band 247 px tall (ascender to
    // descender). The limits are a cell and a half, and a row's band two em
    // tall across the frame (≤ 16 % of its 1.6 M pixels).
    let cases = [
        VerbCase { verb: "recolor-glyph", rgb: Some(GLYPH_RGB), max_w: 200, max_h: 220, max_changed: 20_000, min_spread: 60 },
        VerbCase { verb: "recolor-line", rgb: Some(LINE_RGB), max_w: 1_600, max_h: 300, max_changed: 250_000, min_spread: 60 },
        // The background quad behind the glyph: an em-tall cell (158×299 px
        // measured), alpha-blended over the field, so blue leads by less.
        VerbCase { verb: "set-glyph-background", rgb: Some(BG_RGB), max_w: 200, max_h: 320, max_changed: 60_000, min_spread: 40 },
        // `hide-group`: the whole file leaves the frame in both modes — the
        // Visible side through `set_item_hidden` (the item is not laid out),
        // the Derived side through the CPU cull. No colour to check; the
        // vanished pixel sets must coincide.
        VerbCase { verb: "hide-group", rgb: None, max_w: 1_600, max_h: 1_000, max_changed: 1_600_000, min_spread: i32::MIN },
        // The selection mask: the pick alone tints the picked glyph's
        // coverage (`[glyph_scene] selection_tint`, additive); the Visible
        // side lays `Selection::ByteRange` out again into its mask buffer
        // (`prepare_mask` + `record_mask_draw`), the Derived side draws the
        // slot. Same glyph, same pixels.
        VerbCase { verb: "selection", rgb: None, max_w: 200, max_h: 220, max_changed: 20_000, min_spread: i32::MIN },
    ];
    for case in &cases {
        run_case(case, &dir);
    }
}
