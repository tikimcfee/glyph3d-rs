//! The winit boundary for the wall's keys: one translation from a winit key
//! event to `wall_core::input::KeyEvent`, which names a key as the web does
//! (`KeyboardEvent.code` and `.key`). That is also what termd's `key` op
//! takes, so the event goes to a terminal unchanged.
//!
//! winit's `KeyCode` and `NamedKey` are named after the same W3C values, so
//! their `Debug` names are the strings, with two exceptions handled here:
//! winit says `Super` where the web says `Meta`, and the space bar's `key`
//! is the character `" "`, not a name.

use wall_core::config::OptionAsAlt;
use wall_core::input::{KeyEvent, Mods};
use winit::event::ElementState;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

/// The modifiers held, from winit's state.
pub fn mods(state: ModifiersState) -> Mods {
    Mods { shift: state.shift_key(), ctrl: state.control_key(), alt: state.alt_key(), meta: state.super_key() }
}

/// Effective [`Mods`] for a specific key press or release.
///
/// On macOS, winit queues a modifier key's own `KeyboardInput` *before* its
/// `ModifiersChanged` event (`WinitView::update_modifiers`), so `modifiers` still
/// carries the pre-transition bit. Reflecting the modifier key's own press/release
/// state matches W3C `KeyboardEvent` (`keyup` of `Meta` has `metaKey == false`,
/// cancelling `⌘X` close-hold; `keydown` of `Control` has `ctrlKey == true` for
/// mid-drag snap inversion).
fn mods_for_key(
    physical_key: PhysicalKey,
    logical_key: &Key,
    state: ElementState,
    modifiers: ModifiersState,
) -> Mods {
    let mut m = mods(modifiers);
    let pressed = state == ElementState::Pressed;
    match (physical_key, logical_key) {
        (PhysicalKey::Code(KeyCode::ShiftLeft | KeyCode::ShiftRight), _)
        | (_, Key::Named(NamedKey::Shift)) => m.shift = pressed,
        (PhysicalKey::Code(KeyCode::ControlLeft | KeyCode::ControlRight), _)
        | (_, Key::Named(NamedKey::Control)) => m.ctrl = pressed,
        (PhysicalKey::Code(KeyCode::AltLeft | KeyCode::AltRight), _)
        | (_, Key::Named(NamedKey::Alt)) => m.alt = pressed,
        (PhysicalKey::Code(KeyCode::SuperLeft | KeyCode::SuperRight), _)
        | (_, Key::Named(NamedKey::Super | NamedKey::Meta)) => m.meta = pressed,
        _ => {}
    }
    m
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

fn non_control_text(s: &str) -> Option<String> {
    (!s.is_empty() && !s.chars().any(char::is_control)).then(|| s.to_string())
}

/// What the press typed, as termd wants it: the character without
/// Control's change, never a control character. A named key (Enter, Tab,
/// an arrow) types nothing, except the space bar.
fn text_of(logical: &Key, text: Option<&str>) -> Option<String> {
    match logical {
        Key::Named(NamedKey::Space) => Some(" ".to_string()),
        Key::Character(s) => text.and_then(non_control_text).or_else(|| non_control_text(s)),
        Key::Unidentified(_) => text.and_then(non_control_text),
        _ => None,
    }
}

/// Build a [`KeyEvent`] from physical/logical key parts and winit modifiers.
pub fn key_from_parts(
    physical_key: PhysicalKey,
    logical_key: &Key,
    text: Option<&str>,
    state: ElementState,
    repeat: bool,
    modifiers: ModifiersState,
) -> KeyEvent {
    let typed = text_of(logical_key, text);
    let key = match (logical_key, &typed) {
        (Key::Unidentified(_), Some(t)) => t.clone(),
        _ => key_of(logical_key),
    };
    KeyEvent {
        code: code_of(physical_key),
        key,
        text: typed,
        mods: mods_for_key(physical_key, logical_key, state, modifiers),
        repeat,
        pressed: state == ElementState::Pressed,
    }
}

/// One winit key event as the wall's, resolving the unmodified key on macOS when
/// `option_as_alt` is [`OptionAsAlt::True`] even before `WindowExtMacOS::set_option_as_alt`
/// has drained on the main thread.
pub fn key_event(
    ev: &winit::event::KeyEvent,
    modifiers: ModifiersState,
    option_as_alt: OptionAsAlt,
) -> KeyEvent {
    #[cfg(target_os = "macos")]
    if option_as_alt == OptionAsAlt::True
        && modifiers.alt_key()
        && !modifiers.control_key()
        && !modifiers.super_key()
    {
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        if let Key::Character(base) = ev.key_without_modifiers() {
            let needs_unmodified = matches!(ev.logical_key, Key::Dead(_))
                || (base.is_ascii()
                    && matches!(&ev.logical_key, Key::Character(c) if !c.is_ascii()));
            if needs_unmodified && !base.is_empty() && !base.chars().any(char::is_control) {
                let unmod_str = if modifiers.shift_key() {
                    base.chars().flat_map(char::to_uppercase).collect::<String>()
                } else {
                    base.to_string()
                };
                let unmod_key = Key::Character(winit::keyboard::SmolStr::new(&unmod_str));
                let unmod_text = (ev.state == ElementState::Pressed).then_some(unmod_str.as_str());
                return key_from_parts(
                    ev.physical_key,
                    &unmod_key,
                    unmod_text,
                    ev.state,
                    ev.repeat,
                    modifiers,
                );
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = option_as_alt;

    key_from_parts(
        ev.physical_key,
        &ev.logical_key,
        ev.text.as_deref(),
        ev.state,
        ev.repeat,
        modifiers,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{NativeKey, NativeKeyCode, SmolStr};

    #[test]
    fn keys_take_their_web_names() {
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::KeyA)), "KeyA");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::ArrowLeft)), "ArrowLeft");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::SuperLeft)), "MetaLeft");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::SuperRight)), "MetaRight");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::NumpadEnter)), "NumpadEnter");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::Numpad0)), "Numpad0");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::F1)), "F1");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::F20)), "F20");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::Home)), "Home");
        assert_eq!(code_of(PhysicalKey::Code(KeyCode::PageDown)), "PageDown");
        assert_eq!(key_of(&Key::Named(NamedKey::Enter)), "Enter");
        assert_eq!(key_of(&Key::Named(NamedKey::Space)), " ");
        assert_eq!(key_of(&Key::Named(NamedKey::Super)), "Meta");
        assert_eq!(key_of(&Key::Character(SmolStr::new("A"))), "A");
        assert_eq!(key_of(&Key::Dead(Some('´'))), "Dead");
    }

    #[test]
    fn text_is_the_character_never_a_control_code() {
        let c = Key::Character(SmolStr::new("c"));
        assert_eq!(text_of(&c, Some("c")).as_deref(), Some("c"));
        // Ctrl+C: winit hands the control character; termd wants "c".
        assert_eq!(text_of(&c, Some("\u{3}")).as_deref(), Some("c"));
        // Shift / CapsLock: uppercase character rides in both logical key and text.
        let big_a = Key::Character(SmolStr::new("A"));
        assert_eq!(text_of(&big_a, Some("\u{1}")).as_deref(), Some("A"));
        // Control char in Character fallback is never forwarded as text.
        let del = Key::Character(SmolStr::new("\u{7f}"));
        assert_eq!(text_of(&del, Some("\u{7f}")), None);
        assert_eq!(text_of(&Key::Named(NamedKey::Enter), Some("\r")), None);
        assert_eq!(text_of(&Key::Named(NamedKey::Space), Some(" ")).as_deref(), Some(" "));
        assert_eq!(text_of(&Key::Dead(Some('´')), None), None);
    }

    #[test]
    fn unidentified_logical_key_preserves_printable_text() {
        let ev = key_from_parts(
            PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
            &Key::Unidentified(NativeKey::Unidentified),
            Some("é"),
            ElementState::Pressed,
            false,
            ModifiersState::empty(),
        );
        assert_eq!(ev.key, "é");
        assert_eq!(ev.text.as_deref(), Some("é"));
    }

    #[test]
    fn modifier_press_and_release_reflect_their_own_bit_immediately() {
        // On macOS, SuperLeft release arrives while ModifiersState still has SUPER set.
        let up = key_from_parts(
            PhysicalKey::Code(KeyCode::SuperLeft),
            &Key::Named(NamedKey::Super),
            None,
            ElementState::Released,
            false,
            ModifiersState::SUPER,
        );
        assert_eq!(up.code, "MetaLeft");
        assert_eq!(up.key, "Meta");
        assert!(!up.mods.meta, "releasing Meta must clear mods.meta before ModifiersChanged");

        // Pressing ControlLeft sets mods.ctrl before ModifiersChanged arrives.
        let down = key_from_parts(
            PhysicalKey::Code(KeyCode::ControlLeft),
            &Key::Named(NamedKey::Control),
            None,
            ElementState::Pressed,
            false,
            ModifiersState::empty(),
        );
        assert_eq!(down.key, "Control");
        assert!(down.mods.ctrl);
    }

    #[test]
    fn numpad_and_navigation_keys_match_termd_protocol() {
        let kp_enter = key_from_parts(
            PhysicalKey::Code(KeyCode::NumpadEnter),
            &Key::Named(NamedKey::Enter),
            None,
            ElementState::Pressed,
            false,
            ModifiersState::empty(),
        );
        assert_eq!(kp_enter.code, "NumpadEnter");
        assert_eq!(kp_enter.key, "Enter");
        assert_eq!(kp_enter.text, None);

        let kp_8_nav = key_from_parts(
            PhysicalKey::Code(KeyCode::Numpad8),
            &Key::Named(NamedKey::ArrowUp),
            None,
            ElementState::Pressed,
            true,
            ModifiersState::empty(),
        );
        assert_eq!(kp_8_nav.code, "Numpad8");
        assert_eq!(kp_8_nav.key, "ArrowUp");
        assert_eq!(kp_8_nav.text, None);
        assert!(kp_8_nav.repeat);
    }
}
