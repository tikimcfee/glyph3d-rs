//! Window-facing [`Sessions`] collection and [`News`] events matching `native/src/wall/app.rs`'s
//! seam (`out/wall-port/briefs/S-term-session.md`).
//!
//! Each adopted or spawned terminal runs its [`glyph_vt::Vt`] emulator on its own background OS
//! thread ([`TerminalSession`]). On the window thread:
//! - [`Sessions::pump`] swaps finished, frame-coalesced [`Screen`] rows into each session's
//!   window-owned `front_screen` and emits [`News`] transitions (`Resized`, `Title`, `Modes`,
//!   `Ended`, `Disconnected`).
//! - [`Sessions::screen`] returns a plain `&Screen` borrow into that window-owned `front_screen`
//!   with zero locking, valid until the next [`Sessions::pump`].
//! - Input and control calls (`key`, `paste`, `mouse`, `scroll`, `resize`, `kill`, `repaint`)
//!   dispatch onto the hub's background IO worker so socket round-trips and `kill_grace` waits
//!   never stall the window's event loop.

use std::collections::BTreeMap;
use std::sync::Arc;

use term_cells::{Modes, Screen};
use termd_client::{
    Create, Key, KeyAction, Mouse, MouseAction, MouseButton, OptionAsAlt, Target, View,
};
use wall_core::keymap::{self, TerminalKey, TerminalKeyAction};
use wall_core::mouse::MouseAction as WallMouseAction;
use wall_core::pointer::{ProgramMouseButton, TerminalMouseEvent};

use crate::hub::{DaemonEvent, HubOptions, SessionHub, SpawnOptions};
use crate::session::{ExitInfo, ScrollbackState, TerminalSession};

/// The namespace the native wall keeps its terminals in (`rs-wall`, separate from the browser
/// wall's `wall` namespace so two owners do not fight over terminal sizes).
pub const NAMESPACE: &str = "rs-wall";

/// What happened to a terminal or the daemon since the last [`Sessions::pump`], for the wall's
/// state machine (`wall_core::wall::Wall`) to hear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum News {
    /// Its screen was refilled by its emulator thread since the last [`Sessions::pump`].
    /// `rewritten_cols` holds per-row `(lo, hi)` in-place rewritten spans aligned to the
    /// published screen's row indices and trimmed of trailing `(0, 0)` rows (empty when no
    /// cells were rewritten in place).
    Filled {
        id: String,
        resized: bool,
        scrolled_top: u32,
        within_margins: bool,
        rewritten_cols: Vec<(u16, u16)>,
    },
    /// The terminal is now this size (its owner resized it).
    Resized { id: String, cols: u16, rows: u16 },
    /// The program updated its `OSC 0`/`2` window title.
    Title { id: String, title: String },
    /// The modes a host routes input by (`alt_screen`, `mouse`, `bracketed_paste`, …) changed.
    Modes { id: String, modes: Modes },
    /// Its program exited and the terminal closed: nothing more arrives.
    Ended { id: String },
    /// The daemon connection is gone for good.
    Disconnected { message: String },
}

struct TrackedSession {
    session: TerminalSession,
    reported_size: (u16, u16),
    reported_osc_title: String,
    reported_modes: Modes,
    logged_exit: bool,
    logged_error: Option<(String, String)>,
}

impl TrackedSession {
    fn new(session: TerminalSession) -> Self {
        let reported_size = (session.screen().cols, session.screen().rows);
        let reported_modes = session.modes();
        Self {
            session,
            reported_size,
            reported_osc_title: String::new(),
            reported_modes,
            logged_exit: false,
            logged_error: None,
        }
    }
}

/// All live terminals attached in one `g3d-termd` namespace, keyed by terminal `id`, with one
/// background emulator thread per terminal.
pub struct Sessions {
    hub: SessionHub,
    live: BTreeMap<String, TrackedSession>,
}

impl Sessions {
    /// Connects to `namespace`, starting its `g3d-termd` daemon if none runs. `wake` is called
    /// whenever a terminal publishes a coalesced screen update or a daemon lifecycle event arrives.
    pub fn open(namespace: &str, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        let target = Target::namespace(namespace);
        Self::open_with(&target, HubOptions::default(), wake)
            .map_err(|e| format!("termd namespace `{namespace}`: {e}"))
    }

    /// Connects to an explicit [`Target`] (such as `Target::namespace_in` in scratch tests),
    /// starting `g3d-termd` if needed.
    pub fn open_target(target: &Target, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        Self::open_with(target, HubOptions::default(), wake)
            .map_err(|e| format!("termd target: {e}"))
    }

    /// Connects to `target` with custom [`HubOptions`].
    pub fn open_with(
        target: &Target,
        options: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, termd_client::Error> {
        let hub = SessionHub::open_with(target, options, wake)?;
        Ok(Self {
            hub,
            live: BTreeMap::new(),
        })
    }

    /// Underlying [`SessionHub`].
    pub fn hub(&self) -> &SessionHub {
        &self.hub
    }

    /// Underlying [`TerminalSession`] for `id`, if currently adopted.
    pub fn session(&self, id: &str) -> Option<&TerminalSession> {
        self.live.get(id).map(|t| &t.session)
    }

    /// The ids of the terminals the daemon holds, sorted.
    pub fn list(&self) -> Result<Vec<String>, String> {
        let mut ids: Vec<String> = self
            .hub
            .list()
            .map_err(|e| format!("termd list: {e}"))?
            .into_iter()
            .map(|t| t.id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// The ids of the terminals this `Sessions` instance is currently drawing.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.live.keys().map(String::as_str)
    }

    /// Takes terminal `id` as its size owner (`Role::Own`), spawns its dedicated emulator thread,
    /// and starts drawing it. Returns its `(cols, rows)`.
    pub fn adopt(&mut self, id: &str) -> Result<(u16, u16), String> {
        let session = self
            .hub
            .attach_owner(id, None)
            .map_err(|e| format!("termd subscribe {id}: {e}"))?;
        let dims = (session.screen().cols, session.screen().rows);
        self.live.insert(id.to_owned(), TrackedSession::new(session));
        Ok(dims)
    }

    /// Starts a terminal (the account's login shell when `argv` is `None`) under the first free
    /// `tN` id, and adopts it as size owner. Returns `(id, cols, rows)`.
    pub fn spawn(
        &mut self,
        argv: Option<Vec<String>>,
        cols: u16,
        rows: u16,
    ) -> Result<(String, u16, u16), String> {
        let taken = self.list()?;
        let id = (1u64..)
            .map(|n| format!("t{n}"))
            .find(|cand| !taken.contains(cand) && !self.live.contains_key(cand))
            .expect("some tN is free");
        let made = self
            .hub
            .client()
            .create(&Create {
                id,
                argv,
                cols,
                rows,
                ..Create::default()
            })
            .map_err(|e| format!("termd create: {e}"))?;
        let (cols, rows) = self.adopt(&made.id)?;
        Ok((made.id, cols, rows))
    }

    /// Starts a terminal with full [`SpawnOptions`] (cwd, env, scrollback, colours) and adopts it.
    pub fn spawn_with(&mut self, opts: &SpawnOptions) -> Result<(String, u16, u16), String> {
        let session = self
            .hub
            .spawn(opts)
            .map_err(|e| format!("termd spawn {}: {e}", opts.id))?;
        let id = session.id().to_owned();
        let dims = (session.screen().cols, session.screen().rows);
        self.live.insert(id.clone(), TrackedSession::new(session));
        Ok((id, dims.0, dims.1))
    }

    /// Latest swapped-in [`Screen`] of terminal `id`, held directly on the window thread with zero
    /// locking until the next [`Self::pump`].
    pub fn screen(&self, id: &str) -> Option<&Screen> {
        self.live.get(id).map(|t| t.session.screen())
    }

    /// Stream selection text of a cell range of terminal `id` (`col` in `0..=cols`, end-exclusive).
    pub fn text(&self, id: &str, col0: u16, row0: u16, col1: u16, row1: u16) -> Option<String> {
        self.live
            .get(id)
            .map(|t| t.session.text(col0, row0, col1, row1))
    }

    /// Rectangular block-selection text of a cell range of terminal `id`.
    pub fn block_text(
        &self,
        id: &str,
        col0: u16,
        row0: u16,
        col1: u16,
        row1: u16,
    ) -> Option<String> {
        self.live
            .get(id)
            .map(|t| t.session.block_text(col0, row0, col1, row1))
    }

    /// Single trimmed visual row text of terminal `id`.
    pub fn row_text(&self, id: &str, row: u16) -> Option<String> {
        self.live.get(id).map(|t| t.session.row_text(row))
    }

    /// Working directory last reported via `OSC 7` for terminal `id`.
    pub fn pwd(&self, id: &str) -> Option<&str> {
        self.live.get(id).and_then(|t| t.session.pwd())
    }

    /// Current terminal modes of `id`.
    pub fn modes(&self, id: &str) -> Option<Modes> {
        self.live.get(id).map(|t| t.session.modes())
    }

    /// Current scrollback view state (`offset`, `max`, `viewing`) of `id`.
    pub fn scrollback(&self, id: &str) -> Option<ScrollbackState> {
        self.live.get(id).map(|t| t.session.scrollback())
    }

    /// Drains any unread bells rung on `id` since the previous call.
    pub fn take_bell(&mut self, id: &str) -> u64 {
        self.live.get_mut(id).map_or(0, |t| t.session.take_bell())
    }

    /// Exit status of `id`, if its process has exited.
    pub fn exit_status(&self, id: &str) -> Option<&ExitInfo> {
        self.live.get(id).and_then(|t| t.session.exit_status())
    }

    /// Swaps in any coalesced screens published by session worker threads since the last call and
    /// collects [`News`] for the wall. Never parses VT bytes on the window thread.
    ///
    /// The returned `bool` is `true` iff at least one terminal's [`Screen`] changed.
    pub fn pump(&mut self) -> (bool, Vec<News>) {
        let mut news = Vec::new();
        for ev in self.hub.poll_daemon_events() {
            match ev {
                DaemonEvent::Reconnected { restart, .. } => {
                    log::info!("termd came back (restart: {restart}); its terminals repaint");
                }
                DaemonEvent::Gone { message, .. } => {
                    news.push(News::Disconnected { message });
                }
                DaemonEvent::UpgradeFailed { message } => {
                    log::warn!("termd upgrade failed: {message}");
                }
                DaemonEvent::Status(_) => {}
            }
        }

        let mut any_screen_changed = false;
        let mut closed_ids = Vec::new();

        for (id, tracked) in &mut self.live {
            let prev_gen = tracked.session.screen().generation;
            if !tracked.session.pump() {
                continue;
            }
            if tracked.session.screen().generation != prev_gen {
                any_screen_changed = true;
            }

            let dims = (tracked.session.screen().cols, tracked.session.screen().rows);
            if dims != tracked.reported_size {
                tracked.reported_size = dims;
                news.push(News::Resized {
                    id: id.clone(),
                    cols: dims.0,
                    rows: dims.1,
                });
            }

            if let Some(delta) = tracked.session.take_fill_delta() {
                news.push(News::Filled {
                    id: id.clone(),
                    resized: delta.resized,
                    scrolled_top: delta.scrolled_top,
                    within_margins: delta.within_margins,
                    rewritten_cols: delta.rewritten_cols,
                });
            }

            if tracked.session.osc_title() != tracked.reported_osc_title {
                tracked
                    .reported_osc_title
                    .clone_from(&tracked.session.osc_title().to_owned());
                news.push(News::Title {
                    id: id.clone(),
                    title: tracked.reported_osc_title.clone(),
                });
            }

            let modes = tracked.session.modes();
            if modes != tracked.reported_modes {
                tracked.reported_modes = modes;
                news.push(News::Modes {
                    id: id.clone(),
                    modes,
                });
            }

            if !tracked.logged_exit {
                if let Some(exit) = tracked.session.exit_status() {
                    tracked.logged_exit = true;
                    log::info!(
                        "terminal {id} ended (code {:?}, signal {:?})",
                        exit.code,
                        exit.signal
                    );
                }
            }

            if let Some((code, message)) = tracked.session.last_error() {
                let needs_log = tracked
                    .logged_error
                    .as_ref()
                    .is_none_or(|(c, m)| c != code || m != message);
                if needs_log {
                    tracked.logged_error = Some((code.to_owned(), message.to_owned()));
                    log::warn!("terminal {id}: {code}: {message}");
                }
            }

            if tracked.session.is_closed() {
                closed_ids.push(id.clone());
                news.push(News::Ended { id: id.clone() });
            }
        }

        for id in closed_ids {
            self.live.remove(&id);
        }

        (any_screen_changed, news)
    }

    /// Ends terminal `id` and its process tree in `g3d-termd` without stalling the window thread.
    pub fn kill(&mut self, id: &str) {
        self.hub.send_kill(id);
    }

    /// Requests terminal `id` to resize to `cols` by `rows`; the new size arrives as
    /// [`News::Resized`].
    pub fn resize(&mut self, id: &str, cols: u16, rows: u16) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        tracked.session.send_resize(cols, rows);
    }

    /// Sends a W3C-mapped [`TerminalKey`] to `id` (encoded by `g3d-termd` for the program's active
    /// key modes).
    pub fn key(&mut self, id: &str, k: &TerminalKey) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        let key = Key {
            code: k.code.clone(),
            key: k.key.clone(),
            mods: k.mods.iter().map(|m| (*m).to_string()).collect(),
            text: k.text.clone(),
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
        tracked.session.send_key(key);
    }

    /// Pastes `text` into terminal `id` (bracketed if the program requested DECSET 2004, streamed
    /// in chunks on a background worker so large pastes never stall the window thread).
    pub fn paste(&mut self, id: &str, text: &str) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        tracked.session.send_paste(text.to_owned());
    }

    /// Sends a pointer event to a program listening for terminal mouse reporting on `id`.
    pub fn mouse(&mut self, id: &str, ev: &TerminalMouseEvent) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        let m = Mouse {
            action: match ev.action {
                WallMouseAction::Press => MouseAction::Press,
                WallMouseAction::Release => MouseAction::Release,
                WallMouseAction::Move => MouseAction::Move,
            },
            button: match ev.button {
                ProgramMouseButton::Left => MouseButton::Left,
                ProgramMouseButton::Middle => MouseButton::Middle,
                ProgramMouseButton::Right => MouseButton::Right,
                ProgramMouseButton::Back => MouseButton::Extra(8),
                ProgramMouseButton::Forward => MouseButton::Extra(9),
                ProgramMouseButton::None => MouseButton::None,
            },
            col: ev.col,
            row: ev.row,
            mods: ev.mod_names().iter().map(|m| (*m).to_string()).collect(),
        };
        tracked.session.send_mouse(m);
    }

    /// Scrolls terminal `id` by `lines` at cell `(col, row)` (`lines < 0` scrolls up into history).
    /// On the primary screen this moves this subscriber's scrollback `view` in `g3d-termd`; on the
    /// alternate screen there is no scrollback, so wheel notches at `(col, row)` are sent to the
    /// program instead.
    pub fn scroll(&mut self, id: &str, lines: i32, col: f64, row: f64) {
        if lines == 0 {
            return;
        }
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        if tracked.session.is_ended() {
            return;
        }
        if tracked.session.modes().alt_screen {
            let button = if lines < 0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            for _ in 0..lines.unsigned_abs().min(64) {
                tracked.session.send_mouse(Mouse {
                    action: MouseAction::Press,
                    button,
                    col,
                    row,
                    mods: Vec::new(),
                });
            }
        } else {
            tracked.session.send_view(View::Delta(-i64::from(lines)));
        }
    }

    /// Requests `g3d-termd` to send a fresh `REPAINT` for terminal `id`.
    pub fn repaint(&mut self, id: &str) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        tracked.session.send_repaint();
    }
}
