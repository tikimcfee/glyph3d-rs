//! Live terminals: `g3d-termd` in front of [`Terminal`].
//!
//! One connection to one termd namespace (a [`Hub`]), and one emulator per
//! terminal. The daemon sends each terminal's bytes on that one connection;
//! its reader thread queues them and wakes the event loop, and the window
//! then asks every terminal to [`Terminal::pump`]: the hub sorts what
//! arrived by terminal, each terminal feeds its own bytes to its emulator
//! and refills its screen. Keys go the other way as termd's `key` op, which
//! encodes them for whatever modes the program has set.
//!
//! The emulator runs on the window's thread for now. A flood will want it
//! on a thread of its own per terminal; the seam for that is `pump`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use glyph_vt::{Theme, Vt};
use term_cells::Screen;
use termd_client::{Create, Event, Key, KeyAction, Role, Target, Termd};
use wall_core::input::{KeyEvent, Platform};
use wall_core::keymap::{self, TerminalKeyAction};

use super::app::{Opened, Terminal};

/// The size a terminal the wall starts is born at.
pub const NEW_COLS: u16 = 100;
pub const NEW_ROWS: u16 = 30;

/// The namespace the native wall keeps its terminals in. Not `wall`: that
/// one is the browser wall's, and two owners of a terminal fight over its
/// size.
pub const NAMESPACE: &str = "rs-wall";

/// One connection to termd and what has arrived on it, sorted by terminal.
pub struct Hub {
    termd: Termd,
    inbox: HashMap<u32, Vec<Event>>,
}

impl Hub {
    /// Move everything the reader thread queued into the terminals' inboxes.
    fn sort(&mut self) {
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
                    log::error!("termd is gone: {message}");
                    continue;
                }
                Event::Status(_) | Event::UpgradeFailed { .. } => continue,
            };
            self.inbox.entry(handle).or_default().push(ev);
        }
    }
}

/// A terminal in termd, drawn here.
pub struct LiveTerminal {
    hub: Rc<RefCell<Hub>>,
    id: String,
    handle: u32,
    vt: Vt,
    screen: Screen,
    title: String,
    /// The program exited or the terminal was closed: nothing more arrives.
    ended: bool,
}

impl LiveTerminal {
    fn new(hub: Rc<RefCell<Hub>>, id: String, handle: u32, cols: u16, rows: u16) -> LiveTerminal {
        let theme = Theme::default();
        let mut vt = Vt::new(cols, rows, &theme);
        let mut screen = Screen::new(cols, rows);
        vt.fill(&mut screen);
        LiveTerminal { hub, title: id.clone(), id, handle, vt, screen, ended: false }
    }

    /// The terminal's id in termd.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Terminal for LiveTerminal {
    fn screen(&self) -> &Screen {
        &self.screen
    }

    fn pump(&mut self) -> bool {
        let events = {
            let mut hub = self.hub.borrow_mut();
            hub.sort();
            hub.inbox.remove(&self.handle).unwrap_or_default()
        };
        if events.is_empty() {
            return false;
        }
        for ev in events {
            match ev {
                Event::Output { bytes, .. } => self.vt.feed(&bytes),
                Event::Repaint { cols, rows, bytes, .. } => {
                    if (cols, rows) != (self.vt.cols(), self.vt.rows()) {
                        self.vt.resize(cols, rows);
                    }
                    self.vt.feed(&bytes);
                }
                Event::Size { cols, rows, .. } => self.vt.resize(cols, rows),
                Event::Title { title, .. } => self.title = title,
                Event::Exit { code, signal, .. } => {
                    self.ended = true;
                    log::info!("terminal {} ended (code {code:?}, signal {signal:?})", self.id);
                }
                Event::Closed { .. } => self.ended = true,
                Event::TermError { code, message, .. } => log::warn!("terminal {}: {code}: {message}", self.id),
                _ => {}
            }
        }
        self.vt.fill(&mut self.screen)
    }

    fn key(&mut self, ev: &KeyEvent) {
        if self.ended {
            return;
        }
        // Modifier-only presses and paste chords are not keys for the program.
        let Some(k) = keymap::terminal_key_for(Platform::host(), ev, true, keymap::OptionAsAlt::Auto) else { return };
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
            ..Key::default()
        };
        if let Err(e) = self.hub.borrow().termd.key(self.handle, &key) {
            log::warn!("terminal {}: key {:?} was not sent: {e}", self.id, ev.code);
        }
    }

    fn paste(&mut self, text: &str) {
        if self.ended {
            return;
        }
        if let Err(e) = self.hub.borrow().termd.paste(self.handle, text) {
            log::warn!("terminal {}: paste was not sent: {e}", self.id);
        }
    }

    fn title(&self) -> &str {
        &self.title
    }
}

/// Start one more shell in the hub's namespace, under the first free
/// `tN` id, and take it as its size's owner.
fn spawn(hub: &Rc<RefCell<Hub>>) -> Result<Box<dyn Terminal>, String> {
    let taken: Vec<String> = hub.borrow().termd.list().map_err(|e| format!("termd list: {e}"))?.into_iter().map(|t| t.id).collect();
    let id = (1..).map(|n| format!("t{n}")).find(|id| !taken.contains(id)).expect("some tN is free");
    let made = hub.borrow().termd.create(&Create { id, cols: NEW_COLS, rows: NEW_ROWS, ..Create::default() }).map_err(|e| format!("termd create: {e}"))?;
    let sub = hub.borrow().termd.subscribe(&made.id, Role::Own, None, 0).map_err(|e| format!("termd subscribe {}: {e}", made.id))?;
    Ok(Box::new(LiveTerminal::new(hub.clone(), made.id, sub.handle, sub.cols, sub.rows)))
}

/// Connect to `namespace` (starting its daemon if none runs), take every
/// terminal it holds as their size's owner, and start a shell if it holds
/// none. `wake` is called from the reader thread whenever something arrives.
pub fn open(namespace: &str, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Opened, String> {
    let target = Target::namespace(namespace);
    let termd = Termd::connect_or_start(&target, "glyph3d-wall", Box::new(move || wake())).map_err(|e| format!("termd namespace `{namespace}`: {e}"))?;
    let mut ids: Vec<String> = termd.list().map_err(|e| format!("termd list: {e}"))?.into_iter().map(|t| t.id).collect();
    if ids.is_empty() {
        let made = termd
            .create(&Create { id: "t1".to_string(), cols: NEW_COLS, rows: NEW_ROWS, ..Create::default() })
            .map_err(|e| format!("termd create: {e}"))?;
        ids.push(made.id);
    }
    ids.sort();
    let hub = Rc::new(RefCell::new(Hub { termd, inbox: HashMap::new() }));
    let mut terminals: Vec<Box<dyn Terminal>> = Vec::new();
    for id in ids {
        let sub = hub.borrow().termd.subscribe(&id, Role::Own, None, 0).map_err(|e| format!("termd subscribe {id}: {e}"))?;
        terminals.push(Box::new(LiveTerminal::new(hub.clone(), id, sub.handle, sub.cols, sub.rows)));
    }
    Ok(Opened { terminals, spawn: Some(Box::new(move || spawn(&hub))) })
}
