//! The terminal loop: raw mode in and out, key handling, and the clean
//! handoff to an external command (the renderer, the build).

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, stdout, Stdout};
use std::process::Command;

use crate::manifest::Manifest;
use crate::paths::root;
use super::draw::draw_ui;
use super::state::{FocusField, LauncherState};

pub fn run(manifest: &Manifest) -> bool {
    let mut terminal = match init_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("FAIL  could not initialize terminal: {e}");
            return false;
        }
    };

    let mut state = LauncherState::new();
    let res = run_loop(&mut terminal, &mut state, manifest);
    let _ = restore_terminal(&mut terminal);
    res
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut LauncherState,
    manifest: &Manifest,
) -> bool {
    loop {
        if let Err(e) = terminal.draw(|f| draw_ui(f, state, manifest)) {
            eprintln!("FAIL  render error: {e}");
            return false;
        }

        if let Ok(Event::Key(key)) = event::read() {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return true;
                }
                KeyCode::Esc => {
                    return true;
                }
                KeyCode::Char('q') | KeyCode::Char('Q') if !state.focus.is_text_input() => {
                    return true;
                }
                KeyCode::BackTab => {
                    state.focus = state.focus.prev(state.target);
                }
                KeyCode::Tab => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        state.focus = state.focus.prev(state.target);
                    } else {
                        state.focus = state.focus.next(state.target);
                    }
                }
                KeyCode::Down => {
                    state.focus = state.focus.next(state.target);
                }
                KeyCode::Up => {
                    state.focus = state.focus.prev(state.target);
                }
                KeyCode::Left => {
                    state.cycle_prev();
                }
                KeyCode::Right => {
                    state.cycle_next();
                }
                KeyCode::Char(' ') => {
                    if state.focus.is_text_input() {
                        match state.focus {
                            FocusField::RepoPath => state.repo_path.push(' '),
                            FocusField::FocusFile => state.focus_file.push(' '),
                            FocusField::FilePath => state.file_path.push(' '),
                            FocusField::SessionPath => state.session_path.push(' '),
                            _ => {}
                        }
                    } else {
                        state.toggle_current();
                    }
                }
                KeyCode::Char('b') | KeyCode::Char('B') if !state.focus.is_text_input() => {
                    // Build action: build with --features cubecl
                    run_external_action(terminal, "Building glyph3d-native (with --features cubecl)...", || {
                        let _ = Command::new("cargo")
                            .arg("build")
                            .arg("--release")
                            .arg("-p")
                            .arg("glyph3d-native")
                            .arg("--features")
                            .arg("cubecl")
                            .current_dir(root())
                            .status();
                    });
                    state.status_message = "Build completed (release + cubecl).".to_string();
                }
                KeyCode::Char('t') | KeyCode::Char('T') if !state.focus.is_text_input() => {
                    // Test gates
                    run_external_action(terminal, "Running verification gates...", || {
                        let _ = Command::new("cargo")
                            .arg("glyph")
                            .arg("test")
                            .current_dir(root())
                            .status();
                    });
                    state.status_message = "Gate test completed.".to_string();
                }
                KeyCode::Char('r') | KeyCode::Char('R') if !state.focus.is_text_input() => {
                    state.load_config_file();
                }
                KeyCode::Enter => {
                    // Launch windowed renderer
                    let exe = root().join("target/release/glyph3d-native");
                    if !exe.exists() {
                        state.status_message =
                            "Binary not found: press [B] to build first.".to_string();
                        continue;
                    }
                    let args = state.build_cli_args();
                    let cwd = std::env::current_dir().unwrap_or_else(|_| root());

                    run_external_action(terminal, "Launching glyph3d-native...", || {
                        let _ = Command::new(&exe)
                            .args(&args)
                            .current_dir(cwd)
                            .status();
                    });
                    state.status_message = "Returned from glyph3d-native.".to_string();
                }
                KeyCode::Backspace => match state.focus {
                    FocusField::RepoPath => {
                        state.repo_path.pop();
                    }
                    FocusField::FocusFile => {
                        state.focus_file.pop();
                    }
                    FocusField::FilePath => {
                        state.file_path.pop();
                    }
                    FocusField::SessionPath => {
                        state.session_path.pop();
                    }
                    _ => {}
                },
                KeyCode::Char(c) => match state.focus {
                    FocusField::RepoPath => {
                        state.repo_path.push(c);
                    }
                    FocusField::FocusFile => {
                        state.focus_file.push(c);
                    }
                    FocusField::FilePath => {
                        state.file_path.push(c);
                    }
                    FocusField::SessionPath => {
                        state.session_path.push(c);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn run_external_action<F: FnOnce()>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    msg: &str,
    action: F,
) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    println!("\n── {msg}\n");
    action();
    println!("\n[Press Enter to return to launcher...]");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    let _ = enable_raw_mode();
    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    let _ = terminal.hide_cursor();
    let _ = terminal.clear();
}
