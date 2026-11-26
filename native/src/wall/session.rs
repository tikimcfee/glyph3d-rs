//! Live terminals: `g3d-termd` in front of the wall.
//!
//! [`Sessions`] is one connection to one termd namespace and one emulator
//! per terminal, keyed by the terminal's id in termd, which is also its id
//! on the wall. The daemon sends every terminal's bytes on that one
//! connection; its reader thread queues them and wakes the event loop, and
//! the window then calls [`Sessions::pump`]: each terminal's bytes go to its
//! emulator, its screen is refilled, and what the wall must hear (a size, a
//! title, a mode, an end) comes back as [`News`].
//!
//! Everything the wall asks of a terminal goes out here as a termd request:
//! spawn, kill, resize, a key (termd encodes it for the program's modes),
//! paste, a mouse event, a scroll of the view, a repaint.
//!
//! The emulator runs on the window's thread for now; `crates/term-session`
//! is the same surface with a thread per terminal.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use glyph_vt::{Theme, Vt};
use term_cells::{Modes, Screen};
use termd_client::{Create, Event, Key, KeyAction, Mouse, MouseAction, MouseButton, Role, Target, Termd, View};
use wall_core::keymap::{TerminalKey, TerminalKeyAction};
use wall_core::mouse::MouseAction as WallMouseAction;
use wall_core::pointer::{ProgramMouseButton, TerminalMouseEvent};

/// The namespace the native wall keeps its terminals in. Not `wall`: that
/// one is the browser wall's, and two owners of a terminal fight over its
/// size.
pub const NAMESPACE: &str = "rs-wall";

/// What happened to a terminal, for the wall to hear.
#[derive(Clone, Debug, PartialEq)]
pub enum News {
    /// The terminal is now this size (its owner resized it).
    Resized { id: String, cols: u16, rows: u16 },
    Title { id: String, title: String },
    /// The modes a host routes input by changed.
    Modes { id: String, modes: Modes },
    /// Its program exited or it was closed: nothing more arrives.
    Ended { id: String },
    /// The daemon is gone for good.
    Disconnected { message: String },
}

/// One terminal: its emulator and the screen it fills.
struct Live {
    handle: u32,
    vt: Vt,
    screen: Screen,
    modes: Modes,
    ended: bool,
}

/// The terminals of one termd namespace.
pub struct Sessions {
    termd: Termd,
    live: BTreeMap<String, Live>,
    by_handle: HashMap<u32, String>,
}

impl Sessions {
    /// Connect to `namespace`, starting its daemon if none runs. `wake` is
    /// called from the reader thread whenever something arrives.
    pub fn open(namespace: &str, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Sessions, String> {
        let target = Target::namespace(namespace);
        let termd = Termd::connect_or_start(&target, "glyph3d-wall", Box::new(move || wake())).map_err(|e| format!("termd namespace `{namespace}`: {e}"))?;
        Ok(Sessions { termd, live: BTreeMap::new(), by_handle: HashMap::new() })
    }

    /// The ids of the terminals the daemon holds, sorted.
    pub fn list(&self) -> Result<Vec<String>, String> {
        let mut ids: Vec<String> = self.termd.list().map_err(|e| format!("termd list: {e}"))?.into_iter().map(|t| t.id).collect();
        ids.sort();
        Ok(ids)
    }

    /// The ids of the terminals this window is drawing.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.live.keys().map(String::as_str)
    }

    /// Take terminal `id` as its size's owner and start drawing it. Returns
    /// its size.
    pub fn adopt(&mut self, id: &str) -> Result<(u16, u16), String> {
        let sub = self.termd.subscribe(id, Role::Own, None, 0).map_err(|e| format!("termd subscribe {id}: {e}"))?;
        let mut vt = Vt::new(sub.cols, sub.rows, &Theme::default());
        let mut screen = Screen::new(sub.cols, sub.rows);
        vt.fill(&mut screen);
        let modes = screen.modes;
        self.by_handle.insert(sub.handle, id.to_string());
        self.live.insert(id.to_string(), Live { handle: sub.handle, vt, screen, modes, ended: false });
        Ok((sub.cols, sub.rows))
    }

    /// Start a terminal (the account's shell when `argv` is None) under the
    /// first free `tN` id, and adopt it. Returns its id and size.
    pub fn spawn(&mut self, argv: Option<Vec<String>>, cols: u16, rows: u16) -> Result<(String, u16, u16), String> {
        let taken = self.list()?;
        let id = (1..).map(|n| format!("t{n}")).find(|id| !taken.contains(id)).expect("some tN is free");
        let made = self.termd.create(&Create { id, argv, cols, rows, ..Create::default() }).map_err(|e| format!("termd create: {e}"))?;
        let (cols, rows) = self.adopt(&made.id)?;
        Ok((made.id, cols, rows))
    }

    /// The screen of terminal `id`, to draw or to read text from.
    pub fn screen(&self, id: &str) -> Option<&Screen> {
        self.live.get(id).map(|l| &l.screen)
    }

    /// Text of a cell range of terminal `id` (selection, copy).
    pub fn text(&self, id: &str, col0: u16, row0: u16, col1: u16, row1: u16) -> Option<String> {
        self.live.get(id).map(|l| l.vt.text(col0, row0, col1, row1))
    }

    /// Take in what arrived since the last call: feed each terminal's
    /// emulator, refill its screen, and say what the wall must hear. The
    /// bool is whether any screen changed.
    pub fn pump(&mut self) -> (bool, Vec<News>) {
        let mut news = Vec::new();
        let mut fed: Vec<String> = Vec::new();
        while let Ok(ev) = self.termd.events().try_recv() {
            let handle = match &ev {
                Event::Output { handle, .. }
                | Event::Repaint { handle, .. }
                | Event::Size { handle, .. }
                | Event::Title { handle, .. }
                | Event::Modes { handle, .. }
                | Event::Bell { handle }
                | Event::Pwd { handle, .. }
                | Event::Role { handle, .. }
                | Event::Lagged { handle, .. }
                | Event::Exit { handle, .. }
                | Event::Closed { handle }
                | Event::TermError { handle, .. } => *handle,
                Event::Reconnected { restart, .. } => {
                    log::info!("termd came back (restart: {restart}); its terminals repaint");
                    continue;
                }
                Event::Disconnected { message, .. } => {
                    news.push(News::Disconnected { message: message.clone() });
                    continue;
                }
                Event::Status(_) | Event::UpgradeFailed { .. } => continue,
            };
            let Some(id) = self.by_handle.get(&handle).cloned() else { continue };
            let Some(live) = self.live.get_mut(&id) else { continue };
            match ev {
                Event::Output { bytes, .. } => {
                    live.vt.feed(&bytes);
                    fed.push(id);
                }
                Event::Repaint { cols, rows, bytes, .. } => {
                    if (cols, rows) != (live.vt.cols(), live.vt.rows()) {
                        live.vt.resize(cols, rows);
                        news.push(News::Resized { id: id.clone(), cols, rows });
                    }
                    live.vt.feed(&bytes);
                    fed.push(id);
                }
                Event::Size { cols, rows, .. } => {
                    live.vt.resize(cols, rows);
                    news.push(News::Resized { id: id.clone(), cols, rows });
                    fed.push(id);
                }
                Event::Title { title, .. } => news.push(News::Title { id, title }),
                Event::Exit { code, signal, .. } => {
                    live.ended = true;
                    log::info!("terminal {id} ended (code {code:?}, signal {signal:?})");
                }
                Event::Closed { .. } => {
                    live.ended = true;
                    news.push(News::Ended { id });
                }
                Event::TermError { code, message, .. } => log::warn!("terminal {id}: {code}: {message}"),
                _ => {}
            }
        }
        let mut changed = false;
        fed.sort();
        fed.dedup();
        for id in fed {
            let Some(live) = self.live.get_mut(&id) else { continue };
            changed |= live.vt.fill(&mut live.screen);
            if live.screen.modes != live.modes {
                live.modes = live.screen.modes;
                news.push(News::Modes { id, modes: live.modes });
            }
        }
        for n in &news {
            if let News::Ended { id } = n {
                if let Some(live) = self.live.remove(id) {
                    self.by_handle.remove(&live.handle);
                }
            }
        }
        (changed, news)
    }

    fn handle(&self, id: &str) -> Option<u32> {
        self.live.get(id).filter(|l| !l.ended).map(|l| l.handle)
    }

    /// End terminal `id` and its processes.
    pub fn kill(&mut self, id: &str) {
        if let Err(e) = self.termd.kill(id) {
            log::warn!("terminal {id}: kill was refused: {e}");
        }
    }

    /// Ask for terminal `id` to be `cols` by `rows`. The new size arrives as
    /// [`News::Resized`].
    pub fn resize(&mut self, id: &str, cols: u16, rows: u16) {
        let Some(handle) = self.handle(id) else { return };
        if let Err(e) = self.termd.resize(handle, cols, rows) {
            log::warn!("terminal {id}: resize to {cols}x{rows} was refused: {e}");
        }
    }

    /// A key for the terminal's program; termd encodes it.
    pub fn key(&mut self, id: &str, k: &TerminalKey) {
        let Some(handle) = self.handle(id) else { return };
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
            ..Key::default()
        };
        if let Err(e) = self.termd.key(handle, &key) {
            log::warn!("terminal {id}: key {:?} was not sent: {e}", k.code);
        }
    }

    /// Text pasted into the terminal (bracketed if the program asked).
    pub fn paste(&mut self, id: &str, text: &str) {
        let Some(handle) = self.handle(id) else { return };
        if let Err(e) = self.termd.paste(handle, text) {
            log::warn!("terminal {id}: paste was not sent: {e}");
        }
    }

    /// A pointer event for a program that hears the mouse.
    pub fn mouse(&mut self, id: &str, ev: &TerminalMouseEvent) {
        let Some(handle) = self.handle(id) else { return };
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
        if let Err(e) = self.termd.mouse(handle, &m) {
            log::warn!("terminal {id}: mouse event was not sent: {e}");
        }
    }

    /// Scroll terminal `id` by `lines` at cell (`col`, `row`); negative is
    /// up, into history. On the primary screen this moves our view into
    /// termd's scrollback; on the alternate screen there is none, so the
    /// program gets wheel notches at that cell instead (a pager turns them
    /// into its own scrolling).
    pub fn scroll(&mut self, id: &str, lines: i32, col: f64, row: f64) {
        let Some(live) = self.live.get(id).filter(|l| !l.ended) else { return };
        let handle = live.handle;
        if live.modes.alt_screen {
            let button = if lines < 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..lines.unsigned_abs().min(64) {
                let m = Mouse { action: MouseAction::Press, button, col, row, mods: Vec::new() };
                if let Err(e) = self.termd.mouse(handle, &m) {
                    log::warn!("terminal {id}: wheel was not sent: {e}");
                    break;
                }
            }
        } else if let Err(e) = self.termd.view(handle, View::Delta(-i64::from(lines))) {
            log::debug!("terminal {id}: view did not move: {e}");
        }
    }

    /// Ask termd to redraw terminal `id` from its own screen.
    pub fn repaint(&mut self, id: &str) {
        let Some(handle) = self.handle(id) else { return };
        if let Err(e) = self.termd.repaint(handle) {
            log::warn!("terminal {id}: repaint was refused: {e}");
        }
    }
}
