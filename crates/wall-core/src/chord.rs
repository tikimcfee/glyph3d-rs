//! Platform chord rules, modifier predicates, and keybinding labels.
//!
//! Ports `wall/chord.js` (plus `isPasteChord` from
//! `packages/glyph3d-core/src/services/interaction/keyEncoding.js`).
//!
//! The wall chord modifier is decided once by [`Platform`]: `⌘` (`Meta`) on macOS,
//! and `Ctrl+Alt` elsewhere (`Ctrl` alone belongs to the shell for `^C`, `^R`, `^[`,
//! and `Alt` alone is readline's Meta). This module also decides the snap-inversion
//! modifier ([`inverts_snap`]), wheel-zoom modifiers ([`parse_zoom_keys`],
//! [`zooms_wheel`], [`zoom_wheel_label`]), copy and paste chords ([`is_copy_chord`],
//! [`is_paste_chord`]), focus-history and group-cycling shortcuts
//! ([`focus_history_dir`], [`group_cycle_dir`]), rectangular block-selection
//! modifiers ([`block_selects`]), and human-readable labels for the help panel,
//! menus, and palette ([`chord_label`] and the `*_label` functions).

use crate::input::{KeyEvent, Mods, Platform};

/// Direction along an ordered ring or history stack: backward (`-1`) or forward (`+1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CycleDir {
    /// Step backward (`-1` in the JS wall).
    Prev = -1,
    /// Step forward (`+1` in the JS wall).
    Next = 1,
}

impl CycleDir {
    /// Signed step (`-1` or `1`), matching the JS wall's `-1 | 1` return convention.
    pub const fn step(self) -> i32 {
        self as i32
    }
}

/// Modifier key that can make a pointer wheel zoom the canvas instead of panning it
/// (configured by the `zoomKeys` knob).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ZoomKey {
    Ctrl,
    Alt,
    Meta,
    Shift,
}

impl ZoomKey {
    /// Canonical lowercase token used in the `zoomKeys` setting string.
    pub const fn as_str(self) -> &'static str {
        match self {
            ZoomKey::Ctrl => "ctrl",
            ZoomKey::Alt => "alt",
            ZoomKey::Meta => "meta",
            ZoomKey::Shift => "shift",
        }
    }

    /// Platform display glyph or name for this modifier (`⌃`/`⌥`/`⌘`/`⇧` on macOS,
    /// `Ctrl`/`Alt`/`Super`/`Shift` elsewhere).
    pub const fn label(self, platform: Platform) -> &'static str {
        match (platform, self) {
            (Platform::Mac, ZoomKey::Ctrl) => "⌃",
            (Platform::Mac, ZoomKey::Alt) => "⌥",
            (Platform::Mac, ZoomKey::Meta) => "⌘",
            (Platform::Mac, ZoomKey::Shift) => "⇧",
            (Platform::Other, ZoomKey::Ctrl) => "Ctrl",
            (Platform::Other, ZoomKey::Alt) => "Alt",
            (Platform::Other, ZoomKey::Meta) => "Super",
            (Platform::Other, ZoomKey::Shift) => "Shift",
        }
    }

    /// Whether this modifier is held in `mods`.
    pub const fn held_in(self, mods: Mods) -> bool {
        match self {
            ZoomKey::Ctrl => mods.ctrl,
            ZoomKey::Alt => mods.alt,
            ZoomKey::Meta => mods.meta,
            ZoomKey::Shift => mods.shift,
        }
    }
}

/// Whether `platform` uses macOS `⌘` chord conventions (`IS_MAC` in `chord.js`).
pub const fn is_mac(platform: Platform) -> bool {
    matches!(platform, Platform::Mac)
}

/// The chord's modifier key names (`["Meta"]` on macOS, `["Control", "Alt"]` elsewhere;
/// `CHORD_KEYS` in `chord.js`).
pub const fn chord_keys(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::Mac => &["Meta"],
        Platform::Other => &["Control", "Alt"],
    }
}

/// [`Mods`] carrying the platform's wall chord (with `shift: false`).
pub const fn chord_mods(platform: Platform) -> Mods {
    match platform {
        Platform::Mac => Mods { shift: false, ctrl: false, alt: false, meta: true },
        Platform::Other => Mods { shift: false, ctrl: true, alt: true, meta: false },
    }
}

/// Display name for the drag-to-move modifier (`⌥` on macOS, `Alt` elsewhere; `ALT_LABEL`).
pub const fn alt_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌥",
        Platform::Other => "Alt",
    }
}

/// Whether `Ctrl` (`⌃` on macOS) is held to invert grid snapping during an active move,
/// resize, or group drag (`invertsSnap` in `chord.js`).
///
/// Neither `Alt` (drag-to-move), `Shift` (lasso / `⇧`-click), nor the wall chord uses bare
/// `Ctrl`. On macOS `⌃` at pointer-down is secondary click, so the drag reads it while
/// already under way rather than at press.
pub const fn inverts_snap(mods: Mods) -> bool {
    mods.ctrl
}

/// Display name for the snap-inversion modifier (`⌃` on macOS, `Ctrl` elsewhere; `SNAP_INVERT_LABEL`).
pub const fn snap_invert_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌃",
        Platform::Other => "Ctrl",
    }
}

/// Parse and validate the `zoomKeys` setting string: space-separated words from
/// `ctrl`, `alt`, `meta`, `shift` (empty or whitespace-only yields an empty list).
///
/// Returns `Err` with the same diagnostic message as `chord.js`'s `parseZoomKeys` if any
/// token is not one of the four modifier names.
pub fn parse_zoom_keys(zoom_keys: &str) -> Result<Vec<ZoomKey>, String> {
    let mut keys = Vec::new();
    for word in zoom_keys.split_whitespace() {
        let key = match word {
            "ctrl" => ZoomKey::Ctrl,
            "alt" => ZoomKey::Alt,
            "meta" => ZoomKey::Meta,
            "shift" => ZoomKey::Shift,
            _ => return Err(format!("\"{word}\" is not a modifier (ctrl, alt, meta, shift)")),
        };
        keys.push(key);
    }
    Ok(keys)
}

/// Whether a wheel event with `mods` zooms the canvas rather than panning it, given
/// already-parsed [`ZoomKey`]s.
pub fn zooms_wheel_keys(mods: Mods, keys: &[ZoomKey]) -> bool {
    keys.iter().any(|k| k.held_in(mods))
}

/// Whether a wheel event with `mods` zooms the canvas under the `zoomKeys` setting string
/// (`zoomsWheel` in `chord.js`).
///
/// `ctrl` covers trackpad pinch gestures where browsers synthesize `Ctrl+wheel`; `alt` and
/// `meta` cover remote-desktop sessions from macOS where Option arrives as `Alt` or `⌘` as `Meta`.
pub fn zooms_wheel(mods: Mods, zoom_keys: &str) -> Result<bool, String> {
    let keys = parse_zoom_keys(zoom_keys)?;
    Ok(zooms_wheel_keys(mods, &keys))
}

/// Format already-parsed [`ZoomKey`]s as the help panel names them (`⌃/⌥/⌘` or
/// `Ctrl/Alt/Super`, or `"no key"` when empty).
pub fn zoom_wheel_label_keys(platform: Platform, keys: &[ZoomKey]) -> String {
    if keys.is_empty() {
        return "no key".to_string();
    }
    keys.iter().map(|k| k.label(platform)).collect::<Vec<_>>().join("/")
}

/// Format the `zoomKeys` setting string as the help panel names it (`zoomWheelLabel` in `chord.js`).
pub fn zoom_wheel_label(platform: Platform, zoom_keys: &str) -> Result<String, String> {
    let keys = parse_zoom_keys(zoom_keys)?;
    Ok(zoom_wheel_label_keys(platform, &keys))
}

/// Whether `event` is the platform's copy chord (`⌘C` on macOS, `Ctrl+Shift+C` elsewhere;
/// `isCopyChord` in `chord.js`).
///
/// Plain `Ctrl+C` is never a copy chord: it always reaches the focused program as `SIGINT`.
/// `WallKeys` only acts on the copy chord when the focused terminal has a selection; with
/// none it behaves like an ordinary key.
pub fn is_copy_chord(platform: Platform, event: &KeyEvent) -> bool {
    if event.key != "c" && event.key != "C" {
        return false;
    }
    match platform {
        Platform::Mac => event.mods.meta && !event.mods.ctrl && !event.mods.alt,
        Platform::Other => {
            event.mods.ctrl && event.mods.shift && !event.mods.alt && !event.mods.meta
        }
    }
}

/// Whether `event` is the platform's paste chord (`⌘V` on macOS, `Ctrl+Shift+V` elsewhere;
/// twin of [`is_copy_chord`] and `keyEncoding.isPasteChord`).
///
/// Plain `Ctrl+V` (`\x16`, readline's quoted-insert) is never a paste chord and still
/// reaches the focused program.
///
/// **Deliberate difference from JS (`keyEncoding.js`):** `keyEncoding.isPasteChord` lives
/// in `@glyph3d/core` without `chord.js`'s `IS_MAC` flag and therefore matches both
/// `Meta+V` and `Ctrl+Shift+V` on every OS (swallowing `Ctrl+Shift+V` on macOS and
/// `Super+V` on Linux/Windows instead of forwarding them to the terminal). Taking
/// [`Platform`] here keeps `is_paste_chord` symmetric with [`is_copy_chord`] and
/// [`paste_label`].
pub fn is_paste_chord(platform: Platform, event: &KeyEvent) -> bool {
    if event.key != "v" && event.key != "V" {
        return false;
    }
    match platform {
        Platform::Mac => event.mods.meta && !event.mods.ctrl && !event.mods.alt,
        Platform::Other => {
            event.mods.ctrl && event.mods.shift && !event.mods.alt && !event.mods.meta
        }
    }
}

/// Display label for the copy chord (`⌘C` on macOS, `Ctrl+Shift+C` elsewhere; `COPY_LABEL`).
pub const fn copy_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌘C",
        Platform::Other => "Ctrl+Shift+C",
    }
}

/// Display label for the paste chord (`⌘V` on macOS, `Ctrl+Shift+V` elsewhere; `PASTE_LABEL`).
pub const fn paste_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌘V",
        Platform::Other => "Ctrl+Shift+V",
    }
}

/// Focus-history direction for `Ctrl+,` / `Ctrl+.` (and shifted `Ctrl+<` / `Ctrl+>`):
/// [`CycleDir::Prev`] (`-1`) or [`CycleDir::Next`] (`+1`), or `None` otherwise
/// (`focusHistoryDir` in `chord.js`).
///
/// Excludes `Alt` and `Meta` so `⌘,` and `Ctrl+Alt+,` (settings) stay untouched; neither
/// `,` nor `.` is a shell control byte.
pub fn focus_history_dir(event: &KeyEvent) -> Option<CycleDir> {
    if !event.mods.ctrl || event.mods.alt || event.mods.meta {
        return None;
    }
    match event.code.as_str() {
        "Comma" => Some(CycleDir::Prev),
        "Period" => Some(CycleDir::Next),
        _ => None,
    }
}

/// Display label for the focus-history shortcuts (`⌃, · ⌃.` on macOS, `Ctrl+, · Ctrl+.` elsewhere;
/// `FOCUS_HISTORY_LABEL`).
pub const fn focus_history_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌃, · ⌃.",
        Platform::Other => "Ctrl+, · Ctrl+.",
    }
}

/// Group-cycling direction for `Ctrl+Shift+[` ([`CycleDir::Prev`]) and `Ctrl+Shift+]`
/// ([`CycleDir::Next`]), or `None` otherwise (`groupCycleDir` in `chord.js`).
///
/// Requires `Shift` and excludes `Alt`/`Meta`, so unshifted `Ctrl+[` (`ESC`) and `Ctrl+]`
/// still reach the shell/vim, and `⌘[` / `⌘]` (`Ctrl+Alt+[` / `]` off macOS) still cycle
/// panes through `KEYMAP`.
pub fn group_cycle_dir(event: &KeyEvent) -> Option<CycleDir> {
    if !event.mods.ctrl || !event.mods.shift || event.mods.alt || event.mods.meta {
        return None;
    }
    match event.code.as_str() {
        "BracketLeft" => Some(CycleDir::Prev),
        "BracketRight" => Some(CycleDir::Next),
        _ => None,
    }
}

/// Display label for the group-cycling shortcuts (`⌃⇧[ · ⌃⇧]` on macOS, `Ctrl+Shift+[ · ]`
/// elsewhere; `GROUP_CYCLE_LABEL`).
pub const fn group_cycle_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌃⇧[ · ⌃⇧]",
        Platform::Other => "Ctrl+Shift+[ · ]",
    }
}

/// Whether `mods` held at a pointer press on a terminal's body start rectangular
/// column-block selection (`blockSelects` in `chord.js`).
///
/// Off macOS this follows Ghostty's rule (`Alt` with `Ctrl` or `Super`); on macOS it is
/// `⌘⌥` because `⌥` alone is drag-to-move and `⌃` at press is secondary click.
pub const fn block_selects(platform: Platform, mods: Mods) -> bool {
    mods.alt
        && match platform {
            Platform::Mac => mods.meta,
            Platform::Other => mods.ctrl || mods.meta,
        }
}

/// Display label for the block-selection modifiers (`⌘⌥` on macOS, `Ctrl+Alt` elsewhere;
/// `BLOCK_SELECT_LABEL`).
pub const fn block_select_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌘⌥",
        Platform::Other => "Ctrl+Alt",
    }
}

/// Whether `mods` carry the link-open hover/click modifier (`⌘` alone on macOS,
/// `Ctrl` alone elsewhere), matching standard native terminal conventions without
/// conflicting with `Alt` move-drag, `Shift` selection/lasso, `⌘⌥`/`Ctrl+Alt`
/// block-select, or macOS `Ctrl+click` context menu.
pub const fn is_link_open_mods(platform: Platform, mods: Mods) -> bool {
    match platform {
        Platform::Mac => mods.meta && !mods.ctrl && !mods.alt && !mods.shift,
        Platform::Other => mods.ctrl && !mods.alt && !mods.meta && !mods.shift,
    }
}

/// Display label for the link-open click gesture (`"⌘-click"` on macOS, `"Ctrl-click"` elsewhere).
pub const fn link_open_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "⌘-click",
        Platform::Other => "Ctrl-click",
    }
}

/// Whether `mods` carry the platform's wall chord (`⌘` on macOS, `Ctrl+Alt` without `Meta`
/// elsewhere; `Shift` may ride along).
pub const fn is_chord_mods(platform: Platform, mods: Mods) -> bool {
    match platform {
        Platform::Mac => mods.meta,
        Platform::Other => mods.ctrl && mods.alt && !mods.meta,
    }
}

/// Whether `event` carries the platform's wall chord (`isChord` in `chord.js`).
///
/// `Shift` may ride along. Off macOS, `AltGraph` (which European layouts use to type
/// `[`, `]`, `{`, `}`, `0`) is never a wall chord.
pub fn is_chord(platform: Platform, event: &KeyEvent) -> bool {
    if !is_chord_mods(platform, event.mods) {
        return false;
    }
    !matches!(platform, Platform::Other) || event.key != "AltGraph"
}

/// Format a chord's display label for the help panel, topbar, and palette (`chordLabel`
/// in `chord.js`): `"⌘]"` / `"Ctrl+Alt+]"`, or `"⌘[ ⌘]"` / `"Ctrl+Alt+[ ]"` for several keys.
pub fn chord_label(platform: Platform, keys: &[&str], shift: bool) -> String {
    match platform {
        Platform::Mac => {
            let prefix = if shift { "⌘⇧" } else { "⌘" };
            keys.iter()
                .map(|k| format!("{prefix}{k}"))
                .collect::<Vec<_>>()
                .join(" ")
        }
        Platform::Other => {
            let shift_part = if shift { "Shift+" } else { "" };
            format!("Ctrl+Alt+{shift_part}{}", keys.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_ev(key: &str, code: &str, mods: Mods) -> KeyEvent {
        KeyEvent {
            code: code.to_string(),
            key: key.to_string(),
            text: None,
            mods,
            repeat: false,
            pressed: true,
        }
    }

    #[test]
    fn platform_chord_keys_and_labels_match_js() {
        assert!(is_mac(Platform::Mac));
        assert!(!is_mac(Platform::Other));
        assert_eq!(chord_keys(Platform::Mac), &["Meta"]);
        assert_eq!(chord_keys(Platform::Other), &["Control", "Alt"]);

        assert_eq!(alt_label(Platform::Mac), "⌥");
        assert_eq!(alt_label(Platform::Other), "Alt");
        assert_eq!(snap_invert_label(Platform::Mac), "⌃");
        assert_eq!(snap_invert_label(Platform::Other), "Ctrl");
        assert_eq!(copy_label(Platform::Mac), "⌘C");
        assert_eq!(copy_label(Platform::Other), "Ctrl+Shift+C");
        assert_eq!(paste_label(Platform::Mac), "⌘V");
        assert_eq!(paste_label(Platform::Other), "Ctrl+Shift+V");
        assert_eq!(focus_history_label(Platform::Mac), "⌃, · ⌃.");
        assert_eq!(focus_history_label(Platform::Other), "Ctrl+, · Ctrl+.");
        assert_eq!(group_cycle_label(Platform::Mac), "⌃⇧[ · ⌃⇧]");
        assert_eq!(group_cycle_label(Platform::Other), "Ctrl+Shift+[ · ]");
        assert_eq!(block_select_label(Platform::Mac), "⌘⌥");
        assert_eq!(block_select_label(Platform::Other), "Ctrl+Alt");
    }

    #[test]
    fn snap_invert_uses_ctrl_only() {
        assert!(!inverts_snap(Mods::default()));
        assert!(inverts_snap(Mods { ctrl: true, ..Mods::default() }));
        assert!(!inverts_snap(Mods { alt: true, shift: true, meta: true, ctrl: false }));
    }

    #[test]
    fn zoom_keys_parse_match_and_label() {
        let parsed = parse_zoom_keys("  ctrl   alt meta ").unwrap();
        assert_eq!(parsed, vec![ZoomKey::Ctrl, ZoomKey::Alt, ZoomKey::Meta]);
        assert_eq!(parse_zoom_keys("   ").unwrap(), Vec::<ZoomKey>::new());

        let err = parse_zoom_keys("ctrl super").unwrap_err();
        assert_eq!(err, "\"super\" is not a modifier (ctrl, alt, meta, shift)");

        assert_eq!(zoom_wheel_label(Platform::Mac, "ctrl alt meta").unwrap(), "⌃/⌥/⌘");
        assert_eq!(zoom_wheel_label(Platform::Other, "ctrl alt meta").unwrap(), "Ctrl/Alt/Super");
        assert_eq!(zoom_wheel_label(Platform::Mac, "shift").unwrap(), "⇧");
        assert_eq!(zoom_wheel_label(Platform::Other, "shift").unwrap(), "Shift");
        assert_eq!(zoom_wheel_label(Platform::Other, "").unwrap(), "no key");

        assert!(zooms_wheel(Mods { ctrl: true, ..Mods::default() }, "ctrl alt meta").unwrap());
        assert!(zooms_wheel(Mods { alt: true, ..Mods::default() }, "ctrl alt meta").unwrap());
        assert!(zooms_wheel(Mods { meta: true, ..Mods::default() }, "ctrl alt meta").unwrap());
        assert!(!zooms_wheel(Mods { shift: true, ..Mods::default() }, "ctrl alt meta").unwrap());
        assert!(!zooms_wheel(Mods::default(), "ctrl alt meta").unwrap());
        assert!(!zooms_wheel(Mods { ctrl: true, ..Mods::default() }, "").unwrap());
    }

    #[test]
    fn copy_and_paste_chords_respect_platform_and_keep_sigint_and_ctrl_v() {
        let cmd_c = key_ev("c", "KeyC", Mods { meta: true, ..Mods::default() });
        let ctrl_shift_c =
            key_ev("C", "KeyC", Mods { ctrl: true, shift: true, ..Mods::default() });
        let ctrl_c = key_ev("c", "KeyC", Mods { ctrl: true, ..Mods::default() });

        assert!(is_copy_chord(Platform::Mac, &cmd_c));
        assert!(!is_copy_chord(Platform::Mac, &ctrl_shift_c));
        assert!(!is_copy_chord(Platform::Mac, &ctrl_c));

        assert!(is_copy_chord(Platform::Other, &ctrl_shift_c));
        assert!(!is_copy_chord(Platform::Other, &cmd_c));
        assert!(!is_copy_chord(Platform::Other, &ctrl_c));

        let cmd_v = key_ev("v", "KeyV", Mods { meta: true, ..Mods::default() });
        let ctrl_shift_v =
            key_ev("V", "KeyV", Mods { ctrl: true, shift: true, ..Mods::default() });
        let ctrl_v = key_ev("v", "KeyV", Mods { ctrl: true, ..Mods::default() });

        assert!(is_paste_chord(Platform::Mac, &cmd_v));
        assert!(!is_paste_chord(Platform::Mac, &ctrl_shift_v));
        assert!(!is_paste_chord(Platform::Mac, &ctrl_v));

        assert!(is_paste_chord(Platform::Other, &ctrl_shift_v));
        assert!(!is_paste_chord(Platform::Other, &cmd_v));
        assert!(!is_paste_chord(Platform::Other, &ctrl_v));
    }

    #[test]
    fn focus_history_and_group_cycle_directions() {
        let ctrl_comma = key_ev(",", "Comma", Mods { ctrl: true, ..Mods::default() });
        let ctrl_shift_period =
            key_ev(">", "Period", Mods { ctrl: true, shift: true, ..Mods::default() });
        let cmd_comma = key_ev(",", "Comma", Mods { meta: true, ..Mods::default() });
        let ctrl_alt_comma =
            key_ev(",", "Comma", Mods { ctrl: true, alt: true, ..Mods::default() });

        assert_eq!(focus_history_dir(&ctrl_comma), Some(CycleDir::Prev));
        assert_eq!(focus_history_dir(&ctrl_comma).unwrap().step(), -1);
        assert_eq!(focus_history_dir(&ctrl_shift_period), Some(CycleDir::Next));
        assert_eq!(focus_history_dir(&ctrl_shift_period).unwrap().step(), 1);
        assert_eq!(focus_history_dir(&cmd_comma), None);
        assert_eq!(focus_history_dir(&ctrl_alt_comma), None);

        let ctrl_lbrack = key_ev("[", "BracketLeft", Mods { ctrl: true, ..Mods::default() });
        let ctrl_shift_lbrack =
            key_ev("{", "BracketLeft", Mods { ctrl: true, shift: true, ..Mods::default() });
        let ctrl_shift_rbrack =
            key_ev("}", "BracketRight", Mods { ctrl: true, shift: true, ..Mods::default() });
        let ctrl_alt_shift_lbrack = key_ev(
            "{",
            "BracketLeft",
            Mods { ctrl: true, alt: true, shift: true, meta: false },
        );

        assert_eq!(group_cycle_dir(&ctrl_lbrack), None);
        assert_eq!(group_cycle_dir(&ctrl_shift_lbrack), Some(CycleDir::Prev));
        assert_eq!(group_cycle_dir(&ctrl_shift_rbrack), Some(CycleDir::Next));
        assert_eq!(group_cycle_dir(&ctrl_alt_shift_lbrack), None);
    }

    #[test]
    fn block_selects_and_is_chord_and_chord_label() {
        assert!(!block_selects(Platform::Mac, Mods { alt: true, ..Mods::default() }));
        assert!(!block_selects(Platform::Mac, Mods { alt: true, ctrl: true, ..Mods::default() }));
        assert!(block_selects(Platform::Mac, Mods { alt: true, meta: true, ..Mods::default() }));

        assert!(!block_selects(Platform::Other, Mods { alt: true, ..Mods::default() }));
        assert!(block_selects(
            Platform::Other,
            Mods { alt: true, ctrl: true, ..Mods::default() }
        ));
        assert!(block_selects(
            Platform::Other,
            Mods { alt: true, meta: true, ..Mods::default() }
        ));

        let mac_chord = key_ev("]", "BracketRight", chord_mods(Platform::Mac));
        let other_chord = key_ev("]", "BracketRight", chord_mods(Platform::Other));
        let other_with_meta = key_ev(
            "]",
            "BracketRight",
            Mods { ctrl: true, alt: true, meta: true, shift: false },
        );
        let alt_gr = key_ev("AltGraph", "AltRight", chord_mods(Platform::Other));

        assert!(is_chord(Platform::Mac, &mac_chord));
        assert!(!is_chord(Platform::Mac, &other_chord));
        assert!(is_chord(Platform::Other, &other_chord));
        assert!(!is_chord(Platform::Other, &mac_chord));
        assert!(!is_chord(Platform::Other, &other_with_meta));
        assert!(!is_chord(Platform::Other, &alt_gr));

        assert_eq!(chord_label(Platform::Mac, &["]"], false), "⌘]");
        assert_eq!(chord_label(Platform::Mac, &["↵"], true), "⌘⇧↵");
        assert_eq!(chord_label(Platform::Mac, &["[", "]"], false), "⌘[ ⌘]");
        assert_eq!(chord_label(Platform::Other, &["]"], false), "Ctrl+Alt+]");
        assert_eq!(chord_label(Platform::Other, &["↵"], true), "Ctrl+Alt+Shift+↵");
        assert_eq!(chord_label(Platform::Other, &["[", "]"], false), "Ctrl+Alt+[ ]");
    }
}
