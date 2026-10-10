//! Drawing: every panel of the launcher, and the small span formatters.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

use crate::manifest::Manifest;
use crate::paths::{gpu_key, gpu_profile};
use crate::products::is_current;
use super::state::{
    ClusterMode, ColorMode, FieldMode, FocusField, LauncherState, LayoutMode, PresentMode, RepoEngine,
    TargetMode, WrapMode,
};

pub(crate) fn draw_ui(f: &mut Frame, state: &LauncherState, manifest: &Manifest) {
    let size = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(14),   // 2-Column Body
            Constraint::Length(3), // Live command preview
            Constraint::Length(1), // Footer keybinds
        ])
        .split(size);

    draw_header(f, chunks[0]);
    draw_body(f, chunks[1], state, manifest);
    draw_preview(f, chunks[2], state);
    draw_footer(f, chunks[3]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let header_text = vec![
        Line::from(vec![
            Span::styled(" GLYPH3D ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
            Span::styled(" Interactive Launcher & Configuration Mission Control", Style::default().fg(Color::White).bold()),
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

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::Cyan));
    let p = Paragraph::new(header_text).block(block);
    f.render_widget(p, area);
}

fn draw_body(f: &mut Frame, area: Rect, state: &LauncherState, manifest: &Manifest) {
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Left column: Target & Layout
            Constraint::Percentage(50), // Right column: Graphics & Environment
        ])
        .split(area);

    draw_left_column(f, main_chunks[0], state);
    draw_right_column(f, main_chunks[1], state, manifest);
}

fn draw_left_column(f: &mut Frame, area: Rect, state: &LauncherState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // 1. Launch Target
            Constraint::Min(8),    // 2. Layout & Layout Engine
        ])
        .split(area);

    draw_target_section(f, chunks[0], state);
    draw_layout_section(f, chunks[1], state);
}

fn draw_right_column(f: &mut Frame, area: Rect, state: &LauncherState, manifest: &Manifest) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // 3. Graphics & Shading
            Constraint::Min(6),    // 4. Hardware & Environment
        ])
        .split(area);

    draw_graphics_section(f, chunks[0], state);
    draw_sidebar(f, chunks[1], state, manifest);
}

fn line_prefix(focused: bool) -> Span<'static> {
    if focused {
        Span::styled("▶ ", Style::default().fg(Color::Cyan).bold())
    } else {
        Span::raw("  ")
    }
}

fn draw_target_section(f: &mut Frame, area: Rect, state: &LauncherState) {
    let is_focused = state.focus == FocusField::TargetMode;
    let mut lines = Vec::new();

    // Line 1: Target Mode
    lines.push(Line::from(vec![
        line_prefix(is_focused),
        Span::styled(
            "Target: ",
            if is_focused {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        format_radio("Repo", state.target == TargetMode::Repo, is_focused),
        Span::raw(" "),
        format_radio("File", state.target == TargetMode::File, is_focused),
        Span::raw(" "),
        format_radio("Agent", state.target == TargetMode::Agent, is_focused),
        Span::raw(" "),
        format_radio("Demo", state.target == TargetMode::Demo, is_focused),
    ]));

    // Line 2 & 3: Depending on target mode
    match state.target {
        TargetMode::Repo => {
            let repo_focused = state.focus == FocusField::RepoPath;
            let focus_file_focused = state.focus == FocusField::FocusFile;
            lines.push(Line::from(vec![
                line_prefix(repo_focused),
                Span::styled(
                    "Repo:   ",
                    if repo_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{:<22}", if state.repo_path.is_empty() { "." } else { &state.repo_path }),
                    if repo_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
                Span::styled(" (◄/► presets)", Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(vec![
                line_prefix(focus_file_focused),
                Span::styled(
                    "Focus:  ",
                    if focus_file_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{:<25}", if state.focus_file.is_empty() { "(all files)" } else { &state.focus_file }),
                    if focus_file_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
            ]));
        }
        TargetMode::File => {
            let file_focused = state.focus == FocusField::FilePath;
            lines.push(Line::from(vec![
                line_prefix(file_focused),
                Span::styled(
                    "File:   ",
                    if file_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{:<25}", state.file_path),
                    if file_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Staged directly via Slug WGSL pipeline", Style::default().fg(Color::DarkGray)),
            ]));
        }
        TargetMode::Agent => {
            let session_focused = state.focus == FocusField::SessionPath;
            lines.push(Line::from(vec![
                line_prefix(session_focused),
                Span::styled(
                    "Session:",
                    if session_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(" "),
                Span::styled(
                    format!("{:<28}", if state.session_path.is_empty() { "(press ◄/► to pick recent)" } else { &state.session_path }),
                    if session_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("3D Rolodex deck & Carrel navigation (◄/► recent)", Style::default().fg(Color::DarkGray)),
            ]));
        }
        TargetMode::Demo => {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Stage A 1M quad-field stress demo", Style::default().fg(Color::Magenta)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Evaluates raw GPU fillrate & instancing", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let is_section_focused = matches!(
        state.focus,
        FocusField::TargetMode
            | FocusField::RepoPath
            | FocusField::FocusFile
            | FocusField::FilePath
            | FocusField::SessionPath
    );

    let block = Block::default()
        .title(" 1. Launch Target ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_section_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_layout_section(f: &mut Frame, area: Rect, state: &LauncherState) {
    let is_layout = state.focus == FocusField::LayoutMode;
    let is_wrap = state.focus == FocusField::WrapMode;
    let is_z_spacing = state.focus == FocusField::ZWrapSpacing;
    let is_engine = state.focus == FocusField::RepoEngine;
    let is_color = state.focus == FocusField::ColorMode;
    let is_cluster = state.focus == FocusField::ClusterMode;

    let lines = vec![
        Line::from(vec![
            line_prefix(is_layout),
            Span::styled(
                "Layout:   ",
                if is_layout {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("shelf", state.layout_mode == LayoutMode::Shelf, is_layout),
            Span::raw(" "),
            format_choice("carrel", state.layout_mode == LayoutMode::Carrel, is_layout),
        ]),
        Line::from(vec![
            line_prefix(is_wrap),
            Span::styled(
                "Wrap:     ",
                if is_wrap {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("back (Z-depth)", state.wrap_mode == WrapMode::Back, is_wrap),
            Span::raw(" "),
            format_choice("down (cols)", state.wrap_mode == WrapMode::Down, is_wrap),
        ]),
        Line::from(vec![
            line_prefix(is_z_spacing),
            Span::styled(
                "Z Spacing:",
                if is_z_spacing {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(" "),
            format_dial(state.z_wrap_spacing, is_z_spacing),
            Span::styled(" × em cell (◄/► dial)", Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            line_prefix(is_engine),
            Span::styled(
                "Engine:   ",
                if is_engine {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("hyper (Rayon)", state.repo_engine == RepoEngine::Hyper, is_engine),
            Span::raw(" "),
            format_choice("direct", state.repo_engine == RepoEngine::Direct, is_engine),
            Span::raw(" "),
            format_choice("batch", state.repo_engine == RepoEngine::Batch, is_engine),
        ]),
        Line::from(vec![
            line_prefix(is_color),
            Span::styled(
                "Color:    ",
                if is_color {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("syntax (lexer)", state.color_mode == ColorMode::Syntax, is_color),
            Span::raw(" "),
            format_choice("flat", state.color_mode == ColorMode::Flat, is_color),
        ]),
        Line::from(vec![
            line_prefix(is_cluster),
            Span::styled(
                "Cluster:  ",
                if is_cluster {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("cluster (UAX29)", state.cluster_mode == ClusterMode::Cluster, is_cluster),
            Span::raw(" "),
            format_choice("leader", state.cluster_mode == ClusterMode::Leader, is_cluster),
        ]),
    ];

    let is_section_focused = matches!(
        state.focus,
        FocusField::LayoutMode
            | FocusField::WrapMode
            | FocusField::ZWrapSpacing
            | FocusField::RepoEngine
            | FocusField::ColorMode
            | FocusField::ClusterMode
    );

    let mut block = Block::default()
        .title(" 2. Layout & Layout Engine ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_section_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    if is_engine {
        if let Some(subtitle) = format_subtitle_description(state.repo_engine.description(), area.width) {
            block = block.title_bottom(subtitle);
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_graphics_section(f: &mut Frame, area: Rect, state: &LauncherState) {
    let is_greeking = state.focus == FocusField::Greeking;
    let is_cards = state.focus == FocusField::FileBackgrounds;
    let is_cull = state.focus == FocusField::Cull;
    let is_field_mode = state.focus == FocusField::FieldMode;
    let is_ui = state.focus == FocusField::UiOverlay;
    let is_present = state.focus == FocusField::PresentMode;

    let show_hint = area.width >= 48;

    let lines = vec![
        Line::from(vec![
            line_prefix(is_greeking),
            Span::styled(
                "Greeking:       ",
                if is_greeking {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.greeking, is_greeking),
            if show_hint {
                Span::styled(" (subpixel ink)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_cards),
            Span::styled(
                "File Cards:     ",
                if is_cards {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.file_backgrounds, is_cards),
            if show_hint {
                Span::styled(" (bounds)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_cull),
            Span::styled(
                "Frustum Culling:",
                if is_cull {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.cull, is_cull),
            if show_hint {
                Span::styled(" (AABB cull)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_field_mode),
            Span::styled(
                "Field Mode:     ",
                if is_field_mode {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("instanced (32B)", state.field_mode == FieldMode::Instanced, is_field_mode),
            Span::raw(" "),
            format_choice("derived (20B)", state.field_mode == FieldMode::Derived, is_field_mode),
        ]),
        Line::from(vec![
            line_prefix(is_ui),
            Span::styled(
                "Egui Overlay:   ",
                if is_ui {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.ui_overlay, is_ui),
            if show_hint {
                Span::styled(" (in-app UI)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_present),
            Span::styled(
                "Present Mode:   ",
                if is_present {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("fifo", state.present_mode == PresentMode::Fifo, is_present),
            Span::raw(" "),
            format_choice("mailbox", state.present_mode == PresentMode::Mailbox, is_present),
            Span::raw(" "),
            format_choice("immediate", state.present_mode == PresentMode::Immediate, is_present),
        ]),
    ];

    let is_section_focused = matches!(
        state.focus,
        FocusField::Greeking
            | FocusField::FileBackgrounds
            | FocusField::Cull
            | FocusField::FieldMode
            | FocusField::UiOverlay
            | FocusField::PresentMode
    );

    let mut block = Block::default()
        .title(" 3. Graphics & Shading ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_section_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    if is_field_mode {
        if let Some(subtitle) = format_subtitle_description(state.field_mode.description(), area.width) {
            block = block.title_bottom(subtitle);
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_sidebar(f: &mut Frame, area: Rect, state: &LauncherState, manifest: &Manifest) {
    let gpu = gpu_key().unwrap_or("unknown");
    let prof = gpu_profile()
        .unwrap_or("unavailable")
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let renderer_art = manifest.artifact.get("renderer");
    let is_curr = renderer_art.map(|a| is_current("renderer", a)).unwrap_or(false);

    let diag_lines = vec![
        Line::from(vec![
            Span::styled("GPU Adapter:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if prof.is_empty() { "Probing..." } else { &prof },
                Style::default().fg(Color::Cyan).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("Golden Key:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(gpu, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Build Status: ", Style::default().fg(Color::DarkGray)),
            if is_curr {
                Span::styled("● CURRENT (up to date)", Style::default().fg(Color::Green).bold())
            } else {
                Span::styled("○ STALE (press B to build)", Style::default().fg(Color::Red).bold())
            },
        ]),
        Line::from(vec![
            Span::styled("Config File:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                state.config_source.as_deref().unwrap_or("None (default flags)"),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("Status:       ", Style::default().fg(Color::DarkGray)),
            Span::styled(&state.status_message, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("Sessions:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(state.session_dirs_report(), Style::default().fg(Color::White)),
        ]),
    ];

    let block = Block::default()
        .title(" 4. Environment & Status ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(diag_lines).block(block), area);
}

fn draw_preview(f: &mut Frame, area: Rect, state: &LauncherState) {
    let cmd = state.command_preview();
    let p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("$ ", Style::default().fg(Color::Green).bold()),
            Span::styled(cmd, Style::default().fg(Color::White).bold()),
        ]),
    ])
    .block(
        Block::default()
            .title(" Live Invocation Preview ")
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
        Span::styled(" [B] ", Style::default().bg(Color::Blue).fg(Color::White).bold()),
        Span::styled("Build  ", Style::default().fg(Color::White)),
        Span::styled(" [T] ", Style::default().bg(Color::Magenta).fg(Color::White).bold()),
        Span::styled("Test Gates  ", Style::default().fg(Color::White)),
        Span::styled(" [R] ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
        Span::styled("Reload Config  ", Style::default().fg(Color::White)),
        Span::styled(" [Space] ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::styled("Toggle  ", Style::default().fg(Color::White)),
        Span::styled(" [◄/►] ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::styled("Adjust/Preset  ", Style::default().fg(Color::White)),
        Span::styled(" [Q / Esc] ", Style::default().bg(Color::Red).fg(Color::White).bold()),
        Span::styled("Quit", Style::default().fg(Color::White)),
    ]);
    let p = Paragraph::new(keys).alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn format_radio<'a>(label: &'a str, selected: bool, focused: bool) -> Span<'a> {
    if selected {
        if focused {
            Span::styled(format!("[● {label}]"), Style::default().bg(Color::Cyan).fg(Color::Black).bold())
        } else {
            Span::styled(format!("[● {label}]"), Style::default().fg(Color::Cyan).bold())
        }
    } else {
        Span::styled(format!("[○ {label}]"), Style::default().fg(Color::DarkGray))
    }
}

fn format_choice<'a>(label: &'a str, selected: bool, focused: bool) -> Span<'a> {
    if selected {
        if focused {
            Span::styled(format!("[{label}]"), Style::default().bg(Color::Cyan).fg(Color::Black).bold())
        } else {
            Span::styled(format!("[{label}]"), Style::default().fg(Color::Cyan).bold())
        }
    } else {
        Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray))
    }
}

fn format_dial<'a>(val: f64, focused: bool) -> Span<'a> {
    let text = format!("[◄ {:.2} ►]", val);
    if focused {
        Span::styled(text, Style::default().bg(Color::Cyan).fg(Color::Black).bold())
    } else {
        Span::styled(text, Style::default().fg(Color::Yellow).bold())
    }
}

fn format_toggle(enabled: bool, focused: bool) -> Span<'static> {
    if enabled {
        if focused {
            Span::styled("[● ENABLED]", Style::default().bg(Color::Green).fg(Color::Black).bold())
        } else {
            Span::styled("[● ENABLED]", Style::default().fg(Color::Green).bold())
        }
    } else if focused {
        Span::styled("[○ DISABLED]", Style::default().bg(Color::Red).fg(Color::White).bold())
    } else {
        Span::styled("[○ DISABLED]", Style::default().fg(Color::Red))
    }
}

fn format_subtitle_description(description: &str, area_width: u16) -> Option<Line<'static>> {
    // Require sufficient width to render border corners, margins, icon, and meaningful text.
    if area_width < 18 {
        return None;
    }

    // Border corners (2), border margins (2), prefix " ℹ " (3), suffix " " (1) = 8 columns.
    let available_width = area_width.saturating_sub(8) as usize;
    if available_width < 10 {
        return None;
    }

    let character_count = description.chars().count();
    let text = if character_count <= available_width {
        description.to_string()
    } else {
        let max_characters = available_width.saturating_sub(1);
        let prefix: String = description.chars().take(max_characters).collect();
        format!("{prefix}…")
    };

    Some(Line::from(vec![
        Span::styled(" ℹ ", Style::default().fg(Color::Cyan).bold()),
        Span::styled(format!("{text} "), Style::default().fg(Color::White)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;

    #[test]
    fn test_format_subtitle_description_narrow_and_wide() {
        let desc = "Short test string";
        // Less than 18 columns returns None
        assert!(format_subtitle_description(desc, 17).is_none());

        // Wide area returns full string
        let wide = format_subtitle_description(desc, 60).unwrap();
        assert_eq!(wide.spans[0].content, " ℹ ");
        assert_eq!(wide.spans[1].content, "Short test string ");

        // Narrow area truncates with ellipsis
        // Area width 22: available_width = 14. 13 chars + '…' + ' '
        let narrow = format_subtitle_description(desc, 22).unwrap();
        assert_eq!(narrow.spans[0].content, " ℹ ");
        assert_eq!(narrow.spans[1].content, "Short test st… ");
    }

    #[test]
    fn test_tui_subtitles_rendered_on_select() {
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // 1. RepoEngine focused: layout section should display the engine description
        let mut state = LauncherState::defaults();
        state.focus = FocusField::RepoEngine;
        state.repo_engine = RepoEngine::Hyper;

        terminal
            .draw(|f| {
                draw_layout_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("Parallel CPU Rayon layout"));

        // Change engine to Direct: subtitle updates immediately
        state.repo_engine = RepoEngine::Direct;
        terminal
            .draw(|f| {
                draw_layout_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("Direct CPU layout path"));

        // Unfocus RepoEngine: layout section should not show engine subtitle
        state.focus = FocusField::LayoutMode;
        terminal
            .draw(|f| {
                draw_layout_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(!content.contains("Parallel CPU Rayon layout"));
        assert!(!content.contains("Pure GPU parallel compute"));

        // 2. FieldMode focused: graphics section should display field mode description
        state.focus = FocusField::FieldMode;
        state.field_mode = FieldMode::Derived;
        terminal
            .draw(|f| {
                draw_graphics_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("20 B DerivedSlot"));

        // Change field mode to Instanced: subtitle updates immediately
        state.field_mode = FieldMode::Instanced;
        terminal
            .draw(|f| {
                draw_graphics_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("32 B RenderSlot"));

        // Unfocus FieldMode: graphics section should not show field mode subtitle
        state.focus = FocusField::Greeking;
        terminal
            .draw(|f| {
                draw_graphics_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(!content.contains("32 B RenderSlot"));
        assert!(!content.contains("20 B DerivedSlot"));
    }
}
