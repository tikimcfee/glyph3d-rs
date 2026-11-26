//! The winit boundary for the wall's keys: one translation from a winit key
//! event to `wall_core::input::KeyEvent`, which names a key as the web does
//! (`KeyboardEvent.code` and `.key`). That is also what termd's `key` op
//! takes, so the event goes to a terminal unchanged.
//!
//! winit's `KeyCode` and `NamedKey` are named after the same W3C values, so
//! their `Debug` names are the strings, with two exceptions handled here:
//! winit says `Super` where the web says `Meta`, and the space bar's `key`
//! is the character `" "`, not a name.

use wall_core::input::{KeyEvent, Mods};
use winit::event::ElementState;
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};

/// The modifiers held, from winit's state.
pub fn mods(state: ModifiersState) -> Mods {
    Mods { shift: state.shift_key(), ctrl: state.control_key(), alt: state.alt_key(), meta: state.super_key() }
}

/// The W3C `code` of a physical key; empty when winit could not identify it.
pub fn code_of(key: PhysicalKey) -> String {
    match key {
        PhysicalKey::Code(code) => match format!("{code:?}").as_str() {
            "SuperLeft" => "MetaLeft".to_string(),
            "SuperRight" => "MetaRight".to_string(),
            other => other.to_string(),
        },
        PhysicalKey::Unidentified(_) => String::new(),
    }
}

/// The W3C `key` of a logical key.
pub fn key_of(key: &Key) -> String {
    match key {
        Key::Named(NamedKey::Space) => " ".to_string(),
        Key::Named(NamedKey::Super) => "Meta".to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Character(s) => s.to_string(),
        Key::Dead(_) => "Dead".to_string(),
        Key::Unidentified(_) => "Unidentified".to_string(),
    }
}

/// What the press typed, as termd wants it: the character without
/// Control's change, never a control character. A named key (Enter, Tab,
/// an arrow) types nothing, except the space bar.
fn text_of(logical: &Key, text: Option<&str>) -> Option<String> {
    match logical {
        Key::Named(NamedKey::Space) => Some(" ".to_string()),
        Key::Character(s) => match text {
            Some(t) if !t.is_empty() && !t.chars().any(char::is_control) => Some(t.to_string()),
            // Control held: winit's text is the control character; the
            // key's own character is what the protocol asks for.
            _ => Some(s.to_string()),
        },
        _ => None,
    }
}

/// One winit key event as the wall's.
pub fn key_event(ev: &winit::event::KeyEvent, modifiers: ModifiersState) -> KeyEvent {
    KeyEvent {
        code: code_of(ev.physical_key),
        key: key_of(&ev.logical_key),
        text: text_of(&ev.logical_key, ev.text.as_deref()),
        mods: mods(modifiers),
        repeat: ev.repeat,
        pressed: ev.state == ElementState::Pressed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{KeyCode, SmolStr};

    #[test]
    fn keys_take_their_web_names() {
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::KeyA)), "KeyA");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::ArrowLeft)), "ArrowLeft");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::SuperLeft)), "MetaLeft");
        assert_eq!(key_of(&Key::Named(NamedKey::Enter)), "Enter");
        assert_eq!(key_of(&Key::Named(NamedKey::Space)), " ");
        assert_eq!(key_of(&Key::Named(NamedKey::Super)), "Meta");
        assert_eq!(key_of(&Key::Character(SmolStr::new("A"))), "A");
    }

    #[test]
    fn text_is_the_character_never_a_control_code() {
        let c = Key::Character(SmolStr::new("c"));
        assert_eq!(text_of(&c, Some("c")).as_deref(), Some("c"));
        // Ctrl+C: winit hands the control character; termd wants "c".
        assert_eq!(text_of(&c, Some("\u{3}")).as_deref(), Some("c"));
        assert_eq!(text_of(&Key::Named(NamedKey::Enter), Some("\r")), None);
        assert_eq!(text_of(&Key::Named(NamedKey::Space), Some(" ")).as_deref(), Some(" "));
    }
}
