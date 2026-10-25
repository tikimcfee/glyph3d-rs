//! The console (F9, `panel console`; 2026-10-24): the command bus's REPL.
//! The log shows every line the bus ran — from a panel, a hotkey, a
//! self-test or typed here — with its replies, so a click teaches the line
//! that would have done it; the input line takes the same grammar
//! (`bus::parse_line`, `help` lists it). It replaced the Debug window's K2
//! scratch field as the typing-isolation trap: typing WASD/h/g/t/x here
//! must move nothing (`app::route_key`).

use std::collections::VecDeque;

use super::UiState;

/// How many entries the log keeps; the oldest go first.
pub(in crate::windowed) const LOG_CAP: usize = 500;

/// One log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::windowed) enum LogEntry {
    /// A line the bus ran (a `#` line is a note).
    Line(String),
    /// What a command replied.
    Reply(String),
    /// A line the bus refused, and why.
    Refused(String),
}

/// The bus's log, capped at `LOG_CAP` entries.
#[derive(Default)]
pub(in crate::windowed) struct CommandLog {
    entries: VecDeque<LogEntry>,
}

impl CommandLog {
    pub(in crate::windowed) fn push(&mut self, entry: LogEntry) {
        if self.entries.len() == LOG_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub(in crate::windowed) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(in crate::windowed) fn entries(&self) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter()
    }
}

/// The input line and its history (↑/↓ walk the lines typed here, newest
/// first; ↓ past the newest restores what was being typed).
#[derive(Default)]
pub(in crate::windowed) struct Console {
    pub(in crate::windowed) input: String,
    history: Vec<String>,
    /// Where ↑/↓ stand in `history`; `None` while editing a fresh line.
    position: Option<usize>,
    /// The line being typed when ↑ was first pressed.
    draft: String,
    /// Give the input line focus on the next frame (set when the console
    /// opens, so F9 then typing just works).
    pub(in crate::windowed) focus: bool,
}

impl Console {
    /// The input as submitted: recorded in the history (a repeat of the
    /// newest entry is not recorded twice) and cleared. `None` for a blank line.
    pub(in crate::windowed) fn submit(&mut self) -> Option<String> {
        let line = std::mem::take(&mut self.input).trim().to_string();
        self.position = None;
        self.draft.clear();
        if line.is_empty() {
            return None;
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        Some(line)
    }

    /// ↑: the previous line typed.
    pub(in crate::windowed) fn older(&mut self) {
        let Some(newest) = self.history.len().checked_sub(1) else { return };
        let position = match self.position {
            None => {
                self.draft = self.input.clone();
                newest
            }
            Some(p) => p.saturating_sub(1),
        };
        self.position = Some(position);
        self.input = self.history[position].clone();
    }

    /// ↓: the next line typed, or past the newest, the draft.
    pub(in crate::windowed) fn newer(&mut self) {
        let Some(p) = self.position else { return };
        if p + 1 < self.history.len() {
            self.position = Some(p + 1);
            self.input = self.history[p + 1].clone();
        } else {
            self.position = None;
            self.input = std::mem::take(&mut self.draft);
        }
    }
}

pub(in crate::windowed) fn show(ctx: &egui::Context, shell: &mut UiState, panel_lines: &mut Vec<String>) {
    let UiState { console_open, console, log, .. } = shell;
    egui::Window::new("Console (F9)")
        .id(egui::Id::new("command_console"))
        .open(console_open)
        .default_pos(egui::pos2(400.0, 600.0))
        .default_size(egui::vec2(640.0, 300.0))
        .min_size(egui::vec2(320.0, 160.0))
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            let input_height = ui.text_style_height(&egui::TextStyle::Monospace) + 12.0;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height((ui.available_height() - input_height).max(40.0))
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for entry in log.entries() {
                        match entry {
                            LogEntry::Line(line) => {
                                ui.label(egui::RichText::new(format!("> {line}")).monospace().strong());
                            }
                            LogEntry::Reply(text) => {
                                ui.label(egui::RichText::new(text).monospace());
                            }
                            LogEntry::Refused(text) => {
                                ui.label(egui::RichText::new(text).monospace().color(ui.visuals().error_fg_color));
                            }
                        }
                    }
                });
            ui.separator();
            let input = ui.add(
                egui::TextEdit::singleline(&mut console.input)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .hint_text("a command line: tint-cycle, fly src/main.rs, --field-mode visible ... (help)"),
            );
            if std::mem::take(&mut console.focus) {
                input.request_focus();
            }
            if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Some(line) = console.submit() {
                    panel_lines.push(line);
                }
                input.request_focus();
            } else if input.has_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    console.older();
                } else if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    console.newer();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_the_newest_entries_up_to_its_cap() {
        let mut log = CommandLog::default();
        for i in 0..LOG_CAP + 7 {
            log.push(LogEntry::Reply(i.to_string()));
        }
        let kept: Vec<&LogEntry> = log.entries().collect();
        assert_eq!(kept.len(), LOG_CAP);
        assert_eq!(kept[0], &LogEntry::Reply("7".to_string()), "the oldest seven went first");
        assert_eq!(kept[LOG_CAP - 1], &LogEntry::Reply((LOG_CAP + 6).to_string()));
        log.clear();
        assert_eq!(log.entries().count(), 0);
    }

    #[test]
    fn history_walks_back_and_returns_to_the_draft() {
        let mut c = Console::default();
        for line in ["tint-cycle", "  fly a.rs ", "fly a.rs", ""] {
            c.input = line.to_string();
            c.submit();
        }
        // A blank line and a repeat are not recorded; the line is trimmed.
        c.input = "pick b".to_string();
        c.older();
        assert_eq!(c.input, "fly a.rs");
        c.older();
        assert_eq!(c.input, "tint-cycle");
        c.older();
        assert_eq!(c.input, "tint-cycle", "the oldest stays put");
        c.newer();
        assert_eq!(c.input, "fly a.rs");
        c.newer();
        assert_eq!(c.input, "pick b", "past the newest, what was being typed");
        assert_eq!(c.submit().as_deref(), Some("pick b"));
        c.newer();
        assert_eq!(c.input, "", "↓ with no walk does nothing");
    }
}
