//! Host-placed texts (2026-10-31): `repo::load_texts` puts each in-memory
//! text where its caller says, and `build_text_scene` makes a scene of them
//! whose picks read the texts from memory and report, through
//! `SceneLike::picked_glyph`, which text and which byte. The definition
//! engine's makepad view stands on exactly this: its passage and cards are
//! texts it places, and a click on a glyph becomes a word by its byte.
//!
//! Built as the makepad host builds (`SceneLoad::host()`: no device), so
//! these run without a GPU.

use std::sync::Arc;

use glyph3d_native::glyph_scene::PickCommand;
use glyph3d_native::repo::{load_texts, RepoParams};
use glyph3d_native::{build_text_scene, CameraMode, PlacedText, SceneCullOptions, SceneLoad};

fn atlas() -> Arc<glyph3d_native::atlas::Atlas> {
    Arc::new(glyph3d_native::atlas::Atlas::load(&glyph3d_native::default_emoji_sheet()))
}

#[test]
fn each_text_sits_at_its_given_offset() {
    let texts = vec![("passage.txt".to_string(), b"one two\nthree".to_vec()), ("card-1.txt".to_string(), b"[-a-]{+b+}".to_vec())];
    let offsets = vec![[0.0, 0.0, 0.0], [12.5, -3.0, -2.0]];
    let load = load_texts(texts, offsets.clone(), &RepoParams::default(), None).unwrap();
    assert_eq!(load.files.len(), 2);
    for (view, offset) in load.files.iter().zip(&offsets) {
        assert_eq!(&view.offset, offset, "{}", view.rel_path);
        assert_eq!(&load.groups[view.group_id as usize].cols[0][..3], offset, "{}: the drawn row", view.rel_path);
    }
    assert!(load.bounds_min[0] <= 0.0 && load.bounds_max[0] > 12.5, "bounds cover both texts: {:?} {:?}", load.bounds_min, load.bounds_max);
    assert!(load.bounds_min[2] <= -2.0, "and the card's depth");
}

#[test]
fn texts_a_walk_would_skip_are_refused_by_name() {
    let params = RepoParams::default();
    let doubled = vec![("a.txt".to_string(), b"x".to_vec()), ("a.txt".to_string(), b"y".to_vec())];
    assert!(load_texts(doubled, vec![[0.0; 3]; 2], &params, None).err().unwrap().contains("two texts are named a.txt"));
    let binary = vec![("b.bin".to_string(), vec![0xff, 0xfe])];
    assert!(load_texts(binary, vec![[0.0; 3]], &params, None).err().unwrap().contains("b.bin is not valid UTF-8"));
    let unplaced = vec![("c.txt".to_string(), b"z".to_vec())];
    assert!(load_texts(unplaced, vec![], &params, None).is_err());
}

#[test]
fn a_pick_names_the_text_and_the_byte_from_memory() {
    let texts = vec![
        PlacedText { rel_path: "passage.txt".into(), bytes: "An ephemeral joy.\nThe café was brief.".as_bytes().to_vec(), offset: [0.0; 3] },
        PlacedText { rel_path: "options.txt".into(), bytes: b"1. fleeting\n2. brief".to_vec(), offset: [30.0, 0.0, 0.0] },
    ];
    let (mut scene, _) = build_text_scene(
        &SceneLoad::host(),
        atlas(),
        texts,
        &RepoParams::default(),
        CameraMode::Fly,
        SceneCullOptions::default(),
        false,
    )
    .unwrap();
    assert_eq!(scene.picked_glyph(), None, "nothing picked yet");

    // Row 1, col 9 of the passage is the `w` of "was", after "The café ":
    // `é` is two bytes and one leader, so the byte in its line is col + 1.
    // A pick that reported the leader index instead lands a byte early, on
    // the space before it.
    let line = scene.apply_pick(&PickCommand::RowCol { file: "passage.txt".into(), row: 1, col: 9 });
    assert!(line.is_some(), "the pick hit");
    let (rel_path, byte) = scene.picked_glyph().expect("a glyph is picked");
    let passage = "An ephemeral joy.\nThe café was brief.";
    assert_eq!(rel_path, "passage.txt");
    assert_eq!(&passage[byte..byte + 3], "was", "byte {byte}");

    // A pick on the other text names it, at its own byte.
    scene.apply_pick(&PickCommand::RowCol { file: "options.txt".into(), row: 1, col: 3 });
    assert_eq!(scene.picked_glyph(), Some(("options.txt".to_string(), 15)));

    scene.apply_pick(&PickCommand::Clear);
    assert_eq!(scene.picked_glyph(), None, "a cleared selection names no glyph");
}
