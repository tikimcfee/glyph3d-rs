//! Drawing: the launcher's panels, generic over its controls. A row is drawn
//! from `Launcher::view`, so a choice the CLI gains appears here unasked.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

use super::model::{Control, Launcher, Section, Target, View};

pub(crate) fn draw_ui(f: &mut Frame, l: &Launcher) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(17),   // two-column body
            Constraint::Length(4), // what Enter launches
            Constraint::Length(1), // keys
        ])
        .split(f.area());
    draw_header(f, chunks[0]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(8)])
        .split(cols[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(6)])
        .split(cols[1]);
    draw_section(f, left[0], l, Section::Target, " 1. Launch Target ");
    draw_section(f, left[1], l, Section::Layout, " 2. Layout ");
    draw_section(f, right[0], l, Section::Graphics, " 3. Graphics & LOD ");
    draw_sidebar(f, right[1], l);
    draw_preview(f, chunks[2], l);
    draw_footer(f, chunks[3]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(vec![
            Span::styled(" GLYPH3D ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
            Span::styled(" Launcher", Style::default().fg(Color::White).bold()),
        ]),
        Line::from(vec![
            Span::styled(" Navigate: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Tab / Up / Down", Style::default().fg(Color::Yellow)),
            Span::styled("   Select/Cycle: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Left / Right / Space", Style::default().fg(Color::Yellow)),
            Span::styled("   Launch: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
        ]),
    ];
    let block = Block::default().borders(Borders::BOTTOM).border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn label_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn line_prefix(focused: bool) -> Span<'static> {
    if focused {
        Span::styled("▶ ", Style::default().fg(Color::Cyan).bold())
    } else {
        Span::raw("  ")
    }
}

/// One control's row.
fn control_line(l: &Launcher, c: Control) -> Line<'static> {
    let focused = l.focus == c;
    let mut spans = vec![line_prefix(focused), Span::styled(format!("{:<14}", format!("{}:", c.label())), label_style(focused))];
    match l.view(c) {
        View::Choice(opts) => {
            for (i, o) in opts.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(if c == Control::Target {
                    format_radio(&o.name, o.selected, focused)
                } else {
                    format_choice(&o.name, o.selected, focused)
                });
            }
        }
        View::Toggle(on) => spans.push(format_toggle(on, focused)),
        View::Dial { value, default, unit } => {
            spans.push(format_dial(value.unwrap_or(default), focused));
            spans.push(Span::styled(
                if value.is_some() { format!(" {unit}") } else { format!(" {unit} (default)") },
                Style::default().fg(Color::DarkGray),
            ));
        }
        View::Text { value, placeholder } => {
            let shown = if value.is_empty() { placeholder.to_string() } else { value };
            spans.push(Span::styled(
                format!("{shown:<24}"),
                if focused {
                    Style::default().bg(Color::Blue).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ));
            if c == Control::RepoPath {
                spans.push(Span::styled(" (◄/► presets)", Style::default().fg(Color::DarkGray)));
            }
        }
    }
    Line::from(spans)
}

/// The focused control's description, for the panel's bottom border: the
/// selected choice's help from its clap derive, or a session's title.
fn focused_help(l: &Launcher) -> Option<String> {
    match l.focus {
        Control::SessionPath => l.session_title(),
        c => match l.view(c) {
            View::Choice(opts) => opts.into_iter().find(|o| o.selected).and_then(|o| o.help),
            _ => None,
        },
    }
}

fn draw_section(f: &mut Frame, area: Rect, l: &Launcher, section: Section, title: &'static str) {
    let mut lines: Vec<Line> = Control::ALL
        .iter()
        .filter(|c| c.section() == section && l.is_applicable(**c))
        .map(|c| control_line(l, *c))
        .collect();
    if section == Section::Target && l.target == Target::Demo {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("The quad-field stress demo", Style::default().fg(Color::Magenta)),
        ]));
    }
    let focused = l.focus.section() == section;
    let mut block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) });
    if focused {
        if let Some(sub) = focused_help(l).and_then(|h| format_subtitle_description(&h, area.width)) {
            block = block.title_bottom(sub);
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_sidebar(f: &mut Frame, area: Rect, l: &Launcher) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::from(vec![
        Span::styled("Config file:  ", dim),
        Span::styled(
            match &l.config_source {
                Some(p) => format!("{} (a launch saves changes here)", p.display()),
                None => "none: CLI defaults (a launch creates ./launch_config.toml)".into(),
            },
            Style::default().fg(Color::Yellow),
        ),
    ])];
    if let Some(e) = &l.config_error {
        lines.push(Line::from(Span::styled(e.clone(), Style::default().fg(Color::Red).bold())));
    }
    lines.push(Line::from(vec![
        Span::styled("Status:       ", dim),
        Span::styled(l.status.clone(), Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Sessions:     ", dim),
        Span::styled(l.session_dirs_report(), Style::default().fg(Color::White)),
    ]));
    if let Some(p) = &l.last_launch {
        lines.push(Line::from(vec![
            Span::styled("Last launch:  ", dim),
            Span::styled(p.display().to_string(), Style::default().fg(Color::White)),
        ]));
    }
    let block = Block::default()
        .title(" 4. Status ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim);
    f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }).block(block), area);
}

/// What Enter launches: the renderer with the config it will write, its
/// keys summarized.
fn draw_preview(f: &mut Frame, area: Rect, l: &Launcher) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let keys: Vec<String> = l
        .launch_config(&cwd)
        .to_toml_string()
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.replace(" = ", "="))
        .collect();
    let p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("$ ", Style::default().fg(Color::Green).bold()),
            Span::styled("glyph3d-native --launch-config <this>", Style::default().fg(Color::White).bold()),
        ]),
        Line::from(Span::styled(keys.join("  "), Style::default().fg(Color::Gray))),
    ])
    .block(
        Block::default()
            .title(" Launches ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Green)),
    );
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let keys = Line::from(vec![
        Span::styled(" [Enter] ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        Span::styled("Launch  ", Style::default().fg(Color::White)),
        Span::styled(" [R] ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
        Span::styled("Reload Config  ", Style::default().fg(Color::White)),
        Span::styled(" [Space] ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::styled("Toggle  ", Style::default().fg(Color::White)),
        Span::styled(" [◄/►] ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::styled("Adjust/Preset  ", Style::default().fg(Color::White)),
        Span::styled(" [Q / Esc] ", Style::default().bg(Color::Red).fg(Color::White).bold()),
        Span::styled("Quit", Style::default().fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(keys).alignment(Alignment::Center), area);
}

fn format_radio(label: &str, selected: bool, focused: bool) -> Span<'static> {
    if selected {
        let s = Style::default().fg(Color::Cyan).bold();
        Span::styled(format!("[● {label}]"), if focused { s.bg(Color::Cyan).fg(Color::Black) } else { s })
    } else {
        Span::styled(format!("[○ {label}]"), Style::default().fg(Color::DarkGray))
    }
}

fn format_choice(label: &str, selected: bool, focused: bool) -> Span<'static> {
    if selected {
        let s = Style::default().fg(Color::Cyan).bold();
        Span::styled(format!("[{label}]"), if focused { s.bg(Color::Cyan).fg(Color::Black) } else { s })
    } else {
        Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray))
    }
}

fn format_dial(val: f64, focused: bool) -> Span<'static> {
    let text = format!("[◄ {val:.2} ►]");
    if focused {
        Span::styled(text, Style::default().bg(Color::Cyan).fg(Color::Black).bold())
    } else {
        Span::styled(text, Style::default().fg(Color::Yellow).bold())
    }
}

fn format_toggle(enabled: bool, focused: bool) -> Span<'static> {
    match (enabled, focused) {
        (true, true) => Span::styled("[● ENABLED]", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        (true, false) => Span::styled("[● ENABLED]", Style::default().fg(Color::Green).bold()),
        (false, true) => Span::styled("[○ DISABLED]", Style::default().bg(Color::Red).fg(Color::White).bold()),
        (false, false) => Span::styled("[○ DISABLED]", Style::default().fg(Color::Red)),
    }
}

/// A description for a panel's bottom border, cut to fit with an ellipsis;
/// `None` when the panel is too narrow to say anything.
fn format_subtitle_description(description: &str, area_width: u16) -> Option<Line<'static>> {
    if area_width < 18 {
        return None;
    }
    // Border corners (2), border margins (2), prefix " ℹ " (3), suffix " " (1).
    let available = area_width.saturating_sub(8) as usize;
    if available < 10 {
        return None;
    }
    // A clap help may run to several lines; the border holds one.
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = if flat.chars().count() <= available {
        flat
    } else {
        format!("{}…", flat.chars().take(available - 1).collect::<String>())
    };
    Some(Line::from(vec![
        Span::styled(" ℹ ", Style::default().fg(Color::Cyan).bold()),
        Span::styled(format!("{text} "), Style::default().fg(Color::White)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn render(l: &Launcher, section: Section, w: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, 14)).unwrap();
        t.draw(|f| draw_section(f, Rect::new(0, 0, w, 14), l, section, " t ")).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn subtitle_fits_or_is_cut() {
        assert!(format_subtitle_description("Short test string", 17).is_none());
        let wide = format_subtitle_description("Short test string", 60).unwrap();
        assert_eq!(wide.spans[1].content, "Short test string ");
        let narrow = format_subtitle_description("Short test string", 22).unwrap();
        assert_eq!(narrow.spans[1].content, "Short test st… ");
        let folded = format_subtitle_description("two\n   lines", 60).unwrap();
        assert_eq!(folded.spans[1].content, "two lines ");
    }

    /// The focused choice row's bottom border carries the selected value's
    /// help — the doc comment on the variant the flag parses, not a copy.
    #[test]
    fn focused_choice_shows_its_help_from_the_derive() {
        let mut l = Launcher::defaults();
        l.focus = Control::FieldMode;
        l.cfg.field_mode = Some(glyph_field::GlyphFieldMode::Visible);
        let out = render(&l, Section::Graphics, 60);
        assert!(out.contains("[visible]"), "{out}");
        assert!(out.contains("No slot per glyph"), "the variant's doc comment is the help: {out}");

        l.cfg.field_mode = Some(glyph_field::GlyphFieldMode::Derived);
        let out = render(&l, Section::Graphics, 60);
        assert!(out.contains("Compact 20 B record"), "{out}");

        l.focus = Control::Greeking;
        let out = render(&l, Section::Graphics, 60);
        assert!(!out.contains("Compact 20 B record"), "an unfocused row shows no help: {out}");
    }

    /// Every choice row fits the launcher's half-width column (60 of a
    /// 120-column terminal): the last choice of each is drawn whole.
    #[test]
    fn every_choice_row_fits_half_width() {
        for target in Target::ALL {
            let mut l = Launcher::defaults();
            l.target = target;
            for section in [Section::Target, Section::Layout, Section::Graphics] {
                let out = render(&l, section, 60);
                for c in Control::ALL.iter().filter(|c| c.section() == section && l.is_applicable(**c)) {
                    if let View::Choice(opts) = l.view(*c) {
                        let last = &opts.last().unwrap().name;
                        assert!(out.contains(last.as_str()), "{c:?}: '{last}' cut off at 60 columns:\n{out}");
                    }
                }
            }
        }
    }
}
