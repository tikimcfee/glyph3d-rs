use std::path::PathBuf;
use clap::CommandFactory;
use super::*;
use crate::fold;
use crate::glyph_scene::{PickCommand, Verb};

fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
    let argv = std::iter::once("glyph3d-native").chain(args.iter().copied());
    let matches = Cli::command().try_get_matches_from(argv)?;
    Ok(parse_cli_from(matches))
}

fn parse(args: &[&str]) -> Cli {
    try_parse(args).unwrap_or_else(|e| panic!("parse failed: {e}"))
}

#[test]
fn defaults_match_old_parser() {
    let cli = parse(&[]);
    assert!(cli.screenshot.is_none());
    assert_eq!(cli.frames, 1);
    assert!(!cli.demo);
    assert!(cli.render_file.is_none());
    assert_eq!(cli.copies, 1);
    assert_eq!(cli.zoom, 1.0);
    assert!(cli.engine_trie.is_none());
    assert!(cli.emoji_sheet.is_none());
    assert!(cli.engine_render.is_none());
    assert!(cli.load_repo.is_none());
    // The DEFAULT is `hyper` (pure-Rust parallel direct engine).
    assert_eq!(cli.repo_engine, "hyper");
    // THE DEFAULT THE SCREENSHOT BASELINES DEPEND ON. A change here moves
    // repo-wide.png and repo-zoom.png, so it is pinned in the CLI layer too
    // and not only in RepoParams::default.
    assert_eq!(cli.wrap_mode, "back");
    assert_eq!(parse_wrap_mode(&cli.wrap_mode), fold::WrapMode::Back);
    // ...and the non-default is still reachable and still spelled the same.
    assert_eq!(parse_wrap_mode("down"), fold::WrapMode::Down);
    // Same standing as wrap_mode: the wrap staircase's pitch moves the
    // same baselines (repo-wide renders the staircase), so the CLI layer
    // pins the default too, not only RepoParams::default (repo.rs).
    assert_eq!(cli.z_wrap_spacing, 0.15);
    // The sequence pass: cluster is the default (since 2026-09-22); leader
    // is the reachable other spelling, and the emoji view pins it by hand.
    assert_eq!(cli.cluster_mode, "cluster");
    assert_eq!(parse_cluster_mode(&cli.cluster_mode), fold::ClusterMode::Cluster);
    assert_eq!(parse_cluster_mode("leader"), fold::ClusterMode::Leader);
    assert_eq!(cli.layout_mode, "shelf");
    assert_eq!(parse_layout_mode(&cli.layout_mode), crate::repo::RepoLayoutMode::Shelf);
    assert_eq!(parse_layout_mode("carrel"), crate::repo::RepoLayoutMode::Carrel);
    // Syntax color mode: flat is the default for instant geometric load without unneeded color allocations.
    assert_eq!(cli.color_mode, "flat");
    assert_eq!(parse_color_mode(&cli.color_mode), crate::repo::ColorMode::Flat);
    assert_eq!(parse_color_mode("syntax"), crate::repo::ColorMode::Syntax);
    assert!(!cli.repo_verify);
    assert!(cli.focus_file.is_none());
    assert!(!cli.repo_scan_only);
    assert!(!cli.no_cull);
    assert!(!cli.no_ui);
    assert!(!cli.no_greeking);
    assert!(!cli.greek_pure);
    assert!(!cli.greek_smooth);
    assert!(cli.greek_onset_px.is_none());
    assert!(cli.launch_config.is_none());
    assert!(!cli.file_backgrounds);
    assert!(cli.file_bg_color.is_none());
    assert!(cli.lod_min_px.is_none());
    assert!(cli.screenshot_frame.is_none());
    assert!(cli.screenshot_out.is_none());
    assert!(cli.ops.is_empty());
    assert!(!cli.gpu_key);
    assert!(!cli.gpu_profile);
    // Fifo is the default because it is what every FPS figure before
    // 2026-09-07 was measured under; changing it would make old numbers
    // incomparable without saying so.
    assert_eq!(cli.present_mode, "fifo");
    assert_eq!(parse_present_mode(&cli.present_mode), wgpu::PresentMode::Fifo);
}

#[test]
fn scalar_flags_parse() {
    let cli = parse(&[
        "--screenshot", "out.png", "--frames", "2", "--demo", "--copies", "3", "--zoom",
        "2.5", "--no-cull", "--no-ui", "--launch-config", "config.toml",
        "--file-backgrounds", "--file-bg-color", "0.15,0.15,0.20,0.80",
        "--lod-min-px", "2.0",
        "--load-repo", "fixtures/g-pick-repo",
        "--repo-engine", "batch", "--repo-verify", "--focus-file", "alpha",
        "--wrap-mode", "back", "--z-wrap-spacing", "0.6", "--cluster-mode", "cluster",
        "--layout-mode", "carrel",
        "--color-mode", "flat",
        "--render-file", "src/main.rs", "--engine-trie", "t.bin",
        "--engine-render", "c.rs",
        "--present-mode", "mailbox", "--gpu-key", "--gpu-profile",
        "--emoji-sheet", "sheets/other.bin",
    ]);
    assert_eq!(cli.emoji_sheet, Some(PathBuf::from("sheets/other.bin")));
    assert_eq!(cli.present_mode, "mailbox");
    assert_eq!(parse_present_mode(&cli.present_mode), wgpu::PresentMode::Mailbox);
    assert!(cli.gpu_key);
    assert!(cli.gpu_profile);
    assert_eq!(cli.screenshot, Some(PathBuf::from("out.png")));
    assert_eq!(cli.frames, 2);
    assert!(cli.demo);
    assert_eq!(cli.copies, 3);
    assert_eq!(cli.zoom, 2.5);
    assert!(cli.no_cull);
    assert!(cli.no_ui);
    assert_eq!(cli.launch_config, Some(PathBuf::from("config.toml")));
    assert!(cli.file_backgrounds);
    assert_eq!(cli.file_bg_color, Some([0.15, 0.15, 0.20, 0.80]));
    assert_eq!(cli.lod_min_px, Some(2.0));
    assert_eq!(cli.load_repo, Some(PathBuf::from("fixtures/g-pick-repo")));
    assert_eq!(cli.repo_engine, "batch");
    assert_eq!(cli.wrap_mode, "back");
    assert_eq!(parse_wrap_mode(&cli.wrap_mode), fold::WrapMode::Back);
    assert_eq!(cli.z_wrap_spacing, 0.6);
    assert_eq!(cli.cluster_mode, "cluster");
    assert_eq!(parse_cluster_mode(&cli.cluster_mode), fold::ClusterMode::Cluster);
    assert_eq!(cli.layout_mode, "carrel");
    assert_eq!(parse_layout_mode(&cli.layout_mode), crate::repo::RepoLayoutMode::Carrel);
    assert_eq!(cli.color_mode, "flat");
    assert_eq!(parse_color_mode(&cli.color_mode), crate::repo::ColorMode::Flat);
    assert!(cli.repo_verify);
    assert_eq!(cli.focus_file.as_deref(), Some("alpha"));
    assert_eq!(cli.render_file, Some(PathBuf::from("src/main.rs")));
    assert_eq!(cli.engine_trie, Some(PathBuf::from("t.bin")));
    assert_eq!(cli.engine_render, Some(PathBuf::from("c.rs")));
}

/// An unknown mode is REFUSED at the boundary, not folded to the default.
/// A silent fallback would lay out mode A while the operator asked for B,
/// and nothing downstream could tell them apart from a correct mode-A run.
#[test]
fn an_unknown_wrap_mode_is_refused() {
    let text = match try_parse(&["--wrap-mode", "sideways"]) {
        Ok(_) => panic!("clap must refuse an unknown wrap mode"),
        Err(e) => e.to_string(),
    };
    assert!(text.contains("sideways"), "the error must name the bad value: {text}");
}

/// Same rule as the wrap mode's: an unknown cluster mode is REFUSED at the
/// boundary, not folded to the default — a silent fallback would render
/// mode A while the operator believed they asked for B.
#[test]
fn an_unknown_cluster_mode_is_refused() {
    let text = match try_parse(&["--cluster-mode", "grapheme"]) {
        Ok(_) => panic!("clap must refuse an unknown cluster mode"),
        Err(e) => e.to_string(),
    };
    assert!(text.contains("grapheme"), "the error must name the bad value: {text}");
}

/// Same rule: an unknown layout mode is REFUSED at the boundary.
#[test]
fn an_unknown_layout_mode_is_refused() {
    let text = match try_parse(&["--layout-mode", "grid"]) {
        Ok(_) => panic!("clap must refuse an unknown layout mode"),
        Err(e) => e.to_string(),
    };
    assert!(text.contains("grid"), "the error must name the bad value: {text}");
}

/// An unknown color mode is REFUSED at the boundary.
#[test]
fn an_unknown_color_mode_is_refused() {
    let text = match try_parse(&["--color-mode", "neon"]) {
        Ok(_) => panic!("clap must refuse an unknown color mode"),
        Err(e) => e.to_string(),
    };
    assert!(text.contains("neon"), "the error must name the bad value: {text}");
}

/// Out-of-domain spacing is REFUSED at the boundary with the flag and the
/// value named — not passed down to ItemParams::validate, whose panic
/// would say "z_step" and nothing about the command line. NaN is the
/// dangerous one (two NaN layouts compare bit-equal — see
/// ItemParams::validate); negative is a forward staircase no baseline
/// has ever rendered.
#[test]
fn an_out_of_domain_z_wrap_spacing_is_refused() {
    for bad in ["-0.1", "NaN", "inf", "abc"] {
        let text = match try_parse(&["--z-wrap-spacing", bad]) {
            Ok(_) => panic!("clap must refuse --z-wrap-spacing {bad}"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("--z-wrap-spacing"), "error must name the flag: {text}");
    }
    // The boundary itself is in domain: 0 is the documented flat layout.
    let cli = parse(&["--z-wrap-spacing", "0"]);
    assert_eq!(cli.z_wrap_spacing, 0.0);
}

#[test]
fn repeated_scalar_flag_is_last_wins() {
    // The hand-rolled parser silently took the last occurrence.
    let cli = parse(&["--zoom", "2", "--zoom", "3"]);
    assert_eq!(cli.zoom, 3.0);
}

#[test]
fn op_stream_preserves_cli_order() {
    let cli = parse(&[
        "--pick-file", "alpha", "--pick-row", "4", "--pick-col", "4",
        "--verb", "recolor-line ff0000",
        "--cam-pose", "1", "2", "3", "30", "-10",
        "--pick-px", "100", "200",
        "--verb", "tint-cycle",
        "--pick-px", "300", "400",
        "--pick-file", "beta",
        "--verb", "hide-group",
    ]);
    assert_eq!(cli.ops.len(), 8);
    match &cli.ops[0] {
        Op::Pick(PickCommand::RowCol { file, row, col }) => {
            assert_eq!(file, "alpha");
            assert_eq!(*row, 4);
            assert_eq!(*col, 4);
        }
        _ => panic!("op[0] should be the upgraded RowCol pick"),
    }
    assert!(matches!(cli.ops[1], Op::Verb(Verb::RecolorLine([255, 0, 0]))));
    match &cli.ops[2] {
        Op::CamPose(p, yaw, pitch) => {
            assert_eq!(*p, [1.0, 2.0, 3.0]);
            assert!((yaw - 30f32.to_radians()).abs() < 1e-6);
            assert!((pitch - (-10f32).to_radians()).abs() < 1e-6);
        }
        _ => panic!("op[2] should be CamPose"),
    }
    assert!(matches!(
        cli.ops[3],
        Op::Pick(PickCommand::Pixel { x: 100.0, y: 200.0 })
    ));
    assert!(matches!(cli.ops[4], Op::Verb(Verb::TintCycle)));
    assert!(matches!(
        cli.ops[5],
        Op::Pick(PickCommand::Pixel { x: 300.0, y: 400.0 })
    ));
    assert!(matches!(&cli.ops[6], Op::Pick(PickCommand::File(f)) if f == "beta"));
    assert!(matches!(&cli.ops[7], Op::Verb(Verb::SetHidden(true))));
}

#[test]
fn pick_file_beta_is_in_stream() {
    let cli = parse(&["--pick-px", "1", "2", "--pick-file", "beta", "--pick-row", "7"]);
    assert_eq!(cli.ops.len(), 2);
    assert!(matches!(
        cli.ops[0],
        Op::Pick(PickCommand::Pixel { x: 1.0, y: 2.0 })
    ));
    match &cli.ops[1] {
        Op::Pick(PickCommand::RowCol { file, row, col }) => {
            assert_eq!(file, "beta");
            assert_eq!(*row, 7);
            assert_eq!(*col, 0);
        }
        _ => panic!("op[1] should be RowCol beta 7 0"),
    }
}

#[test]
fn pick_row_col_upgrade_semantics() {
    // Repeated upgrades mutate the same pick (row/col defaults 0).
    let cli = parse(&["--pick-file", "a", "--pick-row", "1", "--pick-row", "2"]);
    assert_eq!(cli.ops.len(), 1);
    match &cli.ops[0] {
        Op::Pick(PickCommand::RowCol { file, row, col }) => {
            assert_eq!(file, "a");
            assert_eq!(*row, 2);
            assert_eq!(*col, 0);
        }
        _ => panic!("expected RowCol"),
    }
    let cli = parse(&["--pick-file", "a", "--pick-col", "5", "--pick-row", "3"]);
    match &cli.ops[0] {
        Op::Pick(PickCommand::RowCol { row, col, .. }) => {
            assert_eq!(*row, 3);
            assert_eq!(*col, 5);
        }
        _ => panic!("expected RowCol"),
    }
    // No row/col: plain File pick survives.
    let cli = parse(&["--pick-file", "a"]);
    assert!(matches!(&cli.ops[0], Op::Pick(PickCommand::File(f)) if f == "a"));
}

#[test]
#[should_panic(expected = "--pick-row/--pick-col must follow --pick-file")]
fn pick_row_without_pick_file_panics() {
    parse(&["--pick-row", "1"]);
}

#[test]
#[should_panic(expected = "--pick-row/--pick-col must follow --pick-file")]
fn pick_row_after_verb_panics() {
    parse(&["--pick-file", "a", "--verb", "tint-cycle", "--pick-row", "1"]);
}

#[test]
fn unknown_flag_errors() {
    let err = try_parse(&["--bogus"]).err().expect("--bogus must fail");
    assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
}

#[test]
fn help_is_display_help() {
    // -h/--help surface as DisplayHelp via try_get_matches_from; the real
    // binary exits 0 through clap's default error exit path.
    let err = Cli::command()
        .try_get_matches_from(["glyph3d-native", "--help"])
        .unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
}

#[test]
fn repo_engine_rejects_bad_value() {
    assert!(try_parse(&["--repo-engine", "turbo"]).is_err());
}

#[test]
fn verb_error_messages_survive() {
    let err = parse_verb("frobnicate 1 2").err().expect("bad verb must fail");
    assert!(err.contains("unknown/malformed --verb"), "{err}");
    let err = parse_verb("nudge-glyph 1").err().expect("bad verb must fail");
    assert!(err.contains("bad/missing float at position 2"), "{err}");
    let err = parse_verb("tint-group zzzz").err().expect("bad verb must fail");
    assert!(err.contains("bad hex color"), "{err}");
    let err = try_parse(&["--verb", "frobnicate"]).err().expect("bad verb must fail");
    assert!(err.to_string().contains("unknown/malformed --verb"), "{err}");
}

#[test]
fn verb_defaults_and_forms() {
    assert!(matches!(
        parse_verb("recolor-glyph").unwrap(),
        Verb::RecolorGlyph([255, 80, 80])
    ));
    assert!(matches!(
        parse_verb("recolor-line").unwrap(),
        Verb::RecolorLine([255, 213, 79])
    ));
    assert!(matches!(
        parse_verb("nudge-glyph 1 2").unwrap(),
        Verb::NudgeGlyph(v) if v == [1.0, 2.0, 0.0]
    ));
    assert!(matches!(
        parse_verb("move-group 1 2 3").unwrap(),
        Verb::MoveGroup(v) if v == [1.0, 2.0, 3.0]
    ));
    // Every channel is asserted, and the literal is chosen so that no
    // channel can be right by accident. `ff0080` could not do this job:
    // 0xFF survives `& -> |`, 0x00 survives `>> -> <<`, and blue was never
    // checked at all — seven distinct mutations of the hex decoder hid
    // behind one badly-chosen colour, all of them invisible to the full
    // twelve-gate battery. 0x12/0x34/0x56 are distinct, and none is 0 or
    // 255, so a wrong mask, a wrong shift or a wrong divisor all move a
    // value this test reads.
    let Verb::TintGroup(v) = parse_verb("tint-group 123456").unwrap() else {
        panic!("tint-group did not parse to TintGroup");
    };
    assert!((v[0] - 18.0 / 255.0).abs() < 1e-6, "red channel: {v:?}");
    assert!((v[1] - 52.0 / 255.0).abs() < 1e-6, "green channel: {v:?}");
    assert!((v[2] - 86.0 / 255.0).abs() < 1e-6, "blue channel: {v:?}");

    // The explicit-colour form, which is what makes the `t.len() > 1`
    // arity checks falsifiable: with only the bare verb tested, `>` and `<`
    // both fall through to the default and the check is unobservable.
    assert!(matches!(
        parse_verb("recolor-glyph aabbcc").unwrap(),
        Verb::RecolorGlyph([0xAA, 0xBB, 0xCC])
    ));
    assert!(matches!(
        parse_verb("recolor-line 0a141e").unwrap(),
        Verb::RecolorLine([0x0A, 0x14, 0x1E])
    ));
    assert!(matches!(parse_verb("show-group").unwrap(), Verb::SetHidden(false)));
    assert!(matches!(parse_verb("toggle-hidden").unwrap(), Verb::ToggleHidden));
    assert!(matches!(
        parse_verb("scale-glyph 2.5").unwrap(),
        Verb::ScaleGlyph(s) if s == 2.5
    ));
    assert!(matches!(
        parse_verb("scale-group 0.5").unwrap(),
        Verb::ScaleGroup(s) if s == 0.5
    ));
}

#[test]
fn screenshot_frame_flags_parse() {
    // Stage K (K6): the windowed capture pair parses together.
    let cli = parse(&["--screenshot-frame", "90", "--screenshot-out", "/tmp/shot.png"]);
    assert_eq!(cli.screenshot_frame, Some(90));
    assert_eq!(cli.screenshot_out, Some(PathBuf::from("/tmp/shot.png")));
    // They require each other.
    assert!(try_parse(&["--screenshot-frame", "90"]).is_err());
    assert!(try_parse(&["--screenshot-out", "/tmp/shot.png"]).is_err());
    // Windowed-only: clap rejects combining them with offscreen --screenshot.
    assert!(try_parse(&[
        "--screenshot", "out.png", "--screenshot-frame", "90", "--screenshot-out", "/tmp/s.png",
    ])
    .is_err());
}

#[test]
fn generate_flag_parses() {
    let cli = parse(&["--generate", "bash"]);
    assert_eq!(cli.generate, Some(clap_complete::Shell::Bash));
    assert!(try_parse(&["--generate", "tcsh"]).is_err());
}

#[test]
fn negative_numbers_in_cam_pose_and_pick_px() {
    // The old parser took the next raw tokens unconditionally; clap needs
    // allow_negative_numbers to match that for oblique camera repros.
    let cli = parse(&["--cam-pose", "-1", "-2", "-3", "-180", "-45"]);
    match &cli.ops[0] {
        Op::CamPose(p, yaw, pitch) => {
            assert_eq!(*p, [-1.0, -2.0, -3.0]);
            assert!((yaw - (-180f32).to_radians()).abs() < 1e-6);
            assert!((pitch - (-45f32).to_radians()).abs() < 1e-6);
        }
        _ => panic!("expected CamPose"),
    }
}

#[test]
fn launch_config_file_merging() {
    let tmp = std::env::temp_dir().join(format!("test_launch_cfg_{}.toml", std::process::id()));
    std::fs::write(
        &tmp,
        r#"
        file_backgrounds = true
        file_bg_color = [0.2, 0.3, 0.4, 0.9]
        lod_min_px = 3.5
        wrap_mode = "down"
        greeking = false
        "#,
    )
    .expect("write temp config");

    let cli = parse(&[
        "--launch-config",
        tmp.to_str().unwrap(),
        "--wrap-mode",
        "back",
    ]);
    let _ = std::fs::remove_file(&tmp);

    assert!(cli.file_backgrounds);
    assert_eq!(cli.file_bg_color, Some([0.2, 0.3, 0.4, 0.9]));
    assert_eq!(cli.lod_min_px, Some(3.5));
    assert!(cli.no_greeking);
    assert_eq!(cli.wrap_mode, "back"); // CLI flag overrode TOML config
}

#[test]
fn launch_config_greek_pure_merging() {
    let tmp = std::env::temp_dir().join(format!("test_launch_cfg_pure_{}.toml", std::process::id()));
    std::fs::write(
        &tmp,
        r#"
        greeking = true
        greek_pure = true
        greek_onset_px = 15.0
        "#,
    )
    .expect("write temp config");

    let cli = parse(&[
        "--launch-config",
        tmp.to_str().unwrap(),
    ]);
    let _ = std::fs::remove_file(&tmp);

    assert!(!cli.no_greeking);
    assert!(cli.greek_pure);
    assert_eq!(cli.greek_onset_px, Some(15.0));
}

