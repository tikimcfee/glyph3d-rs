//! Live terminals: [`term_session`] in front of [`Terminal`].
//!
//! One [`SessionHub`] connects to a `g3d-termd` namespace, routes frames to one background OS
//! thread per terminal, and coalesces screen updates (`16 ms` / `16 KiB` leading-edge, plus DEC
//! mode 2026 whole-frame gating). On the window's thread, [`LiveTerminal::pump`] only swaps in
//! finished [`Screen`] rows and never parses VT bytes or blocks on socket round-trips.

use std::sync::Arc;

use term_cells::Screen;
use term_session::{
    DaemonEvent, Key, KeyAction, OptionAsAlt, SessionHub, Target, TerminalSession,
};
use wall_core::input::{KeyEvent, Platform};
use wall_core::keymap::{self, TerminalKeyAction};

use super::app::Terminal;

/// The size a terminal the wall starts is born at.
pub const NEW_COLS: u16 = 100;
pub const NEW_ROWS: u16 = 30;

/// The namespace the native wall keeps its terminals in. Not `wall`: that
/// one is the browser wall's, and two owners of a terminal fight over its
/// size.
pub const NAMESPACE: &str = "rs-wall";

/// A live terminal in `g3d-termd` whose emulator runs on its own session thread.
pub struct LiveTerminal {
    hub: SessionHub,
    session: TerminalSession,
    logged_exit: bool,
    logged_error: Option<(String, String)>,
}

impl LiveTerminal {
    /// Wraps a running [`TerminalSession`] belonging to `hub`.
    pub fn new(hub: SessionHub, session: TerminalSession) -> Self {
        Self {
            hub,
            session,
            logged_exit: false,
            logged_error: None,
        }
    }

    /// The terminal's id in `g3d-termd`.
    pub fn id(&self) -> &str {
        self.session.id()
    }

    /// Shared namespace hub (for spawning, adopting, or killing sibling terminals).
    pub fn hub(&self) -> &SessionHub {
        &self.hub
    }

    /// Underlying [`TerminalSession`] (modes, pwd, scrollback, selection text, resize).
    pub fn session(&self) -> &TerminalSession {
        &self.session
    }

    /// Mutable access to the underlying [`TerminalSession`].
    pub fn session_mut(&mut self) -> &mut TerminalSession {
        &mut self.session
    }
}

impl Terminal for LiveTerminal {
    fn screen(&self) -> &Screen {
        self.session.screen()
    }

    fn pump(&mut self) -> bool {
        for ev in self.hub.poll_daemon_events() {
            match ev {
                DaemonEvent::Reconnected { restart, .. } => {
                    log::info!("termd came back (restart: {restart}); its terminals repaint");
                }
                DaemonEvent::Gone { message, .. } => {
                    log::error!("termd is gone: {message}");
                }
                DaemonEvent::UpgradeFailed { message } => {
                    log::warn!("termd upgrade failed: {message}");
                }
                DaemonEvent::Status(_) => {}
            }
        }

        let changed = self.session.pump();
        if !self.logged_exit {
            if let Some(exit) = self.session.exit_status() {
                self.logged_exit = true;
                log::info!(
                    "terminal {} ended (code {:?}, signal {:?})",
                    self.session.id(),
                    exit.code,
                    exit.signal
                );
            }
        }
        if let Some((code, message)) = self.session.last_error() {
            let needs_log = self
                .logged_error
                .as_ref()
                .is_none_or(|(c, m)| c != code || m != message);
            if needs_log {
                self.logged_error = Some((code.to_owned(), message.to_owned()));
                log::warn!("terminal {}: {code}: {message}", self.session.id());
            }
        }
        changed
    }

    fn key(&mut self, ev: &KeyEvent) {
        if self.session.is_ended() {
            return;
        }
        // Modifier-only presses and paste chords are not keys for the program.
        let Some(k) =
            keymap::terminal_key_for(Platform::host(), ev, true, keymap::OptionAsAlt::Auto)
        else {
            return;
        };
        let key = Key {
            code: k.code,
            key: k.key,
            mods: k.mods.iter().map(|m| (*m).to_string()).collect(),
            text: k.text,
            action: Some(match k.action {
                TerminalKeyAction::Press => KeyAction::Press,
                TerminalKeyAction::Repeat => KeyAction::Repeat,
                TerminalKeyAction::Release => KeyAction::Release,
            }),
            option_as_alt: match k.option_as_alt {
                keymap::OptionAsAlt::Auto => OptionAsAlt::Auto,
                keymap::OptionAsAlt::True => OptionAsAlt::True,
                keymap::OptionAsAlt::False => OptionAsAlt::False,
            },
        };
        self.session.send_key(key);
    }

    fn paste(&mut self, text: &str) {
        self.session.send_paste(text.to_owned());
    }

    fn title(&self) -> &str {
        self.session.title()
    }
}

/// Connects to `namespace` (starting its daemon if none runs), adopts every live terminal as its
/// size owner, or spawns shell `"t1"` at `NEW_COLS x NEW_ROWS` if the namespace is empty.
pub fn open_hub(
    namespace: &str,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<(SessionHub, Vec<LiveTerminal>), String> {
    let target = Target::namespace(namespace);
    let hub = SessionHub::open(&target, wake)
        .map_err(|e| format!("termd namespace `{namespace}`: {e}"))?;
    let sessions = hub
        .adopt_or_spawn_initial(NEW_COLS, NEW_ROWS)
        .map_err(|e| format!("termd adopt/spawn `{namespace}`: {e}"))?;
    let terminals = sessions
        .into_iter()
        .map(|s| LiveTerminal::new(hub.clone(), s))
        .collect();
    Ok((hub, terminals))
}

/// Connect to `namespace` (starting its daemon if none runs), take every
/// terminal it holds as their size's owner, and start a shell if it holds
/// none. `wake` is called whenever a terminal publishes a coalesced screen update.
pub fn open(
    namespace: &str,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<Vec<Box<dyn Terminal>>, String> {
    let (_hub, terminals) = open_hub(namespace, wake)?;
    Ok(terminals
        .into_iter()
        .map(|t| Box::new(t) as Box<dyn Terminal>)
        .collect())
}
