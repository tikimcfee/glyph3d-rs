//! The launcher: a terminal front end of the renderer itself (C27,
//! 2026-10-10; plan in `out/PLAN-LAUNCHER-INTO-RENDERER-2026-10-10.md`).
//!
//! `glyph3d-native --launcher`, or no arguments at all on a terminal, opens
//! it. Its state is a `LaunchConfig` over the CLI's own defaults (`model`),
//! its rows are drawn from the clap derives (`draw`), and Enter writes that
//! config to a file and starts this same binary with `--launch-config
//! <file>` as a child, returning to the menu when the window closes. A child
//! and not the window in-process because winit allows one event loop per
//! process (macOS cannot recreate it), so an in-process window could never
//! hand back. The file is written and read by one struct in one binary;
//! nothing is translated into flags. Each launch also saves what the
//! launcher changed into the user's own `launch_config.toml` (`save`), and a
//! repo launched from outside the presets joins `repo_presets`.
//!
//! It replaces the TUI that lived in the build tool (`cargo glyph tui`),
//! which parsed `launch_config.toml` with its own looser copy of the keys,
//! kept its own copy of every option's spelling, and handed the renderer
//! flags that the renderer then merged with the same file again.

mod draw;
mod model;
mod save;

pub use model::{Control, Launcher, Target, View};

use std::io::{self, stdout, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

/// Whether this invocation opens the launcher: `--launcher`, or no
/// arguments at all with a terminal on both ends (a script that runs the
/// bare binary still gets the default scene).
pub fn wanted(matches: &clap::ArgMatches) -> bool {
    matches.get_flag("launcher")
        || (std::env::args_os().len() == 1 && io::stdin().is_terminal() && io::stdout().is_terminal())
}

/// Run the launcher until the user quits. `config` is `--launch-config`.
pub fn run(config: Option<PathBuf>) -> i32 {
    let mut launcher = Launcher::open(config.clone());
    let mut terminal = match init_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not open the launcher's terminal: {e}");
            return 1;
        }
    };
    let res = run_loop(&mut terminal, &mut launcher, config);
    let _ = restore_terminal(&mut terminal);
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: launcher: {e}");
            1
        }
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(out))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    l: &mut Launcher,
    config: Option<PathBuf>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw::draw_ui(f, l))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let typing = l.focus.is_text_input();
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Esc => return Ok(()),
            KeyCode::Char('q' | 'Q') if !typing => return Ok(()),
            KeyCode::BackTab | KeyCode::Up => l.focus_next(-1),
            KeyCode::Tab | KeyCode::Down => l.focus_next(1),
            KeyCode::Left => l.step(-1),
            KeyCode::Right => l.step(1),
            KeyCode::Char('r' | 'R') if !typing => l.load(config.clone()),
            KeyCode::Char(' ') if !typing => l.toggle(),
            KeyCode::Enter => launch(terminal, l),
            KeyCode::Backspace => {
                if let Some(t) = l.text_mut() {
                    t.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(t) = l.text_mut() {
                    t.push(c);
                }
            }
            _ => {}
        }
    }
}

/// Where a launch's config is written: one file per launcher process,
/// overwritten by each launch and left behind, so the last launch can be
/// repeated by hand (`glyph3d-native --launch-config <it>`).
fn launch_file() -> PathBuf {
    std::env::temp_dir().join(format!("glyph3d-launch-{}.toml", std::process::id()))
}

/// Write the config and run this binary on it, with the terminal handed
/// over until it exits.
fn launch(terminal: &mut Terminal<CrosstermBackend<Stdout>>, l: &mut Launcher) {
    if l.config_error.is_some() {
        l.status = "Not launched: the config did not load (fix it, then [R])".to_string();
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The user's own file first: what you launch is what opens next time.
    l.remember_repo();
    let saved = match l.save() {
        Ok(true) => format!("saved to {}", l.save_path().display()),
        Ok(false) => String::new(),
        Err(e) => format!("NOT saved: {e}"),
    };
    let file = launch_file();
    if let Err(e) = std::fs::write(&file, l.launch_config(&cwd).to_toml_string()) {
        l.status = format!("Not launched: could not write {}: {e}", file.display());
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            l.status = format!("Not launched: where is this binary? {e}");
            return;
        }
    };
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    println!("\n── glyph3d-native --launch-config {}\n", file.display());
    let status = child(&exe, &file, &cwd);
    println!("\n[{status}; press Enter to return to the launcher]");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    let _ = enable_raw_mode();
    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    let _ = terminal.hide_cursor();
    let _ = terminal.clear();
    l.status = if saved.is_empty() { status } else { format!("{status}; {saved}") };
    l.last_launch = Some(file);
}

fn child(exe: &Path, file: &Path, cwd: &Path) -> String {
    match Command::new(exe).arg("--launch-config").arg(file).current_dir(cwd).status() {
        Ok(s) if s.success() => "Renderer exited cleanly".to_string(),
        Ok(s) => format!("Renderer exited with {s}"),
        Err(e) => format!("Renderer did not start: {e}"),
    }
}

#[cfg(test)]
mod tests;
