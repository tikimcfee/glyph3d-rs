//! The makepad boundary for input: the one translation from a makepad key
//! to the engine's `SceneKey` (the twin of `windowed::app::scene_key`), and
//! the host's own keys (the panel toggles). A key the scene does not act on
//! is None and never reaches it. Mouse-look's keys (backquote, Escape) are
//! the viewport's (`viewport::Glyph3dViewport`).

use glyph3d_native::scene::SceneKey;
use makepad_widgets::KeyCode;

use crate::ui_model::{G3Panel, G3UiAction};

/// The scene's keys by physical position: the fly camera's WASD/E R/Q F,
/// the verb keys, the deck and page keys.
pub fn scene_key(code: KeyCode) -> Option<SceneKey> {
    Some(match code {
        KeyCode::KeyW => SceneKey::W,
        KeyCode::KeyA => SceneKey::A,
        KeyCode::KeyS => SceneKey::S,
        KeyCode::KeyD => SceneKey::D,
        KeyCode::KeyE => SceneKey::E,
        KeyCode::KeyR => SceneKey::R,
        KeyCode::KeyQ => SceneKey::Q,
        KeyCode::KeyF => SceneKey::F,
        KeyCode::KeyH => SceneKey::H,
        KeyCode::KeyT => SceneKey::T,
        KeyCode::KeyX => SceneKey::X,
        KeyCode::KeyB => SceneKey::B,
        KeyCode::KeyG => SceneKey::G,
        KeyCode::KeyC => SceneKey::C,
        KeyCode::KeyV => SceneKey::V,
        KeyCode::KeyN => SceneKey::N,
        KeyCode::KeyP => SceneKey::P,
        KeyCode::LBracket => SceneKey::BracketLeft,
        KeyCode::RBracket => SceneKey::BracketRight,
        KeyCode::ArrowLeft => SceneKey::ArrowLeft,
        KeyCode::ArrowRight => SceneKey::ArrowRight,
        KeyCode::Comma => SceneKey::Comma,
        KeyCode::Period => SceneKey::Period,
        KeyCode::Home => SceneKey::Home,
        KeyCode::End => SceneKey::End,
        _ => return None,
    })
}

/// The keys the host keeps for itself, never the scene's, as the panel
/// actions they stand for: F8 the field HUD, F1 the Debug panel, F7 the
/// agent sessions browser, F6 the canvas layout (a rebuild). They work
/// wherever key focus is.
pub fn host_key(code: KeyCode) -> Option<G3UiAction> {
    Some(match code {
        KeyCode::F8 => G3UiAction::TogglePanel(G3Panel::Hud),
        KeyCode::F1 => G3UiAction::TogglePanel(G3Panel::Debug),
        KeyCode::F7 => G3UiAction::TogglePanel(G3Panel::Sessions),
        KeyCode::F6 => G3UiAction::ToggleLayout,
        _ => return None,
    })
}
