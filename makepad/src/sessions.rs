//! The agent sessions browser's state: the list gomuxai's `agent` CLI gives
//! (`AgentCli::sessions`, read when the browser first opens and again on
//! refresh), where it looked (`dirs`, `describe_dirs`), the search text and
//! the harness tab, and the load that turns a row into a relayout. The F7
//! window (`native/src/windowed/state.rs`) is the spec: the same reads, the
//! same filter, the same words.

use glyph3d_native::agent_transcript::agent_cli::{describe_dirs, nest_subagents, parse_rfc3339};
use glyph3d_native::agent_transcript::types::truncate_chars;
use glyph3d_native::agent_transcript::{load_session, AgentCli, HarnessKind, SessionInfo};
use glyph3d_native::launch_config::LaunchConfig;
use glyph3d_native::relayout::RelayoutRequest;

use crate::ui_model::{G3Choice, G3SessionEntry, G3SessionsState};

pub struct SessionBrowser {
    /// The last `agent sessions --json`, or why there is none; None until
    /// the browser first opens.
    list: Option<Result<Vec<SessionInfo>, String>>,
    /// Where gomuxai looked (`agent sessions --dirs --json`), or why it
    /// couldn't say.
    dirs_report: Result<String, String>,
    filter: String,
    /// None: every harness.
    harness: Option<HarnessKind>,
    /// Why the last load didn't load.
    load_error: Option<String>,
}

impl Default for SessionBrowser {
    fn default() -> Self {
        Self { list: None, dirs_report: Ok(String::new()), filter: String::new(), harness: None, load_error: None }
    }
}

/// The `agent` CLI the launch config names.
fn agent() -> Result<AgentCli, String> {
    AgentCli::from_launch_config(&LaunchConfig::load_or_default())
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
fn age(dur: std::time::Duration) -> String {
    let secs = dur.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

impl SessionBrowser {
    /// Read the list (and where it came from) again.
    pub fn refresh(&mut self) {
        let listed = agent().and_then(|agent| {
            self.dirs_report = agent.dirs().map(|d| describe_dirs(&d));
            agent.sessions()
        });
        self.list = Some(listed);
    }

    /// The first open reads the list; later opens keep it until a refresh.
    pub fn ensure_listed(&mut self) {
        if self.list.is_none() {
            self.refresh();
        }
    }

    pub fn set_filter(&mut self, text: String) {
        self.filter = text;
    }

    /// Tab `0` is every harness, then `HarnessKind::ALL` in order.
    pub fn set_harness(&mut self, tab: usize) {
        self.harness = tab.checked_sub(1).and_then(|i| HarnessKind::ALL.get(i).copied());
    }

    /// Read the session at `path` and ask for the carrel it becomes; on a
    /// failure the browser says why and nothing is rebuilt.
    pub fn load(&mut self, path: &str) -> Option<RelayoutRequest> {
        match agent().and_then(|agent| load_session(&agent, path).map(|l| (agent, l))) {
            Ok((agent, loaded)) => {
                self.load_error = None;
                Some(RelayoutRequest {
                    switch_agent_session: Some((path.to_string(), agent, loaded)),
                    ..Default::default()
                })
            }
            Err(e) => {
                self.load_error = Some(e);
                None
            }
        }
    }

    /// The panel's state: tabs, the scan line, the filtered rows (a parent
    /// stays when a subagent of it matches) and the counts.
    pub fn state(&self) -> G3SessionsState {
        let harness_tabs = std::iter::once(G3Choice { label: "All".to_string(), selected: self.harness.is_none() })
            .chain(HarnessKind::ALL.iter().map(|h| G3Choice {
                label: h.label().to_string(),
                selected: self.harness == Some(*h),
            }))
            .collect();
        let (scanning_line, scanning_error) = match &self.dirs_report {
            Ok(report) => (format!("Scanning: {report}"), false),
            Err(e) => (format!("Scanning: {e}"), true),
        };
        let mut state = G3SessionsState {
            filter: self.filter.clone(),
            harness_tabs,
            scanning_line,
            scanning_error,
            error: self.load_error.clone(),
            ..Default::default()
        };
        let all = match &self.list {
            Some(Ok(list)) => list,
            Some(Err(e)) => {
                state.error = Some(e.clone());
                return state;
            }
            None => return state,
        };
        let nested = nest_subagents(all);
        if nested.is_empty() {
            state.empty_line = Some(
                "gomuxai found no sessions. Point claude_projects_dir, agy_brain_dir or \
                 kimi_sessions_dir in launch_config.toml elsewhere, or set one to \"\" \
                 to skip that app."
                    .to_string(),
            );
            return state;
        }
        let needle = self.filter.to_lowercase();
        let shown = |s: &SessionInfo| {
            self.harness.is_none_or(|h| s.harness_kind() == Some(h))
                && (needle.is_empty()
                    || [&s.title, &s.session_id, &s.project, &s.path]
                        .iter()
                        .any(|f| f.to_lowercase().contains(&needle)))
        };
        let colors = &glyph3d_native::config::settings().ui;
        let now = std::time::SystemTime::now();
        for (top, subs) in nested {
            let subs: Vec<&SessionInfo> = subs.into_iter().filter(|s| shown(s)).collect();
            if !shown(top) && subs.is_empty() {
                continue;
            }
            for (s, is_subagent) in std::iter::once((top, false)).chain(subs.into_iter().map(|s| (s, true))) {
                let (harness_color, harness_tag) = match s.harness_kind() {
                    Some(HarnessKind::Claude) => (colors.harness_claude, "[Claude]".to_string()),
                    Some(HarnessKind::Agy) => (colors.harness_agy, "[Antigravity]".to_string()),
                    Some(HarnessKind::Kimi) => (colors.harness_kimi, "[Kimi]".to_string()),
                    None => (colors.project_name, s.harness.clone()),
                };
                state.rows.push(G3SessionEntry {
                    path: s.path.clone(),
                    is_subagent,
                    harness_tag,
                    harness_color,
                    project: s.project.clone(),
                    project_color: colors.project_name,
                    status: s.status.clone(),
                    age: parse_rfc3339(&s.mtime)
                        .and_then(|m| now.duration_since(m).ok())
                        .map(age)
                        .unwrap_or_default(),
                    title: if s.title.is_empty() { "(untitled)".to_string() } else { s.title.clone() },
                    id_line: format!("ID: {}…", truncate_chars(&s.session_id, 16)),
                    size_line: format!("{:.1} KB", s.size as f64 / 1024.0),
                });
            }
        }
        state.count_line = Some(format!(
            "Showing {} of {} sessions",
            state.rows.len(),
            all.iter().filter(|s| !s.path.is_empty()).count()
        ));
        if state.rows.is_empty() {
            state.empty_line = Some("No matching sessions found.".to_string());
        }
        state
    }
}
