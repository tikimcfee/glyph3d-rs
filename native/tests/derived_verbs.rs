//! The Derived field's glyph verbs say what they cannot do (2026-10-11).
//!
//! A Derived slot stores a glyph's x and nothing else of its placement: y and
//! z derive from its item in the vertex stage, and advance and height come
//! from the atlas table. So `nudge-glyph`'s y/z and `scale-glyph` have no
//! lane to land in. Until 2026-10-11 the field dropped them silently while
//! the reply claimed them applied; now the reply says "not representable",
//! as the Visible field's does, and nothing is applied for those components.
//! Instanced (still a mode until its retirement) applies all of them.
//!
//! Witnessed through the release binary, offscreen, at `repo-zoom`'s camera
//! (alpha.rs at zoom 3, where one glyph is a few hundred pixels): each verb's
//! reply, and whether its frame differs from the plain frame. Needs a GPU,
//! like every render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "fixtures/g-pick-repo";
/// The `a` of `alpha` on `    let alpha = 1;` (row 3 col 8 of alpha.rs), as
/// in `visible_verbs.rs`.
const PICK: [&str; 6] = ["--pick-file", "alpha.rs", "--pick-row", "3", "--pick-col", "8"];

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glyph-derived-verbs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Render g-pick-repo at the repo-zoom camera in `mode` with the pick and
/// `verb` (none for the plain frame); returns the PNG bytes and the verb's
/// reply line.
fn render(mode: &str, verb: Option<&str>, out: &Path) -> (Vec<u8>, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_glyph3d-native"));
    cmd.current_dir(native_dir())
        .args(["--load-repo", REPO, "--frames", "2", "--field-mode", mode])
        .args(["--focus-file", "alpha.rs", "--zoom", "3"])
        .args(PICK);
    if let Some(v) = verb {
        cmd.args(["--verb", v]);
    }
    let output = cmd.arg("--screenshot").arg(out).output().expect("spawn glyph3d-native");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "glyph3d-native failed ({mode}, {verb:?}):\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply = stdout.lines().find(|l| l.starts_with("verb ")).unwrap_or("").to_string();
    (std::fs::read(out).expect("read the screenshot"), reply)
}

#[test]
fn derived_glyph_verbs_reply_not_representable_and_apply_nothing_they_cannot() {
    let dir = scratch();
    let (plain_derived, _) = render("derived", None, &dir.join("derived-plain.png"));
    let (plain_instanced, _) = render("instanced", None, &dir.join("instanced-plain.png"));

    // Derived: a y-only nudge and a scale are refused in the reply and move
    // no pixel; the x component of a nudge still applies.
    let (y_nudge, reply) = render("derived", Some("nudge-glyph 0 0.5 0"), &dir.join("derived-nudge-y.png"));
    println!("derived nudge y: {reply}");
    assert!(reply.contains("not representable"), "the Derived reply refuses the y nudge: {reply:?}");
    assert!(y_nudge == plain_derived, "a y-only nudge applies nothing in Derived");

    let (scaled, reply) = render("derived", Some("scale-glyph 2"), &dir.join("derived-scale.png"));
    println!("derived scale: {reply}");
    assert!(reply.contains("not representable"), "the Derived reply refuses the scale: {reply:?}");
    assert!(scaled == plain_derived, "scale-glyph applies nothing in Derived");

    let (x_nudge, reply) = render("derived", Some("nudge-glyph 0.5 0.5 0"), &dir.join("derived-nudge-xy.png"));
    println!("derived nudge xy: {reply}");
    assert!(reply.contains("not representable") && reply.contains("x -> "), "x applies, y is refused: {reply:?}");
    assert!(x_nudge != plain_derived, "the x component of a nudge still moves the glyph in Derived");

    // Instanced keeps its behaviour: every component applies, no refusal.
    let (y_nudge, reply) = render("instanced", Some("nudge-glyph 0 0.5 0"), &dir.join("instanced-nudge-y.png"));
    println!("instanced nudge y: {reply}");
    assert!(!reply.contains("not representable") && reply.contains("pos -> "), "{reply:?}");
    assert!(y_nudge != plain_instanced, "a y nudge moves the glyph in Instanced");
    let (scaled, reply) = render("instanced", Some("scale-glyph 2"), &dir.join("instanced-scale.png"));
    println!("instanced scale: {reply}");
    assert!(!reply.contains("not representable") && reply.contains("advance/height"), "{reply:?}");
    assert!(scaled != plain_instanced, "scale-glyph grows the glyph in Instanced");
}
