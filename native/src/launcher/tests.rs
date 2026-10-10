use std::path::{Path, PathBuf};

use clap::{CommandFactory, ValueEnum};

use super::model::{Control, Launcher, Target, View};
use crate::cli::{parse_cli_from, Cli, DebugTint, PresentMode};
use crate::fold::{ClusterMode, WrapMode};
use crate::launch_config::{value_name, LaunchConfig};
use crate::repo::{ColorMode, RepoLayoutMode, Strategy};
use glyph_field::GlyphFieldMode;

fn names<T: ValueEnum>() -> Vec<String> {
    T::value_variants().iter().map(value_name).collect()
}

fn choice_names(l: &Launcher, c: Control) -> Vec<String> {
    match l.view(c) {
        View::Choice(opts) => opts.into_iter().map(|o| o.name).collect(),
        other => panic!("{c:?} is not a choice: {other:?}"),
    }
}

/// Every value every option enum has is offered, by the name the flag takes
/// — the lists ARE the enums. (The old TUI kept its own copies; `naive`
/// and the debug tint never reached it.)
#[test]
fn every_value_enum_is_offered_whole() {
    let l = Launcher::defaults();
    assert_eq!(choice_names(&l, Control::LayoutMode), names::<RepoLayoutMode>());
    assert_eq!(choice_names(&l, Control::WrapMode), names::<WrapMode>());
    assert_eq!(choice_names(&l, Control::RepoEngine), names::<Strategy>());
    assert_eq!(choice_names(&l, Control::ColorMode), names::<ColorMode>());
    assert_eq!(choice_names(&l, Control::ClusterMode), names::<ClusterMode>());
    assert_eq!(choice_names(&l, Control::FieldMode), names::<GlyphFieldMode>());
    assert_eq!(choice_names(&l, Control::DebugTint), names::<DebugTint>());
    assert_eq!(choice_names(&l, Control::PresentMode), names::<PresentMode>());
    assert!(choice_names(&l, Control::FieldMode).contains(&"visible".to_string()));
}

/// Untouched, a control shows what the renderer would run with no
/// arguments.
#[test]
fn untouched_controls_show_the_cli_defaults() {
    let l = Launcher::defaults();
    let selected = |c| match l.view(c) {
        View::Choice(opts) => opts.into_iter().find(|o| o.selected).unwrap().name,
        _ => unreachable!(),
    };
    assert_eq!(selected(Control::FieldMode), "instanced");
    assert_eq!(selected(Control::WrapMode), "back");
    assert_eq!(selected(Control::ColorMode), "flat");
    assert_eq!(l.view(Control::Greeking), View::Toggle(true));
    assert_eq!(l.view(Control::Cull), View::Toggle(true));
    // The LOD dials show the config's [lod] defaults until moved.
    match l.view(Control::TextDetail) {
        View::Dial { value: None, default, .. } => assert_eq!(default, 10.0),
        other => panic!("{other:?}"),
    }
}

#[test]
fn focus_skips_controls_the_target_does_not_use() {
    let mut l = Launcher::defaults();
    l.target = Target::File;
    l.focus = Control::Target;
    l.focus_next(1);
    assert_eq!(l.focus, Control::FilePath);
    l.focus_next(1);
    assert_eq!(l.focus, Control::WrapMode, "layout mode is repo-only");
    l.target = Target::Demo;
    l.focus = Control::Target;
    l.focus_next(1);
    assert_eq!(l.focus, Control::FieldMode, "a demo has no text layout");
    l.focus_next(-1);
    assert_eq!(l.focus, Control::Target);
    // A repo's controls are all reached, in order, and then focus wraps.
    l.target = Target::Repo;
    let repo: Vec<Control> = Control::ALL.iter().copied().filter(|c| l.is_applicable(*c)).collect();
    assert!(!repo.contains(&Control::FilePath) && !repo.contains(&Control::SessionPath));
    let mut seen = vec![l.focus];
    for _ in 0..repo.len() {
        l.focus_next(1);
        seen.push(l.focus);
    }
    assert_eq!(seen[..repo.len()], repo[..]);
    assert_eq!(seen[repo.len()], Control::Target, "wraps");
}

#[test]
fn choices_cycle_both_ways_and_dials_clamp() {
    let mut l = Launcher::defaults();
    l.focus = Control::FieldMode;
    l.step(1);
    assert_eq!(l.cfg.field_mode, Some(GlyphFieldMode::Derived));
    l.step(1);
    l.step(1);
    assert_eq!(l.cfg.field_mode, Some(GlyphFieldMode::Instanced), "wraps");
    l.step(-1);
    assert_eq!(l.cfg.field_mode, Some(GlyphFieldMode::Visible));
    l.toggle();
    assert_eq!(l.cfg.field_mode, Some(GlyphFieldMode::Instanced), "space steps a choice forward");

    l.focus = Control::ZWrapSpacing;
    l.step(1);
    assert_eq!(l.cfg.z_wrap_spacing, Some(0.2));
    for _ in 0..100 {
        l.step(1);
    }
    assert_eq!(l.cfg.z_wrap_spacing, Some(2.0));
    for _ in 0..100 {
        l.step(-1);
    }
    assert_eq!(l.cfg.z_wrap_spacing, Some(0.0));

    l.focus = Control::TextDetail;
    l.step(1);
    assert_eq!(l.cfg.text_detail_px, Some(11.0), "a dial starts from the default it shows");

    l.focus = Control::Cull;
    l.toggle();
    assert_eq!(l.cfg.no_cull, Some(true));
    l.step(1);
    assert_eq!(l.cfg.no_cull, Some(false), "◄/► flips a toggle too");
}

#[test]
fn repo_presets_cycle_from_the_config_or_the_built_ins() {
    let mut l = Launcher::defaults();
    l.focus = Control::RepoPath;
    l.step(1);
    assert_eq!(l.repo_path, "native/fixtures/g-pick-repo");
    l.step(1);
    assert_eq!(l.repo_path, ".");
    l.adopt(LaunchConfig::from_toml_str("repo_presets = [\"a\", \"b\", \"c\"]\n").unwrap());
    l.repo_path = "elsewhere".into();
    l.step(-1);
    assert_eq!(l.repo_path, "c", "an unlisted path steps back to the last preset");
}

/// A config's scene keys become the target and its path; the launch puts
/// back only the chosen one.
#[test]
fn scene_keys_become_the_target_and_only_it_is_written() {
    let mut l = Launcher::defaults();
    l.adopt(
        LaunchConfig::from_toml_str("load_repo = \"/r\"\nrender_file = \"/f.rs\"\nfocus_file = \"alpha\"\n").unwrap(),
    );
    assert_eq!(l.target, Target::Repo, "repo outranks file, as in Cli::action");
    assert_eq!(l.file_path, "/f.rs");
    let repo = l.launch_config(Path::new("/cwd"));
    assert_eq!(repo.load_repo, Some(PathBuf::from("/r")));
    assert_eq!((repo.render_file, repo.focus_file.as_deref()), (None, Some("alpha")));

    l.target = Target::File;
    let file = l.launch_config(Path::new("/cwd"));
    assert_eq!((file.load_repo, file.render_file, file.focus_file), (None, Some(PathBuf::from("/f.rs")), None));

    l.target = Target::Repo;
    l.repo_path = "native/fixtures/g-pick-repo".into();
    let rel = l.launch_config(Path::new("/cwd"));
    assert_eq!(rel.load_repo, Some(PathBuf::from("/cwd/native/fixtures/g-pick-repo")), "made absolute");

    l.adopt(LaunchConfig::from_toml_str("demo = true\nagent_session = \"/s.jsonl\"\n").unwrap());
    assert_eq!(l.target, Target::Demo);
    assert_eq!(l.session_path, "/s.jsonl");
}

/// The launch carries the whole loaded file, not just the keys the
/// launcher shows: `[section]` overrides, session dirs, presets. The child
/// reads only the launch file, so anything dropped here would silently stop
/// applying.
#[test]
fn the_launch_keeps_what_the_launcher_does_not_show() {
    let mut l = Launcher::defaults();
    l.adopt(
        LaunchConfig::from_toml_str(
            "kimi_sessions_dir = \"\"\nfile_bg_color = [0.1, 0.2, 0.3, 0.4]\ngreek_smooth = true\n\
             [glyph_scene]\nclear_color = [1.0, 0.0, 0.0, 1.0]\n",
        )
        .unwrap(),
    );
    let text = l.launch_config(Path::new("/cwd")).to_toml_string();
    let back = LaunchConfig::from_toml_str(&text).unwrap();
    assert_eq!(back.kimi_sessions_dir, Some(PathBuf::new()));
    assert_eq!(back.file_bg_color, Some([0.1, 0.2, 0.3, 0.4]));
    assert_eq!(back.greek_smooth, Some(true));
    assert!(back.settings.contains_key("glyph_scene"), "{text}");
}

/// A launch saves what was changed into the user's file, and a repo
/// launched from outside the presets joins them.
#[test]
fn a_launch_saves_changes_and_remembers_the_repo() {
    let dir = std::env::temp_dir().join(format!("test_launcher_save_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("launch_config.toml");
    std::fs::write(&path, "# mine\nwrap_mode = \"back\"\nagent_session = \"~/s.jsonl\"\n").unwrap();

    let mut l = Launcher::open(Some(path.clone()));
    assert_eq!(l.target, Target::Agent);
    assert!(!l.save().unwrap(), "nothing changed: the file is not rewritten (~ kept as typed)");

    l.target = Target::Repo;
    l.repo_path = "~/src/linux".into();
    l.cfg.field_mode = Some(GlyphFieldMode::Visible);
    l.remember_repo();
    assert!(l.save().unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    let back = LaunchConfig::from_toml_str(&text).unwrap();
    assert!(text.starts_with("# mine\nwrap_mode = \"back\"\n"), "{text}");
    assert_eq!(back.field_mode, Some(GlyphFieldMode::Visible));
    assert_eq!(back.load_repo, Some(PathBuf::from("~/src/linux")), "saved as typed, not absolute");
    assert_eq!(back.agent_session, None, "the scene is the one launched");
    assert_eq!(
        back.repo_presets,
        Some(vec![".".into(), "native/fixtures/g-pick-repo".into(), "~/src/linux".into()]),
    );

    // Reopened, the launcher is where it was left.
    let again = Launcher::open(Some(path.clone()));
    assert_eq!((again.target, again.repo_path.as_str()), (Target::Repo, "~/src/linux"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remembered_repos_are_capped_oldest_first_keeping_the_built_ins() {
    let mut l = Launcher::defaults();
    l.target = Target::Repo;
    for i in 0..20 {
        l.repo_path = format!("/r{i}");
        l.remember_repo();
    }
    let presets = l.cfg.repo_presets.clone().unwrap();
    assert_eq!(presets.len(), 16);
    assert_eq!(&presets[..2], &[".".to_string(), "native/fixtures/g-pick-repo".to_string()]);
    assert_eq!(presets[2], "/r6");
    assert_eq!(presets[15], "/r19");
    l.repo_path = "/r19".into();
    l.remember_repo();
    assert_eq!(l.cfg.repo_presets.unwrap().len(), 16, "a known repo is not added twice");
}

/// A config that does not load is shown, never swallowed (the old TUI
/// ignored a file that failed to parse and launched without it).
#[test]
fn a_bad_config_is_an_error_not_a_default() {
    let tmp = std::env::temp_dir().join(format!("test_launcher_bad_{}.toml", std::process::id()));
    std::fs::write(&tmp, "field_mode = \"visable\"\n").unwrap();
    let l = Launcher::open(Some(tmp.clone()));
    let _ = std::fs::remove_file(&tmp);
    let err = l.config_error.as_deref().expect("the error is kept");
    assert!(err.contains("visable"), "{err}");
    assert_eq!(l.config_source, Some(tmp));
}

/// What Enter starts is what the flags would: the written file, parsed by
/// the renderer's own CLI, builds the same plan as the equivalent command
/// line.
#[test]
fn the_written_file_builds_the_plan_the_flags_build() {
    let mut l = Launcher::defaults();
    l.target = Target::Repo;
    l.repo_path = "/repo".into();
    l.cfg.focus_file = Some("alpha".into());
    l.cfg.field_mode = Some(GlyphFieldMode::Visible);
    l.cfg.wrap_mode = Some(WrapMode::Down);
    l.cfg.layout_mode = Some(RepoLayoutMode::Carrel);
    l.cfg.repo_engine = Some(Strategy::Direct);
    l.cfg.debug_tint = Some(DebugTint::Cull);
    l.cfg.present_mode = Some(PresentMode::Mailbox);
    l.cfg.text_detail_px = Some(12.0);
    l.cfg.no_cull = Some(true);
    l.cfg.greeking = Some(false);

    let tmp = std::env::temp_dir().join(format!("test_launcher_plan_{}.toml", std::process::id()));
    std::fs::write(&tmp, l.launch_config(Path::new("/cwd")).to_toml_string()).unwrap();
    let parse = |argv: &[&str]| {
        let cli: Cli = parse_cli_from(Cli::command().try_get_matches_from(argv).unwrap()).unwrap();
        format!("{:?}", cli.action())
    };
    let from_file = parse(&["glyph3d-native", "--launch-config", tmp.to_str().unwrap()]);
    let _ = std::fs::remove_file(&tmp);
    let from_flags = parse(&[
        "glyph3d-native",
        "--load-repo", "/repo",
        "--focus-file", "alpha",
        "--field-mode", "visible",
        "--wrap-mode", "down",
        "--layout-mode", "carrel",
        "--repo-engine", "direct",
        "--debug-tint", "cull",
        "--present-mode", "mailbox",
        "--text-detail-px", "12",
        "--no-cull",
        "--no-greeking",
    ]);
    assert_eq!(from_file, from_flags);
}
