//! Scripted window-level input verbs (`app.move`, `app.down`, `app.up`, `app.click`,
//! `app.dblclick`, `app.tplclick`, `app.drag`, `app.wheel`, `app.wheel_seq`, `app.pinch`,
//! `app.doubletap`, `app.drop`, `app.hover_file`, `app.cancel_file`, `app.key`, `app.type`,
//! `app.ime`, `app.focus`, `app.clipboard`, `app.primary`, and `app.where`) for driving a
//! live `--wall` window over `wallctl`.
//!
//! Coordinates are logical pixels from the window's top-left `(0, 0)`, matching
//! `App::cursor` and `Wall`'s viewport coordinate space.

use std::path::PathBuf;

use wall_core::geom::Point;
use wall_core::input::Platform;
use wall_core::menu::MenuTarget;
use wall_core::router::CommandRouter;
use wall_core::wall::WallFrame;
use winit::event::{MouseButton, TouchPhase};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, NativeKeyCode, PhysicalKey, SmolStr};

/// Default interpolation step count for `app.drag` when omitted.
pub const DEFAULT_DRAG_STEPS: usize = 12;

/// Unit of a scripted scroll delta (`MouseScrollDelta::PixelDelta` vs `LineDelta`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScriptScrollUnit {
    /// Logical pixel delta (`MouseScrollDelta::PixelDelta` after `scale_factor` scaling).
    #[default]
    Pixel,
    /// Discrete mouse-wheel detents (`MouseScrollDelta::LineDelta`).
    Line,
}

/// One timed event in an `app.wheel_seq` stream (`[<phase>:]<dx>,<dy>[@<dt_ms>]`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScriptWheelStep {
    pub phase: TouchPhase,
    pub unit: ScriptScrollUnit,
    pub dx: f64,
    pub dy: f64,
    pub dt_ms: f64,
}

/// Amount supplied to `app.pinch`: multiplicative zoom factor or raw `PinchGesture` delta.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScriptPinchAmount {
    /// Multiplicative zoom factor (`> 0`), passed into `App::pinch` as `factor.ln()`.
    Factor(f64),
    /// Raw `WindowEvent::PinchGesture` `delta` (`NSEvent.magnification`), zoomed by `delta.exp()`.
    Delta(f64),
}

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
    /// `app.tplclick <x> <y>`
    TplClick { x: f64, y: f64 },
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
    /// `app.wheel [--line|--pixel] [--phase <phase>] [--dt <ms>] <x> <y> <dx> <dy> [mods]`
    /// (winit delta convention: `+dy` scrolls up / zooms in).
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        unit: ScriptScrollUnit,
        phase: TouchPhase,
        dt_ms: Option<f64>,
        mods: ModifiersState,
    },
    /// `app.wheel_seq <x> <y> [--mods <mods>] [--line] <step...>`
    WheelSeq {
        x: f64,
        y: f64,
        mods: ModifiersState,
        steps: Vec<ScriptWheelStep>,
    },
    /// `app.pinch [--delta] [--phase <phase>] <x> <y> <factor_or_delta>`
    Pinch {
        x: f64,
        y: f64,
        amount: ScriptPinchAmount,
        phase: TouchPhase,
    },
    /// `app.doubletap [x y]` (macOS two-finger smart-zoom `DoubleTapGesture`)
    DoubleTap { at: Option<(f64, f64)> },
    /// `app.drop [--at <x> <y>] <path...>`
    Drop {
        at: Option<(f64, f64)>,
        paths: Vec<PathBuf>,
    },
    /// `app.hover_file <path...>`
    HoverFile(Vec<PathBuf>),
    /// `app.cancel_file`
    CancelFile,
    /// `app.key [--down|--up|--repeat] [--key <logical>] [--text <typed>] <chord>`
    Key(ScriptKey),
    /// `app.type <text>`
    Type(String),
    /// `app.ime preedit <text> | commit <text> | disable`
    Ime(ScriptIme),
    /// `app.focus on|off`
    Focus(bool),
    /// `app.clipboard [clear | set <text>]`
    Clipboard(ScriptClipboard),
    /// `app.primary [clear | set <text>]` (Linux `PRIMARY` selection / memory-mode stand-in)
    Primary(ScriptClipboard),
    /// `app.where`
    Where,
}

/// Phase of a scripted keystroke (`app.key`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScriptKeyPhase {
    /// Press followed immediately by release (default).
    #[default]
    Tap,
    /// Press only (`--down`).
    Down,
    /// Release only (`--up`).
    Up,
    /// Repeated press (`--repeat`, `KeyEvent.repeat = true`).
    Repeat,
}

/// Physical key, logical key, typed text, modifiers, and phase for one synthesized keystroke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptKey {
    pub physical: PhysicalKey,
    pub logical: Key,
    pub text: Option<String>,
    pub mods: ModifiersState,
    pub phase: ScriptKeyPhase,
}

/// Scripted IME composition event (`app.ime`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptIme {
    Preedit(String),
    Commit(String),
    Disable,
}

/// Scripted clipboard inspection or memory-clipboard mutation (`app.clipboard`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptClipboard {
    Get,
    Clear,
    Set(String),
}

/// Modifier flag corresponding to a physical modifier key, if `physical` is one.
pub fn modifier_bit_of_physical(physical: PhysicalKey) -> Option<ModifiersState> {
    match physical {
        PhysicalKey::Code(KeyCode::ShiftLeft | KeyCode::ShiftRight) => Some(ModifiersState::SHIFT),
        PhysicalKey::Code(KeyCode::ControlLeft | KeyCode::ControlRight) => {
            Some(ModifiersState::CONTROL)
        }
        PhysicalKey::Code(KeyCode::AltLeft | KeyCode::AltRight) => Some(ModifiersState::ALT),
        PhysicalKey::Code(KeyCode::SuperLeft | KeyCode::SuperRight) => Some(ModifiersState::SUPER),
        _ => None,
    }
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
        "app.tplclick" => Some(parse_tplclick(args)),
        "app.drag" => Some(parse_drag(args)),
        "app.wheel" => Some(parse_wheel(args)),
        "app.wheel_seq" => Some(parse_wheel_seq(args)),
        "app.pinch" => Some(parse_pinch(args)),
        "app.doubletap" => Some(parse_doubletap(args)),
        "app.drop" => Some(parse_drop_verb(args)),
        "app.hover_file" => Some(parse_hover_file_verb(args)),
        "app.cancel_file" => {
            if !args.is_empty() {
                Some(Err("usage: app.cancel_file".to_string()))
            } else {
                Some(Ok(ScriptCommand::CancelFile))
            }
        }
        "app.key" => Some(parse_key_verb(args)),
        "app.type" => Some(parse_type_verb(args)),
        "app.ime" => Some(parse_ime_verb(args)),
        "app.focus" => Some(parse_focus_verb(args)),
        "app.clipboard" => Some(parse_clipboard_verb("app.clipboard", args, ScriptCommand::Clipboard)),
        "app.primary" => Some(parse_clipboard_verb("app.primary", args, ScriptCommand::Primary)),
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

fn parse_tplclick(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.tplclick <x> <y>";
    if args.len() != 2 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;
    Ok(ScriptCommand::TplClick { x, y })
}

fn parse_doubletap(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.doubletap [x y]";
    match args.len() {
        0 => Ok(ScriptCommand::DoubleTap { at: None }),
        2 => {
            let x = parse_f64_arg(args, 0, "x", USAGE)?;
            let y = parse_f64_arg(args, 1, "y", USAGE)?;
            Ok(ScriptCommand::DoubleTap { at: Some((x, y)) })
        }
        _ => Err(format!("usage: {USAGE}")),
    }
}

pub fn parse_touch_phase(raw: &str) -> Result<TouchPhase, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "started" | "start" | "began" => Ok(TouchPhase::Started),
        "moved" | "move" | "changed" => Ok(TouchPhase::Moved),
        "ended" | "end" => Ok(TouchPhase::Ended),
        "cancelled" | "canceled" | "cancel" => Ok(TouchPhase::Cancelled),
        _ => Err(format!(
            "phase must be started|moved|ended|cancelled, not '{raw}'"
        )),
    }
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
    const USAGE: &str =
        "app.wheel [--line|--pixel] [--phase <started|moved|ended|cancelled>] [--dt <ms>] <x> <y> <dx> <dy> [mods]";
    let mut unit = ScriptScrollUnit::Pixel;
    let mut phase = TouchPhase::Moved;
    let mut dt_ms: Option<f64> = None;
    let mut pos: Vec<String> = Vec::with_capacity(args.len());

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--line" => unit = ScriptScrollUnit::Line,
            "--pixel" => unit = ScriptScrollUnit::Pixel,
            "--phase" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    return Err(format!("missing value after --phase (usage: {USAGE})"));
                };
                phase = parse_touch_phase(val)?;
            }
            "--dt" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    return Err(format!("missing value after --dt (usage: {USAGE})"));
                };
                let d: f64 = val
                    .parse()
                    .map_err(|_| format!("dt must be a number, not '{val}'"))?;
                if !d.is_finite() || d < 0.0 {
                    return Err(format!("dt must be finite and >= 0, not '{val}'"));
                }
                dt_ms = Some(d);
            }
            tok if tok.starts_with("--") => {
                return Err(format!("unknown flag '{tok}' (usage: {USAGE})"));
            }
            tok => pos.push(tok.to_string()),
        }
        i += 1;
    }

    if pos.len() < 4 || pos.len() > 5 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(&pos, 0, "x", USAGE)?;
    let y = parse_f64_arg(&pos, 1, "y", USAGE)?;
    let dx = parse_f64_arg(&pos, 2, "dx", USAGE)?;
    let dy = parse_f64_arg(&pos, 3, "dy", USAGE)?;
    let mods = match pos.get(4) {
        Some(raw) => parse_mods(raw)?,
        None => ModifiersState::empty(),
    };
    Ok(ScriptCommand::Wheel {
        x,
        y,
        dx,
        dy,
        unit,
        phase,
        dt_ms,
        mods,
    })
}

fn parse_wheel_step_token(
    raw: &str,
    default_unit: ScriptScrollUnit,
    usage: &str,
) -> Result<ScriptWheelStep, String> {
    let (body, dt_ms) = match raw.rsplit_once('@') {
        Some((b, dt_raw)) => {
            let d: f64 = dt_raw
                .parse()
                .map_err(|_| format!("step dt must be a number in '{raw}'"))?;
            if !d.is_finite() || d < 0.0 {
                return Err(format!("step dt must be finite and >= 0 in '{raw}'"));
            }
            (b, d)
        }
        None => (raw, 8.0),
    };

    let mut unit = default_unit;
    let mut phase = TouchPhase::Moved;
    let mut rest = body;
    while let Some((prefix, tail)) = rest.split_once(':') {
        match prefix.trim().to_ascii_lowercase().as_str() {
            "line" => unit = ScriptScrollUnit::Line,
            "pixel" | "px" => unit = ScriptScrollUnit::Pixel,
            other => phase = parse_touch_phase(other)?,
        }
        rest = tail;
    }

    let Some((dx_raw, dy_raw)) = rest.split_once(',') else {
        return Err(format!(
            "wheel_seq step '{raw}' must look like [phase:]dx,dy[@dt_ms] (usage: {usage})"
        ));
    };
    let dx: f64 = dx_raw
        .parse()
        .map_err(|_| format!("dx must be a number in step '{raw}'"))?;
    let dy: f64 = dy_raw
        .parse()
        .map_err(|_| format!("dy must be a number in step '{raw}'"))?;
    if !dx.is_finite() || !dy.is_finite() {
        return Err(format!("dx and dy must be finite in step '{raw}'"));
    }
    Ok(ScriptWheelStep {
        phase,
        unit,
        dx,
        dy,
        dt_ms,
    })
}

fn parse_wheel_seq(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str =
        "app.wheel_seq <x> <y> [--mods <mods>] [--line|--pixel] <[phase:]dx,dy[@dt_ms] ...>";
    if args.len() < 3 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(args, 0, "x", USAGE)?;
    let y = parse_f64_arg(args, 1, "y", USAGE)?;

    let mut mods = ModifiersState::empty();
    let mut default_unit = ScriptScrollUnit::Pixel;
    let mut steps = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--mods" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    return Err(format!("missing value after --mods (usage: {USAGE})"));
                };
                mods = parse_mods(val)?;
            }
            "--line" => default_unit = ScriptScrollUnit::Line,
            "--pixel" => default_unit = ScriptScrollUnit::Pixel,
            tok if tok.starts_with("--") => {
                return Err(format!("unknown flag '{tok}' (usage: {USAGE})"));
            }
            tok => steps.push(parse_wheel_step_token(tok, default_unit, USAGE)?),
        }
        i += 1;
    }
    if steps.is_empty() {
        return Err(format!("usage: {USAGE}"));
    }
    Ok(ScriptCommand::WheelSeq { x, y, mods, steps })
}

fn parse_pinch(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str =
        "app.pinch [--delta] [--phase <started|moved|ended|cancelled>] <x> <y> <factor_or_delta>";
    let mut raw_delta = false;
    let mut phase = TouchPhase::Moved;
    let mut pos: Vec<String> = Vec::with_capacity(args.len());

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--delta" => raw_delta = true,
            "--phase" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    return Err(format!("missing value after --phase (usage: {USAGE})"));
                };
                phase = parse_touch_phase(val)?;
            }
            tok if tok.starts_with("--") => {
                return Err(format!("unknown flag '{tok}' (usage: {USAGE})"));
            }
            tok => pos.push(tok.to_string()),
        }
        i += 1;
    }

    if pos.len() != 3 {
        return Err(format!("usage: {USAGE}"));
    }
    let x = parse_f64_arg(&pos, 0, "x", USAGE)?;
    let y = parse_f64_arg(&pos, 1, "y", USAGE)?;
    let amount = if raw_delta {
        let d = parse_f64_arg(&pos, 2, "delta", USAGE)?;
        ScriptPinchAmount::Delta(d)
    } else {
        let factor = parse_f64_arg(&pos, 2, "factor", USAGE)?;
        if factor <= 0.0 {
            return Err(format!("factor must be > 0, not '{factor}'"));
        }
        ScriptPinchAmount::Factor(factor)
    };
    Ok(ScriptCommand::Pinch {
        x,
        y,
        amount,
        phase,
    })
}

fn parse_key_verb(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str =
        "app.key [--down|--up|--repeat] [--key <logical>] [--text <typed>|--no-text] <chord>";
    if args.is_empty() {
        return Err(format!("usage: {USAGE}"));
    }
    let mut phase = ScriptKeyPhase::Tap;
    let mut phase_set = false;
    let mut key_override: Option<String> = None;
    let mut text_override: Option<Option<String>> = None;
    let mut chord_spec: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--down" => {
                if phase_set {
                    return Err("conflicting phase flags in app.key".to_string());
                }
                phase = ScriptKeyPhase::Down;
                phase_set = true;
            }
            "--up" => {
                if phase_set {
                    return Err("conflicting phase flags in app.key".to_string());
                }
                phase = ScriptKeyPhase::Up;
                phase_set = true;
            }
            "--repeat" => {
                if phase_set {
                    return Err("conflicting phase flags in app.key".to_string());
                }
                phase = ScriptKeyPhase::Repeat;
                phase_set = true;
            }
            "--key" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    return Err(format!("missing value after --key (usage: {USAGE})"));
                };
                key_override = Some(val.clone());
            }
            "--text" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    return Err(format!("missing value after --text (usage: {USAGE})"));
                };
                text_override = Some((!val.is_empty()).then(|| val.clone()));
            }
            "--no-text" => {
                text_override = Some(None);
            }
            tok => {
                if chord_spec.is_some() {
                    return Err(format!("unexpected argument '{tok}' (usage: {USAGE})"));
                }
                chord_spec = Some(tok);
            }
        }
        i += 1;
    }

    let Some(spec) = chord_spec else {
        return Err(format!("usage: {USAGE}"));
    };
    let mut parsed = parse_key_chord(spec)?;
    parsed.phase = phase;
    if let Some(raw_key) = key_override {
        parsed.logical = parse_logical_key_override(&raw_key);
        if text_override.is_none() {
            parsed.text = match &parsed.logical {
                Key::Named(NamedKey::Space) => Some(" ".to_string()),
                Key::Character(s) if !s.is_empty() && !s.chars().any(char::is_control) => {
                    Some(s.to_string())
                }
                _ => None,
            };
        }
    }
    if let Some(explicit_text) = text_override {
        parsed.text = explicit_text;
    }
    Ok(ScriptCommand::Key(parsed))
}

fn parse_logical_key_override(raw: &str) -> Key {
    match raw {
        " " | "Space" => Key::Named(NamedKey::Space),
        "Dead" => Key::Dead(None),
        "Unidentified" => Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
        "Enter" => Key::Named(NamedKey::Enter),
        "Tab" => Key::Named(NamedKey::Tab),
        "Escape" => Key::Named(NamedKey::Escape),
        "Backspace" => Key::Named(NamedKey::Backspace),
        "Delete" => Key::Named(NamedKey::Delete),
        "Insert" => Key::Named(NamedKey::Insert),
        "Home" => Key::Named(NamedKey::Home),
        "End" => Key::Named(NamedKey::End),
        "PageUp" => Key::Named(NamedKey::PageUp),
        "PageDown" => Key::Named(NamedKey::PageDown),
        "ArrowUp" => Key::Named(NamedKey::ArrowUp),
        "ArrowDown" => Key::Named(NamedKey::ArrowDown),
        "ArrowLeft" => Key::Named(NamedKey::ArrowLeft),
        "ArrowRight" => Key::Named(NamedKey::ArrowRight),
        "Clear" => Key::Named(NamedKey::Clear),
        "Shift" => Key::Named(NamedKey::Shift),
        "Control" => Key::Named(NamedKey::Control),
        "Alt" => Key::Named(NamedKey::Alt),
        "Meta" | "Super" => Key::Named(NamedKey::Super),
        "CapsLock" => Key::Named(NamedKey::CapsLock),
        "NumLock" => Key::Named(NamedKey::NumLock),
        other => Key::Character(SmolStr::new(other)),
    }
}

fn parse_type_verb(args: &[String]) -> Result<ScriptCommand, String> {
    if args.is_empty() {
        return Err("usage: app.type <text>".to_string());
    }
    Ok(ScriptCommand::Type(args.join(" ")))
}

fn parse_ime_verb(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.ime preedit [text] | commit <text> | disable";
    let Some(sub) = args.first().map(String::as_str) else {
        return Err(format!("usage: {USAGE}"));
    };
    match sub {
        "preedit" => Ok(ScriptCommand::Ime(ScriptIme::Preedit(args[1..].join(" ")))),
        "commit" => {
            if args.len() < 2 {
                return Err(format!("usage: {USAGE}"));
            }
            Ok(ScriptCommand::Ime(ScriptIme::Commit(args[1..].join(" "))))
        }
        "disable" if args.len() == 1 => Ok(ScriptCommand::Ime(ScriptIme::Disable)),
        _ => Err(format!("usage: {USAGE}")),
    }
}

fn parse_focus_verb(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.focus on|off";
    if args.len() != 1 {
        return Err(format!("usage: {USAGE}"));
    }
    match args[0].to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Ok(ScriptCommand::Focus(true)),
        "off" | "false" | "0" => Ok(ScriptCommand::Focus(false)),
        _ => Err(format!("usage: {USAGE}")),
    }
}

fn parse_drop_verb(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.drop [--at <x> <y>] <path...>";
    let mut at: Option<(f64, f64)> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--at" => {
                let x = parse_f64_arg(args, i + 1, "x", USAGE)?;
                let y = parse_f64_arg(args, i + 2, "y", USAGE)?;
                at = Some((x, y));
                i += 3;
            }
            "--" => {
                for rest in &args[i + 1..] {
                    if !rest.is_empty() {
                        paths.push(PathBuf::from(rest));
                    }
                }
                break;
            }
            tok => {
                if !tok.is_empty() {
                    paths.push(PathBuf::from(tok));
                }
                i += 1;
            }
        }
    }
    if paths.is_empty() {
        return Err(format!("usage: {USAGE}"));
    }
    Ok(ScriptCommand::Drop { at, paths })
}

fn parse_hover_file_verb(args: &[String]) -> Result<ScriptCommand, String> {
    const USAGE: &str = "app.hover_file <path...>";
    let paths: Vec<PathBuf> = args
        .iter()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    if paths.is_empty() {
        return Err(format!("usage: {USAGE}"));
    }
    Ok(ScriptCommand::HoverFile(paths))
}

fn parse_clipboard_verb(
    verb: &str,
    args: &[String],
    wrap: fn(ScriptClipboard) -> ScriptCommand,
) -> Result<ScriptCommand, String> {
    let usage = format!("{verb} [clear | set <text>]");
    match args.first().map(String::as_str) {
        None => Ok(wrap(ScriptClipboard::Get)),
        Some("clear") if args.len() == 1 => Ok(wrap(ScriptClipboard::Clear)),
        Some("set") if args.len() >= 2 => Ok(wrap(ScriptClipboard::Set(args[1..].join(" ")))),
        _ => Err(format!("usage: {usage}")),
    }
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
        phase: ScriptKeyPhase::Tap,
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
        "f13" => Ok((KeyCode::F13, Key::Named(NamedKey::F13), None, false)),
        "f14" => Ok((KeyCode::F14, Key::Named(NamedKey::F14), None, false)),
        "f15" => Ok((KeyCode::F15, Key::Named(NamedKey::F15), None, false)),
        "f16" => Ok((KeyCode::F16, Key::Named(NamedKey::F16), None, false)),
        "f17" => Ok((KeyCode::F17, Key::Named(NamedKey::F17), None, false)),
        "f18" => Ok((KeyCode::F18, Key::Named(NamedKey::F18), None, false)),
        "f19" => Ok((KeyCode::F19, Key::Named(NamedKey::F19), None, false)),
        "f20" => Ok((KeyCode::F20, Key::Named(NamedKey::F20), None, false)),
        "numlock" => Ok((KeyCode::NumLock, Key::Named(NamedKey::NumLock), None, false)),
        "numpadenter" | "kp_enter" => Ok((
            KeyCode::NumpadEnter,
            Key::Named(NamedKey::Enter),
            None,
            false,
        )),
        "numpad0" => Ok((
            KeyCode::Numpad0,
            Key::Character(SmolStr::new("0")),
            (!mods.super_key()).then(|| "0".to_string()),
            false,
        )),
        "numpad1" => Ok((
            KeyCode::Numpad1,
            Key::Character(SmolStr::new("1")),
            (!mods.super_key()).then(|| "1".to_string()),
            false,
        )),
        "numpad2" => Ok((
            KeyCode::Numpad2,
            Key::Character(SmolStr::new("2")),
            (!mods.super_key()).then(|| "2".to_string()),
            false,
        )),
        "numpad3" => Ok((
            KeyCode::Numpad3,
            Key::Character(SmolStr::new("3")),
            (!mods.super_key()).then(|| "3".to_string()),
            false,
        )),
        "numpad4" => Ok((
            KeyCode::Numpad4,
            Key::Character(SmolStr::new("4")),
            (!mods.super_key()).then(|| "4".to_string()),
            false,
        )),
        "numpad5" => Ok((
            KeyCode::Numpad5,
            Key::Character(SmolStr::new("5")),
            (!mods.super_key()).then(|| "5".to_string()),
            false,
        )),
        "numpad6" => Ok((
            KeyCode::Numpad6,
            Key::Character(SmolStr::new("6")),
            (!mods.super_key()).then(|| "6".to_string()),
            false,
        )),
        "numpad7" => Ok((
            KeyCode::Numpad7,
            Key::Character(SmolStr::new("7")),
            (!mods.super_key()).then(|| "7".to_string()),
            false,
        )),
        "numpad8" => Ok((
            KeyCode::Numpad8,
            Key::Character(SmolStr::new("8")),
            (!mods.super_key()).then(|| "8".to_string()),
            false,
        )),
        "numpad9" => Ok((
            KeyCode::Numpad9,
            Key::Character(SmolStr::new("9")),
            (!mods.super_key()).then(|| "9".to_string()),
            false,
        )),
        "numpadadd" => Ok((
            KeyCode::NumpadAdd,
            Key::Character(SmolStr::new("+")),
            (!mods.super_key()).then(|| "+".to_string()),
            false,
        )),
        "numpadsubtract" => Ok((
            KeyCode::NumpadSubtract,
            Key::Character(SmolStr::new("-")),
            (!mods.super_key()).then(|| "-".to_string()),
            false,
        )),
        "numpadmultiply" => Ok((
            KeyCode::NumpadMultiply,
            Key::Character(SmolStr::new("*")),
            (!mods.super_key()).then(|| "*".to_string()),
            false,
        )),
        "numpaddivide" => Ok((
            KeyCode::NumpadDivide,
            Key::Character(SmolStr::new("/")),
            (!mods.super_key()).then(|| "/".to_string()),
            false,
        )),
        "numpaddecimal" => Ok((
            KeyCode::NumpadDecimal,
            Key::Character(SmolStr::new(".")),
            (!mods.super_key()).then(|| ".".to_string()),
            false,
        )),
        "numpadequal" => Ok((
            KeyCode::NumpadEqual,
            Key::Character(SmolStr::new("=")),
            (!mods.super_key()).then(|| "=".to_string()),
            false,
        )),
        "shift" | "shiftleft" => Ok((KeyCode::ShiftLeft, Key::Named(NamedKey::Shift), None, false)),
        "shiftright" => Ok((KeyCode::ShiftRight, Key::Named(NamedKey::Shift), None, false)),
        "ctrl" | "control" | "controlleft" => Ok((
            KeyCode::ControlLeft,
            Key::Named(NamedKey::Control),
            None,
            false,
        )),
        "controlright" => Ok((
            KeyCode::ControlRight,
            Key::Named(NamedKey::Control),
            None,
            false,
        )),
        "alt" | "opt" | "option" | "altleft" => {
            Ok((KeyCode::AltLeft, Key::Named(NamedKey::Alt), None, false))
        }
        "altright" => Ok((KeyCode::AltRight, Key::Named(NamedKey::Alt), None, false)),
        "cmd" | "command" | "meta" | "super" | "metaleft" | "superleft" => {
            Ok((KeyCode::SuperLeft, Key::Named(NamedKey::Super), None, false))
        }
        "metaright" | "superright" => Ok((
            KeyCode::SuperRight,
            Key::Named(NamedKey::Super),
            None,
            false,
        )),
        "capslock" => Ok((
            KeyCode::CapsLock,
            Key::Named(NamedKey::CapsLock),
            None,
            false,
        )),
        "contextmenu" | "menu" => Ok((
            KeyCode::ContextMenu,
            Key::Named(NamedKey::ContextMenu),
            None,
            false,
        )),
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
                phase: ScriptKeyPhase::Tap,
            }),
            '\t' => out.push(ScriptKey {
                physical: PhysicalKey::Code(KeyCode::Tab),
                logical: Key::Named(NamedKey::Tab),
                text: None,
                mods: ModifiersState::empty(),
                phase: ScriptKeyPhase::Tap,
            }),
            ' ' => out.push(ScriptKey {
                physical: PhysicalKey::Code(KeyCode::Space),
                logical: Key::Named(NamedKey::Space),
                text: Some(" ".to_string()),
                mods: ModifiersState::empty(),
                phase: ScriptKeyPhase::Tap,
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
                    phase: ScriptKeyPhase::Tap,
                });
            }
            c if c.is_ascii_digit() => {
                let s = c.to_string();
                out.push(ScriptKey {
                    physical: PhysicalKey::Code(digit_keycode(c)),
                    logical: Key::Character(SmolStr::new(&s)),
                    text: Some(s),
                    mods: ModifiersState::empty(),
                    phase: ScriptKeyPhase::Tap,
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
                        phase: ScriptKeyPhase::Tap,
                    });
                } else {
                    out.push(ScriptKey {
                        physical: PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
                        logical: Key::Character(SmolStr::new(&s)),
                        text: Some(s),
                        mods: ModifiersState::empty(),
                        phase: ScriptKeyPhase::Tap,
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
/// from the window's top-left, plus viewport, platform chord set, camera pose, focused id,
/// wall selection, group frames/headers, and open menu state.
pub fn format_where(described: &WallFrame, cursor: Point, platform: Platform) -> String {
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
                // A terminal's row in the sidebar, the one part a hidden
                // terminal has, and an agent's row there.
                MenuTarget::Terminal {
                    id,
                    part: wall_core::menu::TerminalPart::Row,
                } => format!("row:{id}"),
                MenuTarget::Terminal { id, .. } => format!("terminal:{id}"),
                MenuTarget::Group { id } => format!("group:{id}"),
                MenuTarget::Canvas { .. } => "canvas".to_string(),
                MenuTarget::Agent { target } => format!("agent:{target}"),
            };
            format!("{target} at={:.2},{:.2}", m.at.x, m.at.y)
        }
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "viewport w={vw:.2} h={vh:.2} scale={:.2}",
        described.scale_factor
    ));
    let (platform_name, chord, copy_chord, paste_chord, ctrl_click_menu) = match platform {
        Platform::Mac => ("mac", "cmd", "cmd+c", "cmd+v", true),
        Platform::Other => ("other", "ctrl+alt", "ctrl+shift+c", "ctrl+shift+v", false),
    };
    lines.push(format!(
        "platform {platform_name} chord={chord} copy={copy_chord} paste={paste_chord} ctrl_click_menu={ctrl_click_menu}"
    ));
    lines.push(format!(
        "camera x={:.4} y={:.4} zoom={:.4} animating={}",
        cam.x, cam.y, cam.zoom, described.animating
    ));
    lines.push(format!("cursor x={:.2} y={:.2}", cursor.x, cursor.y));
    lines.push(format!("focused {focused}"));
    lines.push(format!("selection {selection_str}"));
    lines.push(format!("menu {menu_str}"));
    lines.push(format!(
        "panels sidebar={} status={} settings={} help={} palette={} grid={}",
        described.panels.sidebar.open,
        described.panels.status_window.open,
        described.panels.settings_panel.open,
        described.panels.help_panel.open,
        described.palette_open,
        described.paper.grid_show,
    ));
    // The termd banner (`wall_core::status::notice_for`): its kind, and its text to the
    // end of the line.
    lines.push(match &described.status_notice {
        Some(notice) => format!(
            "termd_notice kind={} dismiss={} text={}",
            notice.kind.as_str(),
            notice.dismiss,
            notice.text.replace('\n', " ")
        ),
        None => "termd_notice none".to_string(),
    });
    let fs_str = match &described.fullscreen_backdrop {
        None => "none".to_string(),
        Some(fb) => format!("{:?}:{}", fb.mode, fb.target).to_ascii_lowercase(),
    };
    lines.push(format!("fullscreen {fs_str}"));

    for t in &described.terminals {
        let sb = cam.to_screen_box(t.body_rect, vw, vh);
        let text_sel = match &t.text_selection {
            None => "none".to_string(),
            Some(sel) if sel.block => {
                format!("block:{}:{}..{}:{}", sel.r0, sel.c0, sel.r1, sel.c1)
            }
            Some(sel) => format!("{}:{}..{}:{}", sel.r0, sel.c0, sel.r1, sel.c1),
        };
        let hold_str = if t.close_hold_progress.is_some() {
            "active"
        } else {
            "none"
        };
        lines.push(format!(
            "terminal {} cols={} rows={} focused={} selected={} z={:.2} rect={:.2},{:.2},{:.2},{:.2} world={:.4},{:.4},{:.4},{:.4} text_sel={text_sel} close_hold={hold_str}",
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
                unit: ScriptScrollUnit::Pixel,
                phase: TouchPhase::Moved,
                dt_ms: None,
                mods: ModifiersState::CONTROL,
            }
        );
        assert_eq!(
            parse_script_line("app.wheel --line --phase started --dt 12 400 300 0 -1")
                .unwrap()
                .unwrap(),
            ScriptCommand::Wheel {
                x: 400.0,
                y: 300.0,
                dx: 0.0,
                dy: -1.0,
                unit: ScriptScrollUnit::Line,
                phase: TouchPhase::Started,
                dt_ms: Some(12.0),
                mods: ModifiersState::empty(),
            }
        );
        assert_eq!(
            parse_script_line("app.wheel_seq 24 24 started:0,0@0 moved:-30,-20@8 ended:0,0@8")
                .unwrap()
                .unwrap(),
            ScriptCommand::WheelSeq {
                x: 24.0,
                y: 24.0,
                mods: ModifiersState::empty(),
                steps: vec![
                    ScriptWheelStep {
                        phase: TouchPhase::Started,
                        unit: ScriptScrollUnit::Pixel,
                        dx: 0.0,
                        dy: 0.0,
                        dt_ms: 0.0,
                    },
                    ScriptWheelStep {
                        phase: TouchPhase::Moved,
                        unit: ScriptScrollUnit::Pixel,
                        dx: -30.0,
                        dy: -20.0,
                        dt_ms: 8.0,
                    },
                    ScriptWheelStep {
                        phase: TouchPhase::Ended,
                        unit: ScriptScrollUnit::Pixel,
                        dx: 0.0,
                        dy: 0.0,
                        dt_ms: 8.0,
                    },
                ],
            }
        );
        assert_eq!(
            parse_script_line("app.pinch 640 420 1.25").unwrap().unwrap(),
            ScriptCommand::Pinch {
                x: 640.0,
                y: 420.0,
                amount: ScriptPinchAmount::Factor(1.25),
                phase: TouchPhase::Moved,
            }
        );
        assert_eq!(
            parse_script_line("app.pinch --delta --phase moved 640 420 -0.25")
                .unwrap()
                .unwrap(),
            ScriptCommand::Pinch {
                x: 640.0,
                y: 420.0,
                amount: ScriptPinchAmount::Delta(-0.25),
                phase: TouchPhase::Moved,
            }
        );
        assert_eq!(
            parse_script_line("app.tplclick 120 240").unwrap().unwrap(),
            ScriptCommand::TplClick { x: 120.0, y: 240.0 }
        );
        assert_eq!(
            parse_script_line("app.doubletap 500 350").unwrap().unwrap(),
            ScriptCommand::DoubleTap {
                at: Some((500.0, 350.0))
            }
        );
        assert_eq!(
            parse_script_line("app.doubletap").unwrap().unwrap(),
            ScriptCommand::DoubleTap { at: None }
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
        let report = format_where(&wall.frame(), Point::new(800.0, 500.0), Platform::Mac);
        assert!(report.contains("viewport w=1600.00 h=1000.00 scale=2.00"));
        assert!(report.contains("platform mac chord=cmd copy=cmd+c paste=cmd+v ctrl_click_menu=true"));
        assert!(report.contains("focused t1"));
        assert!(report.contains("panels sidebar=false status=false settings=false help=false palette=false"));
        assert!(report.contains("\ntermd_notice none\n"));
        assert!(report.contains("fullscreen none"));
        assert!(report.contains("terminal t1 cols=100 rows=30 focused=true"));
        assert!(report.contains("close_hold=none"));
        assert!(report.contains("titlebar t1 rect="));

        let other_report = format_where(&wall.frame(), Point::new(800.0, 500.0), Platform::Other);
        assert!(other_report.contains(
            "platform other chord=ctrl+alt copy=ctrl+shift+c paste=ctrl+shift+v ctrl_click_menu=false"
        ));
    }

    #[test]
    fn parses_extended_key_flags_ime_focus_clipboard_and_numpad_modifiers() {
        let opt_b = match parse_script_line("app.key --key ∫ --text ∫ alt+KeyB")
            .unwrap()
            .unwrap()
        {
            ScriptCommand::Key(k) => k,
            other => panic!("expected Key, got {other:?}"),
        };
        assert_eq!(opt_b.physical, PhysicalKey::Code(KeyCode::KeyB));
        assert_eq!(opt_b.logical, Key::Character(SmolStr::new("∫")));
        assert_eq!(opt_b.text.as_deref(), Some("∫"));
        assert_eq!(opt_b.mods, ModifiersState::ALT);
        assert_eq!(opt_b.phase, ScriptKeyPhase::Tap);

        let dead = match parse_script_line("app.key --key Dead --no-text alt+KeyE")
            .unwrap()
            .unwrap()
        {
            ScriptCommand::Key(k) => k,
            other => panic!("expected Key, got {other:?}"),
        };
        assert_eq!(dead.logical, Key::Dead(None));
        assert_eq!(dead.text, None);

        let down_cmd = match parse_script_line("app.key --down cmd").unwrap().unwrap() {
            ScriptCommand::Key(k) => k,
            other => panic!("expected Key, got {other:?}"),
        };
        assert_eq!(down_cmd.physical, PhysicalKey::Code(KeyCode::SuperLeft));
        assert_eq!(down_cmd.phase, ScriptKeyPhase::Down);

        let rep_kp = match parse_script_line("app.key --repeat NumpadEnter")
            .unwrap()
            .unwrap()
        {
            ScriptCommand::Key(k) => k,
            other => panic!("expected Key, got {other:?}"),
        };
        assert_eq!(rep_kp.physical, PhysicalKey::Code(KeyCode::NumpadEnter));
        assert_eq!(rep_kp.phase, ScriptKeyPhase::Repeat);

        assert_eq!(
            parse_script_line("app.ime preedit ´").unwrap().unwrap(),
            ScriptCommand::Ime(ScriptIme::Preedit("´".to_string()))
        );
        assert_eq!(
            parse_script_line("app.ime commit é").unwrap().unwrap(),
            ScriptCommand::Ime(ScriptIme::Commit("é".to_string()))
        );
        assert_eq!(
            parse_script_line("app.focus off").unwrap().unwrap(),
            ScriptCommand::Focus(false)
        );
        assert_eq!(
            parse_script_line("app.clipboard").unwrap().unwrap(),
            ScriptCommand::Clipboard(ScriptClipboard::Get)
        );
        assert_eq!(
            parse_script_line("app.clipboard clear").unwrap().unwrap(),
            ScriptCommand::Clipboard(ScriptClipboard::Clear)
        );
        assert_eq!(
            parse_script_line("app.clipboard set hello world").unwrap().unwrap(),
            ScriptCommand::Clipboard(ScriptClipboard::Set("hello world".to_string()))
        );
        assert_eq!(
            parse_script_line("app.primary set linux primary").unwrap().unwrap(),
            ScriptCommand::Primary(ScriptClipboard::Set("linux primary".to_string()))
        );
        assert_eq!(
            parse_script_line(r#"app.drop --at 420 310 /tmp/a.txt "/tmp/my file's.txt""#)
                .unwrap()
                .unwrap(),
            ScriptCommand::Drop {
                at: Some((420.0, 310.0)),
                paths: vec![
                    PathBuf::from("/tmp/a.txt"),
                    PathBuf::from("/tmp/my file's.txt"),
                ],
            }
        );
        assert_eq!(
            parse_script_line("app.hover_file /tmp/dragged.rs")
                .unwrap()
                .unwrap(),
            ScriptCommand::HoverFile(vec![PathBuf::from("/tmp/dragged.rs")])
        );
        assert_eq!(
            parse_script_line("app.cancel_file").unwrap().unwrap(),
            ScriptCommand::CancelFile
        );
    }
}
