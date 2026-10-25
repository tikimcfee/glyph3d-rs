//! The command palette (Cmd-K on macOS, Ctrl-K elsewhere; 2026-10-24): a
//! fuzzy list of every line the window knows, where each row's subtitle IS
//! the line it fires — the palette teaches the console's grammar. A row is
//! `{title, line}` and nothing else. Enter fires the line if it parses as it
//! stands; a template (an argument still to give: `nudge-glyph`, `pick
//! FILE[:ROW[:COL]]`) fills the console instead, as Tab always does.
//!
//! The rows are the verb heads (`cli::VERB_HEADS`), the live settings each
//! as its flag line (`--field-mode visible`, from the `ValueEnum` variants,
//! as the launcher builds its option rows; only those the snapshot says
//! the scene has), `fly <path>` per file, `--agent-session "<path>"` per
//! scanned session, and the app words (`bus::APP_WORDS`, which `help`
//! prints). The CLI's spellings, not a namespaced vocabulary of its own:
//! the CLI is the existing oracle, and anything that later feeds lines in
//! (an IPC listener) goes through the same parser.

use clap::ValueEnum;

use crate::glyph_scene::UiProbeState;

use super::bus::{self, lines};
use super::ui::UiState;

/// How many ranked rows the palette shows.
const SHOWN: usize = 12;

/// One palette row: what it is, and the line it fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) title: String,
    pub(super) line: String,
}

impl Entry {
    fn new(title: impl Into<String>, line: impl Into<String>) -> Self {
        Self { title: title.into(), line: line.into() }
    }
}

/// Every row for the scene as the snapshot describes it.
pub(super) fn catalog(
    snap: Option<&UiProbeState>,
    sessions: Option<&[crate::agent_transcript::discovery::DiscoveredSession]>,
) -> Vec<Entry> {
    let mut rows: Vec<Entry> = crate::cli::VERB_HEADS
        .iter()
        .map(|head| Entry::new(format!("verb: {head}"), *head))
        .collect();
    if let Some(snap) = snap {
        fn settings<T: ValueEnum + PartialEq + Copy>(rows: &mut Vec<Entry>, what: &str, flag: &str, current: Option<T>) {
            let Some(current) = current else { return };
            for value in T::value_variants() {
                let line = lines::flag(flag, *value);
                let now = if *value == current { " (current)" } else { "" };
                let name = line.rsplit(' ').next().unwrap_or_default().to_string();
                rows.push(Entry::new(format!("{what}: {name}{now}"), line));
            }
        }
        settings(&mut rows, "field mode", "field-mode", snap.field_mode);
        settings(&mut rows, "layout engine", "repo-engine", snap.strategy);
        settings(&mut rows, "canvas layout", "layout-mode", snap.layout_mode);
        let cluster = snap.cluster_mode.map(|on| if on { crate::fold::ClusterMode::Cluster } else { crate::fold::ClusterMode::Leader });
        settings(&mut rows, "sequence pass", "cluster-mode", cluster);
        rows.extend(
            snap.files
                .iter()
                .map(|f| Entry::new(format!("fly to {}", f.rel_path), format!("fly {}", bus::quote_arg(&f.rel_path)))),
        );
    }
    for s in sessions.unwrap_or_default() {
        rows.push(Entry::new(format!("load session: {}", s.title), lines::agent_session(&s.path)));
    }
    rows.extend(bus::APP_WORDS.iter().map(|(line, what)| Entry::new(*what, *line)));
    rows
}

/// Fuzzy subsequence score of `query` in `target`, case-insensitive (M1's
/// palette, `worktree-interface` cf0f1d2, verbatim): a run of matches
/// scores 3 a character, a match at a word start 2, any other 1, less one
/// point per 16 characters of target; `None` when the query is not a
/// subsequence. Whitespace in the query is ignored.
pub(super) fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    // Both sides lowercased — "MAIN" must match "main" (the query side was
    // the miss; caught by the case-insensitivity test).
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = target.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut prev_match = false;
    for (ti, tc) in t.iter().enumerate() {
        if qi < q.len() && *tc == q[qi] {
            let word_start =
                ti == 0 || matches!(t[ti - 1], '/' | '-' | '_' | '.' | ' ');
            score += if prev_match {
                3
            } else if word_start {
                2
            } else {
                1
            };
            prev_match = true;
            qi += 1;
        } else {
            prev_match = false;
        }
    }
    (qi == q.len()).then_some(score - (target.len() as i32) / 16)
}

/// The rows that match `query`, best first: the better of the title's and
/// the line's score, ties to the shorter line, then the line itself, so the
/// order never depends on the catalog's.
pub(super) fn rank(rows: &[Entry], query: &str) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let score = fuzzy_score(query, &row.title).max(fuzzy_score(query, &row.line))?;
            Some((score, i))
        })
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then(rows[*a].line.len().cmp(&rows[*b].line.len()))
            .then(rows[*a].line.cmp(&rows[*b].line))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

/// What Tab (or Enter on a template) puts in the console: the line up to
/// its first placeholder word (one with a capital, or `...`), ready for
/// the arguments; a line with no placeholder, whole.
pub(super) fn fill(line: &str) -> String {
    let words: Vec<&str> = line.split(' ').collect();
    match words.iter().position(|w| w.chars().any(|c| c.is_ascii_uppercase()) || w.contains("...")) {
        Some(0) => String::new(),
        Some(n) => words[..n].join(" ") + " ",
        None if bus::parse_line(line).is_ok() => line.to_string(),
        // A verb head that wants arguments.
        None => format!("{line} "),
    }
}

/// The palette: Cmd/Ctrl-K toggles it (consumed before any text field is
/// shown, since egui's TextEdit binds Ctrl-K itself), the query takes the
/// keyboard, ↑/↓ move, Enter fires, Tab fills the console, Esc closes.
pub(super) fn show(ctx: &egui::Context, snap: Option<&UiProbeState>, shell: &mut UiState, panel_lines: &mut Vec<String>) {
    let chord = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
    if ctx.input_mut(|i| i.consume_shortcut(&chord)) {
        shell.palette_open = !shell.palette_open;
        shell.palette_query.clear();
        shell.palette_selected = 0;
    }
    if !shell.palette_open {
        return;
    }
    let rows = catalog(snap, shell.discovered_sessions.as_deref());
    let ranked = rank(&rows, &shell.palette_query);
    let (up, down, enter, tab, escape) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::Enter),
            i.key_pressed(egui::Key::Tab),
            i.key_pressed(egui::Key::Escape),
        )
    });
    let shown = ranked.len().min(SHOWN);
    if down && shown > 0 {
        shell.palette_selected = (shell.palette_selected + 1).min(shown - 1);
    }
    if up {
        shell.palette_selected = shell.palette_selected.saturating_sub(1);
    }
    shell.palette_selected = shell.palette_selected.min(shown.saturating_sub(1));
    let mut chosen: Option<(usize, bool)> = None; // (row, fill instead of fire)
    egui::Window::new("Command palette")
        .id(egui::Id::new("command_palette"))
        .title_bar(false)
        .anchor(egui::Align2::CENTER_TOP, [0.0, 48.0])
        .fixed_size(egui::vec2(560.0, 0.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let query = ui.add(
                egui::TextEdit::singleline(&mut shell.palette_query)
                    .desired_width(f32::INFINITY)
                    .hint_text("type to search; the grey line under each row is what it runs"),
            );
            query.request_focus();
            ui.separator();
            if ranked.is_empty() {
                ui.weak("nothing matches");
            }
            for (k, &i) in ranked.iter().take(SHOWN).enumerate() {
                let row = &rows[i];
                let selected = k == shell.palette_selected;
                let response = ui.selectable_label(selected, &row.title).on_hover_text(&row.line);
                ui.label(egui::RichText::new(&row.line).monospace().weak());
                if response.clicked() {
                    chosen = Some((i, false));
                }
            }
        });
    if escape {
        shell.palette_open = false;
        return;
    }
    if chosen.is_none() && (enter || tab) {
        chosen = ranked.get(shell.palette_selected).map(|&i| (i, tab));
    }
    let Some((i, fill_only)) = chosen else { return };
    let line = &rows[i].line;
    shell.palette_open = false;
    if !fill_only && bus::parse_line(line).is_ok_and(|p| !p.commands.is_empty()) {
        panel_lines.push(line.clone());
    } else {
        shell.console.input = fill(line);
        shell.console_open = true;
        shell.console.focus = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[(&str, &str)]) -> Vec<Entry> {
        lines.iter().map(|(t, l)| Entry::new(*t, *l)).collect()
    }

    #[test]
    fn word_starts_rank_first() {
        // "tc": the word starts of tint-cycle beat two letters mid-word.
        assert!(fuzzy_score("tc", "tint-cycle") > fuzzy_score("tc", "attach"));
        let r = rows(&[("", "attach"), ("", "tint-cycle")]);
        assert_eq!(rank(&r, "tc"), [1, 0]);
    }

    #[test]
    fn matching_is_case_insensitive_and_unmatched_rows_are_dropped() {
        assert!(fuzzy_score("MAIN", "native/src/main.rs").is_some());
        assert!(fuzzy_score("main", "MAIN.RS").is_some());
        assert_eq!(fuzzy_score("zz", "main.rs"), None);
        let r = rows(&[("verb: tint-cycle", "tint-cycle"), ("file", "fly main.rs")]);
        assert_eq!(rank(&r, "qqq"), Vec::<usize>::new());
        // The title counts as much as the line.
        assert_eq!(rank(&r, "verb"), [0]);
    }

    #[test]
    fn ties_break_by_shorter_line_then_by_the_line() {
        let r = rows(&[("x", "fly bb"), ("x", "fly a"), ("x", "fly aa"), ("x", "fly ab")]);
        // An empty query scores every row 0: shorter first, then lexical.
        assert_eq!(rank(&r, ""), [1, 2, 3, 0]);
        // Whatever order the catalog came in.
        let mut reversed = r.clone();
        reversed.reverse();
        let order: Vec<&str> = rank(&reversed, "").iter().map(|&i| reversed[i].line.as_str()).collect();
        assert_eq!(order, ["fly a", "fly aa", "fly ab", "fly bb"]);
    }

    #[test]
    fn every_palette_row_fires_its_subtitle_or_fills_the_console() {
        let snap = UiProbeState {
            field_mode: Some(glyph_field::GlyphFieldMode::Derived),
            strategy: Some(crate::repo::Strategy::Hyper),
            layout_mode: Some(crate::repo::RepoLayoutMode::Shelf),
            cluster_mode: Some(true),
            files: std::rc::Rc::new(vec![
                crate::glyph_scene::UiFileRow {
                    rel_path: "src/a file.rs".to_string(),
                    group_id: 0,
                    aabb_min: [0.0; 2],
                    aabb_max: [1.0; 2],
                },
                crate::glyph_scene::UiFileRow {
                    rel_path: "it's \"quoted\".md".to_string(),
                    group_id: 1,
                    aabb_min: [0.0; 2],
                    aabb_max: [1.0; 2],
                },
            ]),
            ..Default::default()
        };
        let rows = catalog(Some(&snap), None);
        assert!(rows.len() > crate::cli::VERB_HEADS.len() + bus::APP_WORDS.len(), "settings and file rows are there");
        for row in &rows {
            match bus::parse_line(&row.line) {
                Ok(parsed) => assert!(!parsed.commands.is_empty() || row.line.starts_with('#'), "{row:?} runs nothing"),
                // A template: its fill is a prefix of the line, for the arguments.
                Err(_) => assert!(row.line.starts_with(fill(&row.line).trim_end()), "{row:?}"),
            }
        }
        // A file row flies to exactly that path, spaces and quotes and all.
        let fly = rows.iter().find(|r| r.title == "fly to it's \"quoted\".md").expect("the file row");
        let parsed = bus::parse_line(&fly.line).unwrap();
        assert!(matches!(&parsed.commands[..], [bus::Command::Fly(bus::FlyTarget::File(f))] if f == "it's \"quoted\".md"));
        // A setting row is the flag line itself.
        assert!(rows.iter().any(|r| r.line == "--field-mode visible"));
        assert_eq!(fill("nudge-glyph"), "nudge-glyph ");
        assert_eq!(fill("pick FILE[:ROW[:COL]]"), "pick ");
        assert_eq!(fill("tint-cycle"), "tint-cycle");
    }
}
