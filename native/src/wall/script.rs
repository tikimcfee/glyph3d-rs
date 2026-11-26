//! Scripted window-level input verbs (`app.move`, `app.down`, `app.up`, `app.click`,
//! `app.dblclick`, `app.drag`, `app.wheel`, `app.pinch`, `app.key`, `app.type`, and
//! `app.where`) for driving a live `--wall` window over `wallctl`.
//!
//! Coordinates are logical pixels from the window's top-left `(0, 0)`, matching
//! `App::cursor` and `Wall`'s viewport coordinate space.

use wall_core::geom::Point;
use wall_core::menu::MenuTarget;
use wall_core::router::CommandRouter;
use wall_core::wall::WallFrame;
use winit::event::MouseButton;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, NativeKeyCode, PhysicalKey, SmolStr};

/// Default interpolation step count for `app.drag` when omitted.
pub const DEFAULT_DRAG_STEPS: usize = 12;

/// One parsed `app.*` scripted-input or inspection command.
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptCommand {
    /// `app.move <x> <y>`
    Move { x: f64, y: f64 },
    /// `app.down [left|right|middle] [mods]`
    Down {
        button: MouseButton,
        mods: Option<ModifiersState>,
    },
    /// `app.up [left|right|middle] [mods]`
    Up {
        button: MouseButton,
        mods: Option<ModifiersState>,
    },
    /// `app.click <x> <y> [left|right|middle] [mods]`
    Click {
        x: f64,
        y: f64,
        button: MouseButton,
        mods: ModifiersState,
    },
    /// `app.dblclick <x> <y>`
    DblClick { x: f64, y: f64 },
    /// `app.drag <x0> <y0> <x1> <y1> [steps] [left|right|middle] [mods]`
    Drag {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        steps: usize,
        button: MouseButton,
        mods: ModifiersState,
    },
    /// `app.wheel <x> <y> <dx> <dy> [mods]` (winit logical pixel delta: `+dy` scrolls up / zooms in).
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        mods: ModifiersState,
    },
    /// `app.pinch <x> <y> <factor>`
    Pinch { x: f64, y: f64, factor: f64 },
    /// `app.key <chord>` (e.g. `cmd+d`, `cmd+shift+d`, `shift+Tab`, `Enter`, `Escape`).
    Key(ScriptKey),
    /// `app.type <text>`
    Type(String),
    /// `app.where`
    Where,
}

/// Physical key, logical key, typed text, and modifiers for one synthesized keystroke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptKey {
    pub physical: PhysicalKey,
    pub logical: Key,
    pub text: Option<String>,
    pub mods: ModifiersState,
}

/// Check whether `line` targets one of the scripted `app.*` verbs handled by this module.
///
/// Returns `None` if `line` is not one of these verbs (e.g. `app.shot`, `app.text`, `app.quit`,
/// or any `wall.*` / `terminal.*` router verb).
pub fn parse_script_line(line: &str) -> Option<Result<ScriptCommand, String>> {
    let argv = CommandRouter::<()>::parse_line(line);
    let verb = argv.first()?.as_str();
    let args = &argv[1..];
    match verb {
        "app.move" => Some(parse_move(args)),
        "app.down" => Some(parse_down_or_up("app.down", args, true)),
        "app.up" => Some(parse_down_or_up("app.up", args, false)),
        "app.click" => Some(parse_click(args)),
        "app.dblclick" => Some(parse_dblclick(args)),
        "app.drag" => Some(parse_drag(args)),
        "app.wheel" => Some(parse_wheel(args)),
        "app.pinch" => Some(parse_pinch(args)),
        "app.key" => Some(parse_key_verb(args)),
        "app.type" => Some(parse_type_verb(args)),
        "app.where" => {
            if !args.is_empty() {
                Some(Err("usage: app.where".to_string()))
            } else {
                Some(Ok(ScriptCommand::Where))
            }
        }
        _ => None,
    }
}

fn parse_f64_arg(args: &[String], idx: usize, name: &str, usage: &str) -> Result<f64, String> {
    let Some(raw) = args.get(idx) else {
        return Err(format!("usage: {usage}"));
    };
    let val: f64 = raw
        .parse()
        .map_err(|_| format!("{name} must be a number, not '{raw}'"))?;
    if !val.is_finite() {
        return Err(format!("{name} must be finite, not '{raw}'"));
    }
    Ok(val)
}

/// Parse a mouse button token (`left`, `right`, `middle`, `back`, `forward`).
pub fn parse_button(raw: &str) -> Result<MouseButton, String> {
    match raw.to_ascii_lowercase().as_str() {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        "back" => Ok(MouseButton::Back),
        "forward" => Ok(MouseButton::Forward),
        _ => Err(format!("button is left|right|middle, not '{raw}'")),
    }
}

pub(crate) fn button_label(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
        MouseButton::Back => "back",
        MouseButton::Forward => "forward",
        MouseButton::Other(_) => "other",
    }
}

/// Parse `+`-joined modifier names (`shift`, `ctrl`, `alt`/`opt`, `cmd`/`meta`/`super`, or `none`).
pub fn parse_mods(raw: &str) -> Result<ModifiersState, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok(ModifiersState::empty());
    }
    let mut out = ModifiersState::empty();
    for part in trimmed.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "" => return Err(format!("empty modifier in '{raw}'")),
            "none" => {}
            "shift" => out |= ModifiersState::SHIFT,
            "ctrl" | "control" => out |= ModifiersState::CONTROL,
            "alt" | "opt" | "option" => out |= ModifiersState::ALT,
            "cmd" | "command" | "meta" | "super" => out |= ModifiersState::SUPER,
            other => {
                return Err(format!(
                    "unknown modifier '{other}' (expected shift, ctrl, alt, cmd)"
                ));
            }
        }
    }
    Ok(out)
}

fn parse_move(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.move <x> <y>";
    if args.len() != 2 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;
    Ok(ScriptCommand::Move { x, y })
}

fn parse_down_or_up(verb: &str, args: &[String], is_down: bool) -> Result<ScriptCommand, String> {
    if args.len() > 2 {
        return Err(format!("usage: {verb} [left|right|middle] [mods]"));
    }
    let (button, mods) = match args.len() {
        0 => (MouseButton::Left, None),
        1 => match parse_button(&args[0]) {
            Ok(b) => (b, None),
            Err(btn_err) => match parse_mods(&args[0]) {
                Ok(m) => (MouseButton::Left, Some(m)),
                Err(_) => return Err(btn_err),
            },
        },
        _ => (parse_button(&args[0])?, Some(parse_mods(&args[1])?)),
    };
    if is_down {
        Ok(ScriptCommand::Down { button, mods })
    } else {
        Ok(ScriptCommand::Up { button, mods })
    }
}

fn parse_click(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.click <x> <y> [left|right|middle] [mods]";
    if args.len() < 2 || args.len() > 4 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;
    let (button, mods) = match args.len() {
        2 => (MouseButton::Left, ModifiersState::empty()),
        3 => match parse_button(&args[2]) {
            Ok(b) => (b, ModifiersState::empty()),
            Err(btn_err) => match parse_mods(&args[2]) {
                Ok(m) => (MouseButton::Left, m),
                Err(_) => return Err(btn_err),
            },
        },
        _ => (parse_button(&args[2])?, parse_mods(&args[3])?),
    };
    Ok(ScriptCommand::Click { x, y, button, mods })
}

fn parse_dblclick(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.dblclick <x> <y>";
    if args.len() != 2 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;
    Ok(ScriptCommand::DblClick { x, y })
}

fn parse_drag(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.drag <x0> <y0> <x1> <y1> [steps] [left|right|middle] [mods]";
    if args.len() < 4 || args.len() > 7 {
        return Err(format!("usage: {USAGE}"));
    }
    let x0 = parse_f64_arg(args, 0, "x0", USAGE)?;
    let y0 = parse_f64_arg(args, 1, "y0", USAGE)?;
    let x1 = parse_f64_arg(args, 2, "x1", USAGE)?;
    let y1 = parse_f64_arg(args, 3, "y1", USAGE)?;

    let mut steps: Option<usize> = None;
    let mut button: Option<MouseButton> = None;
    let mut mods: Option<ModifiersState> = None;

    for tok in &args[4..] {
        if steps.is_none() {
            if let Ok(n) = tok.parse::<isize>() {
                if n < 1 {
                    return Err(format!("steps must be >= 1, not '{tok}'"));
                }
                steps = Some(n as usize);
                continue;
            }
        }
        if button.is_none() {
            if let Ok(b) = parse_button(tok) {
                button = Some(b);
                continue;
            }
        }
        if mods.is_none() {
            if let Ok(m) = parse_mods(tok) {
                mods = Some(m);
                continue;
            }
        }
        return Err(format!(
            "unexpected drag argument '{tok}' (usage: {USAGE})"
        ));
    }

    Ok(ScriptCommand::Drag {
        x0,
        y0,
        x1,
        y1,
        steps: steps.unwrap_or(DEFAULT_DRAG_STEPS),
        button: button.unwrap_or(MouseButton::Left),
        mods: mods.unwrap_or_else(ModifiersState::empty),
    })
}

fn parse_wheel(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.wheel <x> <y> <dx> <dy> [mods]";
    if args.len() < 4 || args.len() > 5 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;
    let dx = parse_f64_arg(args, 2, "dx", USAGE)?;
    let dy = parse_f64_arg(args, 3, "dy", USAGE)?;
    let mods = match args.get(4) {
        Some(raw) => parse_mods(raw)?,
        None => ModifiersState::empty(),
    };
    Ok(ScriptCommand::Wheel { x, y, dx, dy, mods })
}

fn parse_pinch(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.pinch <x> <y> <factor>";
    if args.len() != 3 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;
    let factor = parse_f64_arg(args, 2, "factor", USAGE)?;
    if factor <= 0.0 {
        return Err(format!("factor must be > 0, not '{factor}'"));
    }
    Ok(ScriptCommand::Pinch { x, y, factor })
}

fn parse_key_verb(args: &[String]) -> Result<ScriptCommand, String> {
    if args.len() != 1 {
        return Err("usage: app.key <chord>".to_string());
    }
    parse_key_chord(&args[0]).map(ScriptCommand::Key)
}

fn parse_type_verb(args: &[String]) -> Result<ScriptCommand, String> {
    if args.is_empty() {
        return Err("usage: app.type <text>".to_string());
    }
    Ok(ScriptCommand::Type(args.join(" ")))
}

/// Parse a chord specification such as `cmd+d`, `cmd+shift+d`, `shift+Tab`, `Enter`,
/// `Escape`, `cmd+backslash`, `cmd+\`, or `F1` into a [`ScriptKey`].
pub fn parse_key_chord(spec: &str) -> Result<ScriptKey, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("empty key chord".to_string());
    }

    // Split `mod1+mod2+key` while preserving `+` itself as the key when written `+` or `cmd++`.
    let (mod_prefix, key_tok) = if trimmed == "+" {
        ("", "+")
    } else if let Some(prefix) = trimmed.strip_suffix("++") {
        (prefix, "+")
    } else if let Some((mods_part, last)) = trimmed.rsplit_once('+') {
        if last.is_empty() {
            return Err(format!("missing key after '+' in '{spec}'"));
        }
        (mods_part, last)
    } else {
        ("", trimmed)
    };

    let mut mods = if mod_prefix.is_empty() {
        ModifiersState::empty()
    } else {
        parse_mods(mod_prefix)?
    };

    let (code, logical, text, extra_shift) = resolve_key_token(key_tok, mods)?;
    if extra_shift {
        mods |= ModifiersState::SHIFT;
    }
    Ok(ScriptKey {
        physical: PhysicalKey::Code(code),
        logical,
        text,
        mods,
    })
}

fn resolve_key_token(
    tok: &str,
    mods: ModifiersState,
) -> Result<(KeyCode, Key, Option<String>, bool), String> {
    let shift = mods.shift_key();
    let has_chord_mod = mods.super_key() || mods.control_key() || mods.alt_key();

    // Single ASCII letter: `a`..`z` / `A`..`Z` (`cmd+d` and `cmd+D` both mean `KeyD` without
    // implicit Shift unless `shift+` was written explicitly).
    if tok.len() == 1 {
        let ch = tok.chars().next().expect("tok len == 1");
        if ch.is_ascii_alphabetic() {
            let code = letter_keycode(ch);
            let out_ch = if shift {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            };
            let s = out_ch.to_string();
            let text = (!mods.super_key()).then(|| s.clone());
            return Ok((code, Key::Character(SmolStr::new(&s)), text, false));
        }
        if ch.is_ascii_digit() {
            let code = digit_keycode(ch);
            let out = if shift { shifted_digit(ch) } else { ch }.to_string();
            let text = (!mods.super_key()).then(|| out.clone());
            return Ok((code, Key::Character(SmolStr::new(&out)), text, false));
        }
    }

    // Explicit W3C `KeyA`..`KeyZ` or `Digit0`..`Digit9`.
    if tok.len() == 4 && tok[..3].eq_ignore_ascii_case("key") {
        let ch = tok.chars().nth(3).expect("len == 4");
        if ch.is_ascii_alphabetic() {
            let code = letter_keycode(ch);
            let out_ch = if shift {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            };
            let s = out_ch.to_string();
            let text = (!mods.super_key()).then(|| s.clone());
            return Ok((code, Key::Character(SmolStr::new(&s)), text, false));
        }
    }
    if tok.len() == 6 && tok[..5].eq_ignore_ascii_case("digit") {
        let ch = tok.chars().nth(5).expect("len == 6");
        if ch.is_ascii_digit() {
            let code = digit_keycode(ch);
            let out = if shift { shifted_digit(ch) } else { ch }.to_string();
            let text = (!mods.super_key()).then(|| out.clone());
            return Ok((code, Key::Character(SmolStr::new(&out)), text, false));
        }
    }

    let sym = |code: KeyCode, unshifted: &str, shifted: &str, force_shift: bool| {
        let use_s = if shift || force_shift { shifted } else { unshifted };
        let text = (!has_chord_mod || (!mods.super_key() && !mods.control_key()))
            .then(|| use_s.to_string());
        Ok((
            code,
            Key::Character(SmolStr::new(use_s)),
            text,
            force_shift,
        ))
    };

    match tok.to_ascii_lowercase().as_str() {
        "enter" | "return" => Ok((KeyCode::Enter, Key::Named(NamedKey::Enter), None, false)),
        "tab" => Ok((KeyCode::Tab, Key::Named(NamedKey::Tab), None, false)),
        "escape" | "esc" => Ok((KeyCode::Escape, Key::Named(NamedKey::Escape), None, false)),
        "space" | " " => Ok((
            KeyCode::Space,
            Key::Named(NamedKey::Space),
            Some(" ".to_string()),
            false,
        )),
        "backspace" => Ok((
            KeyCode::Backspace,
            Key::Named(NamedKey::Backspace),
            None,
            false,
        )),
        "delete" | "del" => Ok((KeyCode::Delete, Key::Named(NamedKey::Delete), None, false)),
        "insert" => Ok((KeyCode::Insert, Key::Named(NamedKey::Insert), None, false)),
        "home" => Ok((KeyCode::Home, Key::Named(NamedKey::Home), None, false)),
        "end" => Ok((KeyCode::End, Key::Named(NamedKey::End), None, false)),
        "pageup" | "pgup" => Ok((KeyCode::PageUp, Key::Named(NamedKey::PageUp), None, false)),
        "pagedown" | "pgdn" => Ok((
            KeyCode::PageDown,
            Key::Named(NamedKey::PageDown),
            None,
            false,
        )),
        "arrowup" | "up" => Ok((KeyCode::ArrowUp, Key::Named(NamedKey::ArrowUp), None, false)),
        "arrowdown" | "down" => Ok((
            KeyCode::ArrowDown,
            Key::Named(NamedKey::ArrowDown),
            None,
            false,
        )),
        "arrowleft" | "left" => Ok((
            KeyCode::ArrowLeft,
            Key::Named(NamedKey::ArrowLeft),
            None,
            false,
        )),
        "arrowright" | "right" => Ok((
            KeyCode::ArrowRight,
            Key::Named(NamedKey::ArrowRight),
            None,
            false,
        )),
        "f1" => Ok((KeyCode::F1, Key::Named(NamedKey::F1), None, false)),
        "f2" => Ok((KeyCode::F2, Key::Named(NamedKey::F2), None, false)),
        "f3" => Ok((KeyCode::F3, Key::Named(NamedKey::F3), None, false)),
        "f4" => Ok((KeyCode::F4, Key::Named(NamedKey::F4), None, false)),
        "f5" => Ok((KeyCode::F5, Key::Named(NamedKey::F5), None, false)),
        "f6" => Ok((KeyCode::F6, Key::Named(NamedKey::F6), None, false)),
        "f7" => Ok((KeyCode::F7, Key::Named(NamedKey::F7), None, false)),
        "f8" => Ok((KeyCode::F8, Key::Named(NamedKey::F8), None, false)),
        "f9" => Ok((KeyCode::F9, Key::Named(NamedKey::F9), None, false)),
        "f10" => Ok((KeyCode::F10, Key::Named(NamedKey::F10), None, false)),
        "f11" => Ok((KeyCode::F11, Key::Named(NamedKey::F11), None, false)),
        "f12" => Ok((KeyCode::F12, Key::Named(NamedKey::F12), None, false)),
        "\\" | "backslash" => sym(KeyCode::Backslash, "\\", "|", false),
        "|" | "pipe" => sym(KeyCode::Backslash, "\\", "|", true),
        "/" | "slash" => sym(KeyCode::Slash, "/", "?", false),
        "?" | "question" => sym(KeyCode::Slash, "/", "?", true),
        "," | "comma" => sym(KeyCode::Comma, ",", "<", false),
        "<" => sym(KeyCode::Comma, ",", "<", true),
        "." | "period" => sym(KeyCode::Period, ".", ">", false),
        ">" => sym(KeyCode::Period, ".", ">", true),
        ";" | "semicolon" => sym(KeyCode::Semicolon, ";", ":", false),
        ":" | "colon" => sym(KeyCode::Semicolon, ";", ":", true),
        "'" | "quote" => sym(KeyCode::Quote, "'", "\"", false),
        "\"" => sym(KeyCode::Quote, "'", "\"", true),
        "[" | "bracketleft" => sym(KeyCode::BracketLeft, "[", "{", false),
        "{" => sym(KeyCode::BracketLeft, "[", "{", true),
        "]" | "bracketright" => sym(KeyCode::BracketRight, "]", "}", false),
        "}" => sym(KeyCode::BracketRight, "]", "}", true),
        "-" | "minus" => sym(KeyCode::Minus, "-", "_", false),
        "_" | "underscore" => sym(KeyCode::Minus, "-", "_", true),
        "=" | "equal" | "equals" => sym(KeyCode::Equal, "=", "+", false),
        "+" | "plus" => sym(KeyCode::Equal, "=", "+", true),
        "`" | "backquote" | "grave" => sym(KeyCode::Backquote, "`", "~", false),
        "~" | "tilde" => sym(KeyCode::Backquote, "`", "~", true),
        _ => Err(format!("unknown key '{tok}'")),
    }
}

/// Expand typed text (`app.type`) into one [`ScriptKey`] per character (`'\n'` -> `Enter`,
/// `'\t'` -> `Tab`, `' '` -> `Space`, printable ASCII/Unicode -> character press).
pub fn keys_for_typed_text(text: &str) -> Vec<ScriptKey> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\r' => continue,
            '\n' => out.push(ScriptKey {
                physical: PhysicalKey::Code(KeyCode::Enter),
                logical: Key::Named(NamedKey::Enter),
                text: None,
                mods: ModifiersState::empty(),
            }),
            '\t' => out.push(ScriptKey {
                physical: PhysicalKey::Code(KeyCode::Tab),
                logical: Key::Named(NamedKey::Tab),
                text: None,
                mods: ModifiersState::empty(),
            }),
            ' ' => out.push(ScriptKey {
                physical: PhysicalKey::Code(KeyCode::Space),
                logical: Key::Named(NamedKey::Space),
                text: Some(" ".to_string()),
                mods: ModifiersState::empty(),
            }),
            c if c.is_ascii_alphabetic() => {
                let is_upper = c.is_ascii_uppercase();
                let s = c.to_string();
                out.push(ScriptKey {
                    physical: PhysicalKey::Code(letter_keycode(c)),
                    logical: Key::Character(SmolStr::new(&s)),
                    text: Some(s),
                    mods: if is_upper {
                        ModifiersState::SHIFT
                    } else {
                        ModifiersState::empty()
                    },
                });
            }
            c if c.is_ascii_digit() => {
                let s = c.to_string();
                out.push(ScriptKey {
                    physical: PhysicalKey::Code(digit_keycode(c)),
                    logical: Key::Character(SmolStr::new(&s)),
                    text: Some(s),
                    mods: ModifiersState::empty(),
                });
            }
            c => {
                let s = c.to_string();
                if let Some((code, shifted)) = ascii_punct_keycode(c) {
                    out.push(ScriptKey {
                        physical: PhysicalKey::Code(code),
                        logical: Key::Character(SmolStr::new(&s)),
                        text: Some(s),
                        mods: if shifted {
                            ModifiersState::SHIFT
                        } else {
                            ModifiersState::empty()
                        },
                    });
                } else {
                    out.push(ScriptKey {
                        physical: PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
                        logical: Key::Character(SmolStr::new(&s)),
                        text: Some(s),
                        mods: ModifiersState::empty(),
                    });
                }
            }
        }
    }
    out
}

fn letter_keycode(ch: char) -> KeyCode {
    match ch.to_ascii_lowercase() {
        'a' => KeyCode::KeyA,
        'b' => KeyCode::KeyB,
        'c' => KeyCode::KeyC,
        'd' => KeyCode::KeyD,
        'e' => KeyCode::KeyE,
        'f' => KeyCode::KeyF,
        'g' => KeyCode::KeyG,
        'h' => KeyCode::KeyH,
        'i' => KeyCode::KeyI,
        'j' => KeyCode::KeyJ,
        'k' => KeyCode::KeyK,
        'l' => KeyCode::KeyL,
        'm' => KeyCode::KeyM,
        'n' => KeyCode::KeyN,
        'o' => KeyCode::KeyO,
        'p' => KeyCode::KeyP,
        'q' => KeyCode::KeyQ,
        'r' => KeyCode::KeyR,
        's' => KeyCode::KeyS,
        't' => KeyCode::KeyT,
        'u' => KeyCode::KeyU,
        'v' => KeyCode::KeyV,
        'w' => KeyCode::KeyW,
        'x' => KeyCode::KeyX,
        'y' => KeyCode::KeyY,
        'z' => KeyCode::KeyZ,
        _ => unreachable!("checked is_ascii_alphabetic"),
    }
}

fn digit_keycode(ch: char) -> KeyCode {
    match ch {
        '0' => KeyCode::Digit0,
        '1' => KeyCode::Digit1,
        '2' => KeyCode::Digit2,
        '3' => KeyCode::Digit3,
        '4' => KeyCode::Digit4,
        '5' => KeyCode::Digit5,
        '6' => KeyCode::Digit6,
        '7' => KeyCode::Digit7,
        '8' => KeyCode::Digit8,
        '9' => KeyCode::Digit9,
        _ => unreachable!("checked is_ascii_digit"),
    }
}

fn shifted_digit(ch: char) -> char {
    match ch {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        _ => ch,
    }
}

fn ascii_punct_keycode(ch: char) -> Option<(KeyCode, bool)> {
    Some(match ch {
        '`' => (KeyCode::Backquote, false),
        '~' => (KeyCode::Backquote, true),
        '!' => (KeyCode::Digit1, true),
        '@' => (KeyCode::Digit2, true),
        '#' => (KeyCode::Digit3, true),
        '$' => (KeyCode::Digit4, true),
        '%' => (KeyCode::Digit5, true),
        '^' => (KeyCode::Digit6, true),
        '&' => (KeyCode::Digit7, true),
        '*' => (KeyCode::Digit8, true),
        '(' => (KeyCode::Digit9, true),
        ')' => (KeyCode::Digit0, true),
        '-' => (KeyCode::Minus, false),
        '_' => (KeyCode::Minus, true),
        '=' => (KeyCode::Equal, false),
        '+' => (KeyCode::Equal, true),
        '[' => (KeyCode::BracketLeft, false),
        '{' => (KeyCode::BracketLeft, true),
        ']' => (KeyCode::BracketRight, false),
        '}' => (KeyCode::BracketRight, true),
        '\\' => (KeyCode::Backslash, false),
        '|' => (KeyCode::Backslash, true),
        ';' => (KeyCode::Semicolon, false),
        ':' => (KeyCode::Semicolon, true),
        '\'' => (KeyCode::Quote, false),
        '"' => (KeyCode::Quote, true),
        ',' => (KeyCode::Comma, false),
        '<' => (KeyCode::Comma, true),
        '.' => (KeyCode::Period, false),
        '>' => (KeyCode::Period, true),
        '/' => (KeyCode::Slash, false),
        '?' => (KeyCode::Slash, true),
        _ => return None,
    })
}

/// Format `app.where`: each terminal's body and title-bar rects in logical screen pixels
/// from the window's top-left, plus viewport, camera pose, focused id, wall selection,
/// group frames/headers, and open menu state.
pub fn format_where(described: &WallFrame, cursor: Point) -> String {
    let vw = described.viewport.w;
    let vh = described.viewport.h;
    let cam = described.camera;

    let focused = described
        .terminals
        .iter()
        .find(|t| t.focused)
        .map_or("none", |t| t.id.as_str());

    let selected: Vec<&str> = described
        .terminals
        .iter()
        .filter(|t| t.selected)
        .map(|t| t.id.as_str())
        .collect();
    let selection_str = if selected.is_empty() {
        "none".to_string()
    } else {
        selected.join(",")
    };

    let menu_str = match &described.menu {
        None => "none".to_string(),
        Some(m) => {
            let target = match &m.target {
                MenuTarget::Terminal { id, .. } => format!("terminal:{id}"),
                MenuTarget::Group { id } => format!("group:{id}"),
                MenuTarget::Canvas { .. } => "canvas".to_string(),
            };
            format!("{target} at={:.2},{:.2}", m.at.x, m.at.y)
        }
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "viewport w={vw:.2} h={vh:.2} scale={:.2}",
        described.scale_factor
    ));
    lines.push(format!(
        "camera x={:.4} y={:.4} zoom={:.4} animating={}",
        cam.x, cam.y, cam.zoom, described.animating
    ));
    lines.push(format!("cursor x={:.2} y={:.2}", cursor.x, cursor.y));
    lines.push(format!("focused {focused}"));
    lines.push(format!("selection {selection_str}"));
    lines.push(format!("menu {menu_str}"));

    for t in &described.terminals {
        let sb = cam.to_screen_box(t.body_rect, vw, vh);
        let text_sel = match &t.text_selection {
            None => "none".to_string(),
            Some(sel) => format!("{}:{}..{}:{}", sel.r0, sel.c0, sel.r1, sel.c1),
        };
        lines.push(format!(
            "terminal {} cols={} rows={} focused={} selected={} z={:.2} rect={:.2},{:.2},{:.2},{:.2} world={:.4},{:.4},{:.4},{:.4} text_sel={text_sel}",
            t.id,
            t.cols,
            t.rows,
            t.focused,
            t.selected,
            t.z,
            sb.left,
            sb.top,
            sb.right,
            sb.bottom,
            t.body_rect.min_x,
            t.body_rect.min_y,
            t.body_rect.max_x,
            t.body_rect.max_y,
        ));
    }

    for bar in &described.title_bars {
        let sb = cam.to_screen_box(bar.rect, vw, vh);
        lines.push(format!(
            "titlebar {} rect={:.2},{:.2},{:.2},{:.2} text={:?}",
            bar.terminal_id, sb.left, sb.top, sb.right, sb.bottom, bar.text
        ));
    }

    for g in &described.groups {
        let fb = cam.to_screen_box(g.frame_rect, vw, vh);
        let hb = cam.to_screen_box(g.header_rect, vw, vh);
        lines.push(format!(
            "group {} layout={} focused={} rect={:.2},{:.2},{:.2},{:.2} header={:.2},{:.2},{:.2},{:.2} name={:?}",
            g.id,
            g.layout.as_str(),
            g.focused,
            fb.left,
            fb.top,
            fb.right,
            fb.bottom,
            hb.left,
            hb.top,
            hb.right,
            hb.bottom,
            g.name,
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wall_core::layout::Size;
    use wall_core::wall::{TerminalAppeared, Wall, WallOptions};

    #[test]
    fn parses_pointer_wheel_and_pinch_verbs_and_rejects_bad_inputs() {
        assert_eq!(parse_script_line("wall.spawn"), None);
        assert_eq!(parse_script_line("app.shot /tmp/a.png"), None);

        assert_eq!(
            parse_script_line("app.move 320.5 180").unwrap().unwrap(),
            ScriptCommand::Move { x: 320.5, y: 180.0 }
        );
        assert_eq!(
            parse_script_line("app.down").unwrap().unwrap(),
            ScriptCommand::Down {
                button: MouseButton::Left,
                mods: None,
            }
        );
        assert_eq!(
            parse_script_line("app.down right shift").unwrap().unwrap(),
            ScriptCommand::Down {
                button: MouseButton::Right,
                mods: Some(ModifiersState::SHIFT),
            }
        );
        assert_eq!(
            parse_script_line("app.up middle").unwrap().unwrap(),
            ScriptCommand::Up {
                button: MouseButton::Middle,
                mods: None,
            }
        );
        assert_eq!(
            parse_script_line("app.click 400 300 right")
                .unwrap()
                .unwrap(),
            ScriptCommand::Click {
                x: 400.0,
                y: 300.0,
                button: MouseButton::Right,
                mods: ModifiersState::empty(),
            }
        );
        assert_eq!(
            parse_script_line("app.dblclick 512 164").unwrap().unwrap(),
            ScriptCommand::DblClick { x: 512.0, y: 164.0 }
        );
        assert_eq!(
            parse_script_line("app.drag 10 20 300 400").unwrap().unwrap(),
            ScriptCommand::Drag {
                x0: 10.0,
                y0: 20.0,
                x1: 300.0,
                y1: 400.0,
                steps: DEFAULT_DRAG_STEPS,
                button: MouseButton::Left,
                mods: ModifiersState::empty(),
            }
        );
        assert_eq!(
            parse_script_line("app.drag 10 20 300 400 24 left shift+alt")
                .unwrap()
                .unwrap(),
            ScriptCommand::Drag {
                x0: 10.0,
                y0: 20.0,
                x1: 300.0,
                y1: 400.0,
                steps: 24,
                button: MouseButton::Left,
                mods: ModifiersState::SHIFT | ModifiersState::ALT,
            }
        );
        // Omitting optional steps/button while supplying mods still parses cleanly.
        assert_eq!(
            parse_script_line("app.drag 0 0 200 200 shift")
                .unwrap()
                .unwrap(),
            ScriptCommand::Drag {
                x0: 0.0,
                y0: 0.0,
                x1: 200.0,
                y1: 200.0,
                steps: DEFAULT_DRAG_STEPS,
                button: MouseButton::Left,
                mods: ModifiersState::SHIFT,
            }
        );
        assert_eq!(
            parse_script_line("app.wheel 800 500 -40 120 ctrl")
                .unwrap()
                .unwrap(),
            ScriptCommand::Wheel {
                x: 800.0,
                y: 500.0,
                dx: -40.0,
                dy: 120.0,
                mods: ModifiersState::CONTROL,
            }
        );
        assert_eq!(
            parse_script_line("app.pinch 640 420 1.25").unwrap().unwrap(),
            ScriptCommand::Pinch {
                x: 640.0,
                y: 420.0,
                factor: 1.25,
            }
        );
        assert_eq!(
            parse_script_line("app.where").unwrap().unwrap(),
            ScriptCommand::Where
        );

        assert!(parse_script_line("app.pinch 10 10 0").unwrap().is_err());
        assert!(parse_script_line("app.drag 0 0 10 10 0").unwrap().is_err());
        assert!(parse_script_line("app.down side").unwrap().is_err());
        assert!(parse_script_line("app.wheel 0 0 0 10 hyper").unwrap().is_err());
    }

    #[test]
    fn parses_key_chords_and_typed_strings_matching_winit_and_wall_conventions() {
        let cmd_d = parse_key_chord("cmd+d").unwrap();
        assert_eq!(cmd_d.physical, PhysicalKey::Code(KeyCode::KeyD));
        assert_eq!(cmd_d.logical, Key::Character(SmolStr::new("d")));
        assert_eq!(cmd_d.mods, ModifiersState::SUPER);

        let cmd_shift_d = parse_key_chord("cmd+shift+d").unwrap();
        assert_eq!(cmd_shift_d.physical, PhysicalKey::Code(KeyCode::KeyD));
        assert_eq!(cmd_shift_d.logical, Key::Character(SmolStr::new("D")));
        assert_eq!(cmd_shift_d.mods, ModifiersState::SUPER | ModifiersState::SHIFT);

        let cmd_bs = parse_key_chord("cmd+backslash").unwrap();
        assert_eq!(cmd_bs.physical, PhysicalKey::Code(KeyCode::Backslash));
        assert_eq!(cmd_bs.mods, ModifiersState::SUPER);

        let shift_tab = parse_key_chord("shift+Tab").unwrap();
        assert_eq!(shift_tab.physical, PhysicalKey::Code(KeyCode::Tab));
        assert_eq!(shift_tab.logical, Key::Named(NamedKey::Tab));
        assert_eq!(shift_tab.mods, ModifiersState::SHIFT);

        let enter = parse_key_chord("Enter").unwrap();
        assert_eq!(enter.physical, PhysicalKey::Code(KeyCode::Enter));
        assert_eq!(enter.text, None);

        let typed = match parse_script_line(r#"app.type "tput cols\n""#)
            .unwrap()
            .unwrap()
        {
            ScriptCommand::Type(s) => keys_for_typed_text(&s),
            other => panic!("expected Type, got {other:?}"),
        };
        assert_eq!(typed.len(), 10);
        assert_eq!(typed[4].physical, PhysicalKey::Code(KeyCode::Space));
        assert_eq!(typed[4].text.as_deref(), Some(" "));
        assert_eq!(typed[9].physical, PhysicalKey::Code(KeyCode::Enter));
        assert_eq!(typed[9].text, None);
    }

    #[test]
    fn format_where_projects_terminal_and_titlebar_into_logical_screen_pixels() {
        let mut wall = Wall::in_memory(WallOptions {
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 2.0,
            ..WallOptions::default()
        });
        wall.terminal_appeared(TerminalAppeared::new("t1", 100, 30));
        wall.run("wall.fit");
        // Settle the camera framing ease.
        for i in 1..=60 {
            wall.tick(1.0 / 60.0, f64::from(i) * 16.67);
        }
        let report = format_where(&wall.frame(), Point::new(800.0, 500.0));
        assert!(report.contains("viewport w=1600.00 h=1000.00 scale=2.00"));
        assert!(report.contains("focused t1"));
        assert!(report.contains("terminal t1 cols=100 rows=30 focused=true"));
        assert!(report.contains("titlebar t1 rect="));
    }
}
