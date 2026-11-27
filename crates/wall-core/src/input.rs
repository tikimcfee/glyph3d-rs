//! Input as the wall's rules see it: keys, modifiers and pointer buttons
//! with no window system in them. A key is named as the web names it
//! (`KeyboardEvent.code` and `.key`), which is also what termd's `key` op
//! takes and what winit's key names are, so one event serves all three.

/// The modifier keys held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Command on macOS, the Super/Windows key elsewhere.
    pub meta: bool,
}

/// Which conventions apply: the wall's chord is Command on macOS and
/// Ctrl+Alt elsewhere, and a few gestures differ the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    Mac,
    Other,
}

impl Platform {
    /// The platform this binary was built for.
    pub const fn host() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Other
        }
    }
}

/// One key event.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct KeyEvent {
    /// The physical key (`KeyboardEvent.code`): `"KeyA"`, `"Enter"`, `"BracketLeft"`.
    pub code: String,
    /// What the key means under the layout and modifiers (`KeyboardEvent.key`):
    /// `"a"`, `"A"`, `"Enter"`, `"ArrowUp"`.
    pub key: String,
    /// The text the press types, if any.
    pub text: Option<String>,
    pub mods: Mods,
    pub repeat: bool,
    /// False for a release.
    pub pressed: bool,
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}
