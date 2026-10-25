//! The windowed command bus (2026-10-24; `out/INTERFACE-SHELL-2026-10-24.md`
//! states its laws): every intent the window acts on — a panel button, a
//! hotkey, a self-test step — is a command LINE, and the line is what runs.
//! `parse_line` turns it into `Command`s; `exec::drain` runs them between
//! frames. A line that starts with `--` is parsed by the launch CLI's own
//! clap definition (`Cli::command()`, `Cli::from_arg_matches`,
//! `cli::ops::try_build_ops`), and every short form here is sugar that
//! becomes those same argv words, so a line typed in the console, the same
//! line fired by a panel and the same flags on the command line are one
//! parse by construction. There are no hand-written inverse formatters for
//! verbs or flags: the few lines the window writes itself (`lines`) are
//! plain strings, and the tests parse every one.
//!
//! What has no CLI flag gets an app word (`fly`, `pick group N`, `panel`,
//! `shot`, ...), listed in `APP_WORDS`, which `help` prints. `key <name>`
//! is a stopgap: a scene key the panels press for the carrel and grab
//! buttons, until those keys have verbs in `parse_verb`.
//!
//! Ungated: the hotkeys push lines in the no-UI build too, so the parser is
//! live there; only what needs the egui overlay (the file rows a fly reads,
//! the panel-only lines) is behind `egui-ui`.

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use winit::keyboard::KeyCode;

use crate::cli::Cli;
use crate::glyph_scene::PickCommand;

use super::app::{CarrelPatch, RelayoutRequest};
use super::LiveStep;

/// One thing the executor does. `Op` is the CLI op stream's own enum, so a
/// pick, verb, pose or highlight line reaches the scene through
/// `cli::ops::apply_op`, as the same flags do at launch.
#[derive(Debug)]
pub(super) enum Command {
    Op(crate::Op),
    /// A rebuild of the scene with new layout values (`--field-mode`,
    /// `--layout-mode`, `--agent-session`, ...): `app::apply_relayout`.
    Relayout(RelayoutRequest),
    /// Frame a file from the front, from its LIVE pose; lowered to an
    /// `Op::CamPose` by the executor (`fly_pose`).
    Fly(FlyTarget),
    /// The picks argv cannot spell: `PickCommand::Group` and `Clear`.
    Pick(PickCommand),
    App(AppCommand),
    /// A scene key press (`key <name>`): the stopgap for the carrel and grab
    /// buttons, whose keys are not verbs yet.
    SceneKey(KeyCode),
}

/// What `fly` frames: the exact rel path first, else the first path that
/// contains the text; or a group row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FlyTarget {
    File(String),
    Group(u32),
}

/// The window's own words: nothing the scene or the CLI knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AppCommand {
    /// Save the next presented frame (F2).
    Shot,
    /// Print the camera as its `--cam-pose` line and copy it.
    CopyPose,
    /// Step a live (`fieldzed`) source (F3/F4/F5).
    Live(LiveStep),
    /// Toggle a window (F1, F7, F8).
    Panel(PanelId),
    /// Scan the agent-session directories again.
    SessionsRefresh,
    /// The command list (`APP_WORDS` and the verb heads).
    Help,
    /// The launch CLI's own `--help`.
    CliHelp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PanelId {
    Debug,
    Hud,
    Sessions,
}

/// A line and what it lowered to. The line is kept for the log; the
/// commands run in order.
#[derive(Debug)]
pub(super) struct Parsed {
    pub(super) line: String,
    pub(super) commands: Vec<Command>,
}

/// The op-stream flags: they become `Command::Op`s, in CLI order.
const OP_FLAGS: &[&str] = &["pick_file", "pick_row", "pick_col", "pick_px", "cam_pose", "verb", "highlight"];
/// The flags a live scene can take by rebuilding (`RelayoutRequest`). Every
/// other typed flag is a launch flag and the line is refused.
const RELAYOUT_FLAGS: &[&str] = &[
    "field_mode",
    "repo_engine",
    "layout_mode",
    "cluster_mode",
    "z_wrap_spacing",
    "agent_session",
    "deck_window_limit",
    "deck_scroll_offset",
    "desk_revision_limit",
    "max_file_stacks",
];

/// `key <name>`: the scene keys the panels press, by name.
const KEY_NAMES: &[(&str, KeyCode)] = &[
    ("g", KeyCode::KeyG),
    ("c", KeyCode::KeyC),
    ("v", KeyCode::KeyV),
    ("end", KeyCode::End),
    ("home", KeyCode::Home),
    ("bracketleft", KeyCode::BracketLeft),
    ("bracketright", KeyCode::BracketRight),
];

/// Every app word and short form, with what it does: `help` prints this
/// table, and the palette lists the rows that fire as they stand.
pub(super) const APP_WORDS: &[(&str, &str)] = &[
    ("shot", "save the next frame to out/windowed-shot-<stamp>.png (F2)"),
    ("copy-pose", "print the camera as its --cam-pose line and copy it"),
    ("panel debug", "show or hide the Debug window (F1)"),
    ("panel hud", "show or hide the field HUD (F8)"),
    ("panel sessions", "show or hide the Agent Sessions browser (F7)"),
    ("sessions refresh", "scan the agent-session directories again"),
    ("live next", "step a live source forward (F3)"),
    ("live prev", "step a live source back (F4)"),
    ("live reset", "return a live source to its first step (F5)"),
    ("pick clear", "clear the selection"),
    ("pick group N", "select the file drawn with group row N"),
    ("pick FILE[:ROW[:COL]]", "--pick-file FILE [--pick-row ROW [--pick-col COL]]"),
    ("pick px X Y", "--pick-px X Y"),
    ("pose X Y Z YAW PITCH", "--cam-pose X Y Z YAW PITCH (degrees)"),
    ("fly FILE", "frame a file from the front: the exact path, else the first path containing FILE"),
    ("fly group N", "frame the file drawn with group row N"),
    ("set FLAG VALUE", "--FLAG VALUE"),
    ("key NAME", "press a scene key: g, c, v, end, home, bracketleft, bracketright"),
    ("VERB [ARGS]", "--verb \"VERB [ARGS]\" on the current pick"),
    ("--FLAG ...", "any op or relayout flag, parsed by the CLI itself (--help lists them)"),
    ("# ...", "a comment: logged, runs nothing"),
    ("help", "this list"),
];

/// The `help` reply.
pub(super) fn help_text() -> String {
    let width = APP_WORDS.iter().map(|(line, _)| line.len()).max().unwrap_or(0);
    let mut text = String::from("commands (the console, the palette and every panel fire these lines):\n");
    for (line, what) in APP_WORDS {
        text += &format!("  {line:width$}  {what}\n");
    }
    text += &format!("verbs: {}", crate::cli::VERB_HEADS.join(" "));
    text
}

/// Split a line into words: whitespace separates, `"..."` and `'...'`
/// quote, a backslash outside single quotes takes the next character as it
/// is. No shell: no variables, globs or escapes beyond that. `quote_arg` is
/// its inverse for one word.
pub(super) fn split_args(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => word.push(chars.next().ok_or("a backslash ends the line")?),
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == '\\' => {
                word.push(chars.next().ok_or("a backslash ends the line")?);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!("an unclosed {q} quote"));
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// One word as `split_args` reads it back: bare when it can be, else double
/// quoted with `"` and `\` escaped.
#[cfg(feature = "egui-ui")]
pub(super) fn quote_arg(word: &str) -> String {
    let bare = !word.is_empty() && !word.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '#'));
    if bare {
        word.to_string()
    } else {
        format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Parse one line into the commands it runs. An `Err` is the reason, for
/// the log; nothing here panics on any input.
pub(super) fn parse_line(line: &str) -> Result<Parsed, String> {
    let line = line.trim();
    if line.starts_with('#') {
        return Ok(Parsed { line: line.to_string(), commands: Vec::new() });
    }
    let words = split_args(line)?;
    let Some(head) = words.first() else {
        return Err("an empty line".to_string());
    };
    let rest = &words[1..];
    let none = |commands: Vec<Command>| {
        if rest.is_empty() {
            Ok(commands)
        } else {
            Err(format!("`{head}` takes no arguments"))
        }
    };
    let flags = |flag: &str, args: &[String]| {
        let argv: Vec<String> = std::iter::once(flag.to_string()).chain(args.iter().cloned()).collect();
        parse_flags(&argv)
    };
    let commands = match head.as_str() {
        h if h.starts_with('-') => parse_flags(&words)?,
        h if crate::cli::VERB_HEADS.contains(&h) => flags("--verb", &[words.join(" ")])?,
        "pick" => match rest {
            [w] if w == "clear" => vec![Command::Pick(PickCommand::Clear)],
            [w, n] if w == "group" => vec![Command::Pick(PickCommand::Group(parse_number(n, "pick group")?))],
            [w, x, y] if w == "px" => flags("--pick-px", &[x.clone(), y.clone()])?,
            [target] => {
                let mut parts = target.split(':');
                let file = parts.next().unwrap_or_default().to_string();
                let mut argv = vec!["--pick-file".to_string(), file];
                if let Some(row) = parts.next() {
                    argv.extend(["--pick-row".to_string(), row.to_string()]);
                }
                if let Some(col) = parts.next() {
                    argv.extend(["--pick-col".to_string(), col.to_string()]);
                }
                if parts.next().is_some() {
                    return Err(format!("`pick {target}`: FILE[:ROW[:COL]] has at most two colons"));
                }
                parse_flags(&argv)?
            }
            _ => return Err("`pick` takes FILE[:ROW[:COL]], `px X Y`, `group N` or `clear`".to_string()),
        },
        "pose" => flags("--cam-pose", rest)?,
        "set" => match rest {
            [flag, values @ ..] if !flag.starts_with('-') && !values.is_empty() => flags(&format!("--{flag}"), values)?,
            _ => return Err("`set` takes a flag name without its dashes, then its value".to_string()),
        },
        "fly" => match rest {
            [w, n] if w == "group" => vec![Command::Fly(FlyTarget::Group(parse_number(n, "fly group")?))],
            [file] => vec![Command::Fly(FlyTarget::File(file.clone()))],
            _ => return Err("`fly` takes a FILE (quote a path with spaces) or `group N`".to_string()),
        },
        "key" => match rest {
            [name] => match KEY_NAMES.iter().find(|(n, _)| n == name) {
                Some((_, code)) => vec![Command::SceneKey(*code)],
                None => return Err(format!("`key {name}`: not a scene key this bus knows (see `help`)")),
            },
            _ => return Err("`key` takes one key name".to_string()),
        },
        "panel" => match rest {
            [w] => vec![Command::App(AppCommand::Panel(match w.as_str() {
                "debug" => PanelId::Debug,
                "hud" => PanelId::Hud,
                "sessions" => PanelId::Sessions,
                other => return Err(format!("`panel {other}`: the panels are debug, hud and sessions")),
            }))],
            _ => return Err("`panel` takes one panel name".to_string()),
        },
        "live" => match rest {
            [w] => vec![Command::App(AppCommand::Live(match w.as_str() {
                "next" => LiveStep::Next,
                "prev" => LiveStep::Prev,
                "reset" => LiveStep::Reset,
                other => return Err(format!("`live {other}`: next, prev or reset")),
            }))],
            _ => return Err("`live` takes next, prev or reset".to_string()),
        },
        "sessions" => match rest {
            [w] if w == "refresh" => vec![Command::App(AppCommand::SessionsRefresh)],
            _ => return Err("`sessions` takes `refresh`".to_string()),
        },
        "shot" => none(vec![Command::App(AppCommand::Shot)])?,
        "copy-pose" => none(vec![Command::App(AppCommand::CopyPose)])?,
        "help" => none(vec![Command::App(AppCommand::Help)])?,
        other => return Err(format!("unknown command `{other}` (`help` lists them)")),
    };
    Ok(Parsed { line: line.to_string(), commands })
}

fn parse_number(word: &str, what: &str) -> Result<u32, String> {
    word.parse().map_err(|_| format!("`{what} {word}`: not a whole number"))
}

/// A flag line through the launch CLI's own clap definition. Typed op flags
/// become `Command::Op`s in CLI order; typed relayout flags one
/// `Command::Relayout` ahead of them (the rebuild lands first, then the ops
/// apply to the new scene, as at launch); any other typed flag refuses the
/// line. `ValueSource::CommandLine` is what "typed" means, the test the
/// launch config's merge uses.
fn parse_flags(argv: &[String]) -> Result<Vec<Command>, String> {
    let matches = match Cli::command().no_binary_name(true).try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) if matches!(e.kind(), clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion) => {
            return Ok(vec![Command::App(AppCommand::CliHelp)]);
        }
        Err(e) => return Err(e.render().to_string().trim_end().to_string()),
    };
    let cli = Cli::from_arg_matches(&matches).map_err(|e| e.render().to_string().trim_end().to_string())?;
    let typed = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
    // `ids()` names the derive's argument GROUPS too (`Cli`, `RawOps`),
    // typed whenever one of their flags is; only arguments are judged.
    let command = Cli::command();
    let is_flag = |id: &str| command.get_arguments().any(|a| a.get_id() == id);
    if let Some(id) = matches
        .ids()
        .map(clap::Id::as_str)
        .find(|id| is_flag(id) && typed(id) && !OP_FLAGS.contains(id) && !RELAYOUT_FLAGS.contains(id))
    {
        return Err(format!("--{} is a launch flag, not a live command", id.replace('_', "-")));
    }
    let mut request = RelayoutRequest::default();
    let mut relayout = false;
    let mut take = |id: &str| {
        let t = typed(id);
        relayout |= t;
        t
    };
    if take("field_mode") {
        request.set_field_mode = Some(cli.field_mode);
    }
    if take("repo_engine") {
        request.set_strategy = Some(cli.repo_engine);
    }
    if take("layout_mode") {
        request.layout_mode = Some(cli.layout_mode);
    }
    if take("cluster_mode") {
        request.cluster_mode = Some(cli.cluster_mode);
    }
    if take("z_wrap_spacing") {
        request.z_wrap_spacing = Some(cli.z_wrap_spacing);
    }
    if take("agent_session") {
        // As parse_cli_from does for the launch flag.
        request.switch_agent_session = cli.agent_session.as_deref().map(crate::launch_config::expand_home);
    }
    request.carrel = CarrelPatch {
        deck_window_limit: take("deck_window_limit").then_some(cli.deck_window_limit),
        deck_scroll_offset: take("deck_scroll_offset").then_some(cli.deck_scroll_offset),
        desk_revision_limit: take("desk_revision_limit").then_some(cli.desk_revision_limit),
        max_file_stacks: take("max_file_stacks").then_some(cli.max_file_stacks),
    };
    let mut commands = Vec::new();
    if relayout {
        commands.push(Command::Relayout(request));
    }
    commands.extend(crate::cli::try_build_ops(&matches, &cli.raw_ops)?.into_iter().map(Command::Op));
    if commands.is_empty() {
        return Err("no live flag on the line".to_string());
    }
    Ok(commands)
}

/// The scene's own carrel moves (`SceneLike::take_pending_carrel_options`):
/// a beat step that slid the window. Queued so the log shows the rebuild,
/// but as a `#` NOTE, not a line: the active beat and the desk scroll it
/// carries have no flag, so no line could say it.
pub(super) fn scene_carrel_note(options: crate::spatial_scene::CarrelLayoutOptions) -> Parsed {
    Parsed {
        line: format!(
            "# the scene moved the carrel window: deck-window-limit {}, deck-scroll-offset {}, desk-revision-limit {}, \
             desk-scroll-offset {}, max-file-stacks {}, active beat {:?}",
            options.deck_window_limit,
            options.deck_scroll_offset,
            options.desk_revision_limit,
            options.desk_scroll_offset,
            options.max_file_stacks,
            options.active_beat
        ),
        commands: vec![Command::Relayout(RelayoutRequest { carrel_options: Some(options), ..Default::default() })],
    }
}

/// The lines the window writes itself: hotkeys, panel buttons, self-tests.
/// Every one is parsed before it runs, and `every_line_the_window_writes_parses`
/// parses each one here.
pub(super) mod lines {
    pub(in crate::windowed) const SHOT: &str = "shot";
    pub(in crate::windowed) const PANEL_DEBUG: &str = "panel debug";
    pub(in crate::windowed) const PANEL_HUD: &str = "panel hud";
    pub(in crate::windowed) const PANEL_SESSIONS: &str = "panel sessions";
    pub(in crate::windowed) const LIVE_NEXT: &str = "live next";
    pub(in crate::windowed) const LIVE_PREV: &str = "live prev";
    pub(in crate::windowed) const LIVE_RESET: &str = "live reset";

    /// `--flag value`, the value spelled as clap spells it (`--layout-mode
    /// library`, `--repo-engine batch`).
    pub(in crate::windowed) fn flag<T: clap::ValueEnum>(flag: &str, value: T) -> String {
        let value = value.to_possible_value().expect("every live enum value has a CLI spelling");
        format!("--{flag} {}", value.get_name())
    }

    /// The z dial's line. `f64`'s Display is the shortest decimal that
    /// round-trips, so the value the slider holds is the value parsed.
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) fn z_wrap_spacing(spacing: f64) -> String {
        format!("--z-wrap-spacing {spacing}")
    }

    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const COPY_POSE: &str = "copy-pose";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const SESSIONS_REFRESH: &str = "sessions refresh";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const PICK_CLEAR: &str = "pick clear";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_GRAB_FILE: &str = "key g";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_GRAB_CARREL: &str = "key c";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_CARREL_OLDEST: &str = "key end";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_CARREL_PREV: &str = "key bracketleft";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_CARREL_NEXT: &str = "key bracketright";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_CARREL_LATEST: &str = "key home";
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) const KEY_CARREL_MODE: &str = "key v";

    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) fn pick_group(group: u32) -> String {
        format!("pick group {group}")
    }

    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) fn fly_group(group: u32) -> String {
        format!("fly group {group}")
    }

    /// The Carrel window's sliders, all four flags at once.
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) fn carrel(options: &crate::spatial_scene::CarrelLayoutOptions) -> String {
        format!(
            "--deck-window-limit {} --deck-scroll-offset {} --desk-revision-limit {} --max-file-stacks {}",
            options.deck_window_limit, options.deck_scroll_offset, options.desk_revision_limit, options.max_file_stacks
        )
    }

    /// Load a session: the path quoted, so spaces survive `split_args`.
    #[cfg(feature = "egui-ui")]
    pub(in crate::windowed) fn agent_session(path: &std::path::Path) -> String {
        format!("--agent-session {}", super::quote_arg(&path.to_string_lossy()))
    }
}

/// The row a fly frames: the exact rel path first, then the first path
/// containing the text (`--pick-file`'s substring rule would let `a.rs`
/// land on `ba.rs` when `a.rs` exists), or the first row of a group.
#[cfg(feature = "egui-ui")]
pub(super) fn fly_row(files: &[crate::glyph_scene::UiFileRow], target: &FlyTarget) -> Result<usize, String> {
    match target {
        FlyTarget::File(text) => files
            .iter()
            .position(|f| f.rel_path == *text)
            .or_else(|| files.iter().position(|f| f.rel_path.contains(text.as_str())))
            .ok_or_else(|| format!("fly: no file matches {text:?}")),
        FlyTarget::Group(group) => files
            .iter()
            .position(|f| f.group_id == *group)
            .ok_or_else(|| format!("fly: no file is drawn with group row {group}")),
    }
}

/// The front-on framing of one file's live world box: the camera faces −Z
/// at the box centre, at the distance that fits its half-height (or its
/// half-width over the viewport aspect, whichever needs more), the formula
/// the file browser's click used inline until 2026-10-24.
#[cfg(feature = "egui-ui")]
pub(super) fn fly_pose(row: &crate::glyph_scene::UiFileDyn, aspect: f32) -> crate::Op {
    let half_h_needed = row.half[1].max(row.half[0] / aspect);
    let dist = crate::glyph_scene::fit_distance(half_h_needed);
    crate::Op::CamPose([row.center[0], row.center[1], dist], 0.0, 0.0)
}

// Gated with the overlay: the tests parse the panel-only lines too.
#[cfg(all(test, feature = "egui-ui"))]
mod tests {
    use super::*;
    use std::path::Path;

    /// The op stream the launch CLI builds from these argv words.
    fn cli_ops(argv: &[&str]) -> String {
        let matches = Cli::command()
            .try_get_matches_from(std::iter::once("glyph3d-native").chain(argv.iter().copied()))
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let cli = crate::cli::parse_cli_from(matches).unwrap();
        format!("{:?}", cli.ops)
    }

    /// The ops a line lowers to, in the same Debug form.
    fn line_ops(line: &str) -> String {
        let parsed = parse_line(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        let ops: Vec<&crate::Op> = parsed
            .commands
            .iter()
            .map(|c| match c {
                Command::Op(op) => op,
                other => panic!("{line:?}: {other:?} is not an op"),
            })
            .collect();
        format!("{ops:?}")
    }

    #[test]
    fn console_lines_lower_to_the_cli_op_stream() {
        // Row and column differ in every pick, so a swap cannot pass.
        for (line, argv) in [
            ("pick alpha.rs:4:5", &["--pick-file", "alpha.rs", "--pick-row", "4", "--pick-col", "5"][..]),
            ("pick alpha.rs:4", &["--pick-file", "alpha.rs", "--pick-row", "4"][..]),
            ("pick alpha.rs", &["--pick-file", "alpha.rs"][..]),
            ("pick px 100 -20", &["--pick-px", "100", "-20"][..]),
            ("pose 1 2 3 30 -10", &["--cam-pose", "1", "2", "3", "30", "-10"][..]),
            ("tint-cycle", &["--verb", "tint-cycle"][..]),
            ("nudge-glyph 0.5 0 -1", &["--verb", "nudge-glyph 0.5 0 -1"][..]),
            ("recolor-line ff8000", &["--verb", "recolor-line ff8000"][..]),
            ("page-next", &["--verb", "page-next"][..]),
            ("set verb hide-group", &["--verb", "hide-group"][..]),
            (
                "--pick-file beta --pick-col 3 --pick-row 7 --verb tint-cycle --cam-pose 0 0 9 0 0",
                &["--pick-file", "beta", "--pick-col", "3", "--pick-row", "7", "--verb", "tint-cycle", "--cam-pose", "0", "0", "9", "0", "0"][..],
            ),
        ] {
            assert_eq!(line_ops(line), cli_ops(argv), "{line:?}");
        }
    }

    #[test]
    fn short_forms_and_flag_lines_agree() {
        for (short, long) in [
            ("pick alpha.rs:4:5", "--pick-file alpha.rs --pick-row 4 --pick-col 5"),
            ("pick px 10 20", "--pick-px 10 20"),
            ("pose 1 2 3 30 -10", "--cam-pose 1 2 3 30 -10"),
            ("tint-cycle", "--verb tint-cycle"),
            ("move-group 1 2 3", "--verb \"move-group 1 2 3\""),
            ("set verb 'scale-group 2'", "--verb 'scale-group 2'"),
        ] {
            assert_eq!(line_ops(short), line_ops(long), "{short:?} vs {long:?}");
        }
    }

    #[test]
    fn malformed_lines_are_refused_without_panicking() {
        for line in [
            "",
            "   ",
            "--pick-row 1",
            "--bogus",
            "--load-repo fixtures/g-pick-repo",
            "--screenshot /tmp/x.png",
            "--verb frobnicate",
            "frobnicate",
            "nudge-glyph 1",
            "pick",
            "pick a:b:c:d",
            "pick a.rs:x",
            "pick group x",
            "fly",
            "fly a b",
            "set",
            "set --verb tint-cycle",
            "set verb",
            "key q",
            "panel nope",
            "live sideways",
            "shot now",
            "--agent-session \"unclosed",
            "trailing \\",
            "--z-wrap-spacing -1",
            "--field-mode sideways",
        ] {
            assert!(parse_line(line).is_err(), "{line:?} should be refused");
        }
        let err = parse_line("--load-repo x").err().unwrap();
        assert!(err.contains("--load-repo is a launch flag"), "{err}");
        let err = parse_line("--pick-row 1").err().unwrap();
        assert!(err.contains("--pick-row/--pick-col must follow --pick-file"), "{err}");
        // --help is not an error: it renders the CLI's own help.
        assert!(matches!(&parse_line("--help").unwrap().commands[..], [Command::App(AppCommand::CliHelp)]));
        // A comment runs nothing and is not refused.
        assert!(parse_line("# a note with an \"unclosed quote").unwrap().commands.is_empty());
    }

    #[test]
    fn relayout_flags_become_absolute_requests() {
        let request = |line: &str| match &parse_line(line).unwrap_or_else(|e| panic!("{line:?}: {e}")).commands[..] {
            [Command::Relayout(r)] => format!("{r:?}"),
            other => panic!("{line:?}: {other:?}"),
        };
        let expect = |r: RelayoutRequest| format!("{r:?}");
        assert_eq!(
            request("--layout-mode library"),
            expect(RelayoutRequest { layout_mode: Some(crate::repo::RepoLayoutMode::Library), ..Default::default() })
        );
        assert_eq!(
            request("--cluster-mode leader"),
            expect(RelayoutRequest { cluster_mode: Some(crate::fold::ClusterMode::Leader), ..Default::default() })
        );
        assert_eq!(request("--z-wrap-spacing 0.3"), expect(RelayoutRequest { z_wrap_spacing: Some(0.3), ..Default::default() }));
        assert_eq!(
            request("--field-mode visible --repo-engine batch"),
            expect(RelayoutRequest {
                set_field_mode: Some(glyph_field::GlyphFieldMode::Visible),
                set_strategy: Some(crate::repo::Strategy::Batched),
                ..Default::default()
            })
        );
        // Only the typed carrel flags: the others keep the scene's values.
        assert_eq!(
            request("--deck-scroll-offset 4 --max-file-stacks 9"),
            expect(RelayoutRequest {
                carrel: CarrelPatch { deck_scroll_offset: Some(4), max_file_stacks: Some(9), ..Default::default() },
                ..Default::default()
            })
        );
        // A relayout lands before the ops on its line, as at launch.
        let parsed = parse_line("--verb tint-cycle --layout-mode carrel").unwrap();
        assert!(matches!(&parsed.commands[..], [Command::Relayout(_), Command::Op(_)]), "{:?}", parsed.commands);
    }

    #[test]
    fn session_paths_with_spaces_survive_the_line() {
        let path = Path::new("/Users/a b/My \"Sessions\"/it's\\here.jsonl");
        let parsed = parse_line(&lines::agent_session(path)).unwrap();
        let [Command::Relayout(r)] = &parsed.commands[..] else { panic!("{:?}", parsed.commands) };
        assert_eq!(r.switch_agent_session.as_deref(), Some(path));
        assert_eq!(split_args("a \"b c\" 'd e' f\\ g").unwrap(), ["a", "b c", "d e", "f g"]);
        for word in ["plain", "with space", "q\"uote", "back\\slash", "it's", "", "#hash", "tab\there"] {
            assert_eq!(split_args(&quote_arg(word)).unwrap(), [word], "{word:?}");
        }
    }

    #[test]
    fn every_line_the_window_writes_parses() {
        use crate::fold::ClusterMode;
        use crate::repo::{RepoLayoutMode, Strategy};
        use clap::ValueEnum;
        let mut written: Vec<String> = [
            lines::SHOT, lines::PANEL_DEBUG, lines::PANEL_HUD, lines::PANEL_SESSIONS, lines::LIVE_NEXT,
            lines::LIVE_PREV, lines::LIVE_RESET, lines::COPY_POSE, lines::SESSIONS_REFRESH, lines::PICK_CLEAR,
            lines::KEY_GRAB_FILE, lines::KEY_GRAB_CARREL, lines::KEY_CARREL_OLDEST, lines::KEY_CARREL_PREV,
            lines::KEY_CARREL_NEXT, lines::KEY_CARREL_LATEST, lines::KEY_CARREL_MODE,
        ]
        .map(String::from)
        .to_vec();
        written.extend(super::super::ui::PANEL_VERBS.iter().map(|v| v.to_string()));
        written.extend(glyph_field::GlyphFieldMode::value_variants().iter().map(|m| lines::flag("field-mode", *m)));
        written.extend(Strategy::value_variants().iter().map(|s| lines::flag("repo-engine", *s)));
        written.extend(RepoLayoutMode::value_variants().iter().map(|m| lines::flag("layout-mode", *m)));
        written.extend(ClusterMode::value_variants().iter().map(|m| lines::flag("cluster-mode", *m)));
        written.extend([0.0, 0.15, 0.3, 1.0, 0.123_456_789].map(lines::z_wrap_spacing));
        written.extend([lines::pick_group(3), lines::fly_group(3)]);
        written.push(lines::carrel(&crate::spatial_scene::CarrelLayoutOptions::default()));
        written.push(lines::agent_session(Path::new("/tmp/a session.jsonl")));
        for line in &written {
            let parsed = parse_line(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
            assert!(!parsed.commands.is_empty(), "{line:?} runs nothing");
        }
        // The z dial: the value parsed is the value the slider held.
        let parsed = parse_line(&lines::z_wrap_spacing(0.123_456_789)).unwrap();
        let [Command::Relayout(r)] = &parsed.commands[..] else { panic!() };
        assert_eq!(r.z_wrap_spacing.map(f64::to_bits), Some(0.123_456_789f64.to_bits()));
    }

    #[test]
    fn every_app_word_in_help_is_a_line_or_a_template() {
        // The rows that stand as they are fire; the rest are templates (an
        // argument placeholder in capitals, or `...`) and fill the console.
        for (line, _) in APP_WORDS {
            let template = line.chars().any(|c| c.is_ascii_uppercase()) || line.contains("...");
            if !template {
                assert!(parse_line(line).is_ok(), "{line:?}");
            }
        }
        let help = help_text();
        assert!(APP_WORDS.iter().all(|(line, _)| help.contains(line)));
        assert!(crate::cli::VERB_HEADS.iter().all(|v| help.contains(v)));
    }

    #[test]
    fn fly_targets_resolve_exact_path_first() {
        let row = |p: &str, g: u32| crate::glyph_scene::UiFileRow {
            rel_path: p.to_string(),
            group_id: g,
            aabb_min: [0.0; 2],
            aabb_max: [1.0; 2],
        };
        let files = [row("ab/a.rs", 7), row("b/a.rs", 3), row("a.rs", 5)];
        let fly = |t: FlyTarget| fly_row(&files, &t);
        assert_eq!(fly(FlyTarget::File("a.rs".into())), Ok(2), "the exact path, not the first containing it");
        assert_eq!(fly(FlyTarget::File("b/a".into())), Ok(0), "else the first path containing the text");
        assert_eq!(fly(FlyTarget::Group(3)), Ok(1));
        assert!(fly(FlyTarget::File("zz".into())).is_err());
        assert!(fly(FlyTarget::Group(9)).is_err());
    }

    #[test]
    fn fly_pose_is_the_browser_formula() {
        let row = crate::glyph_scene::UiFileDyn { center: [10.0, -4.0], half: [8.0, 2.0], ..Default::default() };
        // Wide file in a 2:1 viewport: the width needs 8 / 2 = 4 > 2.
        let crate::Op::CamPose(eye, yaw, pitch) = fly_pose(&row, 2.0) else { panic!() };
        assert_eq!(eye, [10.0, -4.0, crate::glyph_scene::fit_distance(4.0)]);
        assert_eq!((yaw, pitch), (0.0, 0.0));
        // Tall file: the height wins.
        let crate::Op::CamPose(eye, ..) = fly_pose(&row, 8.0) else { panic!() };
        assert_eq!(eye[2], crate::glyph_scene::fit_distance(2.0));
    }
}
