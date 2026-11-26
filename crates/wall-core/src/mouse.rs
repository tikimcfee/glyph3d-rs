//! Port of `wall/WallMouse.js`: the pure exports (`button_of`, `mods_of`,
//! `program_gets`, `held_button`) plus `WallMouse`'s decisions over plain data
//! (`effective_tracking`, `claims_press`, and [`WallMouseState`] for gesture,
//! button-chord, cell-deduplicated drag, cancel, and any-event hover routing).
//!
//! Only the focused terminal's body hands pointer presses to a mouse-reporting
//! program (DECSET 9 / 1000 / 1002 / 1003), and Shift (Ghostty's
//! `mouse-shift-capture` bypass) or Alt on the left button (the wall's ⌥-drag
//! move) keeps the press for the wall instead.

use crate::input::{Button, Mods};

/// Pointer button names understood by `termd`'s `mouse` op (`BUTTONS` in `wall/WallMouse.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProgramButton {
    Left,
    Middle,
    Right,
    /// Fourth button (`8` on the `termd` / X11 wire).
    Back,
    /// Fifth button (`9` on the `termd` / X11 wire).
    Forward,
}

impl From<Button> for ProgramButton {
    fn from(button: Button) -> Self {
        match button {
            Button::Left => Self::Left,
            Button::Middle => Self::Middle,
            Button::Right => Self::Right,
            Button::Back => Self::Back,
            Button::Forward => Self::Forward,
        }
    }
}

/// DOM `MouseEvent.buttons` bitmask flag for the primary (left) button.
pub const BUTTON_BIT_LEFT: u8 = 1;
/// DOM `MouseEvent.buttons` bitmask flag for the secondary (right) button.
pub const BUTTON_BIT_RIGHT: u8 = 2;
/// DOM `MouseEvent.buttons` bitmask flag for the auxiliary (middle) button.
pub const BUTTON_BIT_MIDDLE: u8 = 4;
/// DOM `MouseEvent.buttons` bitmask flag for the fourth (back) button.
pub const BUTTON_BIT_BACK: u8 = 8;
/// DOM `MouseEvent.buttons` bitmask flag for the fifth (forward) button.
pub const BUTTON_BIT_FORWARD: u8 = 16;

/// `BUTTONS` paired with `BITS` in DOM button index order (`0..=4`).
const BUTTON_SLOTS: [(ProgramButton, u8); 5] = [
    (ProgramButton::Left, BUTTON_BIT_LEFT),
    (ProgramButton::Middle, BUTTON_BIT_MIDDLE),
    (ProgramButton::Right, BUTTON_BIT_RIGHT),
    (ProgramButton::Back, BUTTON_BIT_BACK),
    (ProgramButton::Forward, BUTTON_BIT_FORWARD),
];

impl ProgramButton {
    /// The DOM `MouseEvent.buttons` bitmask bit corresponding to this button.
    pub const fn dom_bit(self) -> u8 {
        match self {
            Self::Left => BUTTON_BIT_LEFT,
            Self::Middle => BUTTON_BIT_MIDDLE,
            Self::Right => BUTTON_BIT_RIGHT,
            Self::Back => BUTTON_BIT_BACK,
            Self::Forward => BUTTON_BIT_FORWARD,
        }
    }
}

/// Map a DOM `MouseEvent.button` index (`0..=4`) to the program's button, or
/// `None` for any button index the terminal protocol does not name (`buttonOf`
/// in `wall/WallMouse.js`).
pub const fn button_of(dom_button: i32) -> Option<ProgramButton> {
    match dom_button {
        0 => Some(ProgramButton::Left),
        1 => Some(ProgramButton::Middle),
        2 => Some(ProgramButton::Right),
        3 => Some(ProgramButton::Back),
        4 => Some(ProgramButton::Forward),
        _ => None,
    }
}

/// Held modifiers as W3C key names (`"Shift"`, `"Control"`, `"Alt"`, `"Meta"`),
/// matching `termd`'s `mouse` op (`modsOf` in `wall/WallMouse.js`).
pub fn mods_of(mods: Mods) -> Vec<&'static str> {
    let mut names = Vec::new();
    if mods.shift {
        names.push("Shift");
    }
    if mods.ctrl {
        names.push("Control");
    }
    if mods.alt {
        names.push("Alt");
    }
    if mods.meta {
        names.push("Meta");
    }
    names
}

/// Which button a drag move reports while `held_mask` (a DOM `buttons` mask) is
/// down: left, else middle, else right, else back, else forward — xterm's
/// priority order — or `None` (`'none'` in JS) when no button is held
/// (`heldButton` in `wall/WallMouse.js`).
pub const fn held_button(held_mask: u8) -> Option<ProgramButton> {
    let mut index = 0;
    while index < BUTTON_SLOTS.len() {
        let (button, bit) = BUTTON_SLOTS[index];
        if (held_mask & bit) != 0 {
            return Some(button);
        }
        index += 1;
    }
    None
}

/// DECSET mouse tracking mode active on a terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MouseTracking {
    #[default]
    None,
    /// DECSET 9 (X10 mouse reporting).
    X10,
    /// DECSET 1000 (normal press and release reporting).
    Normal,
    /// DECSET 1002 (button-event drag tracking).
    Button,
    /// DECSET 1003 (any-event hover and drag tracking).
    Any,
}

/// Which part of a terminal window a pointer hit landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HitPart {
    Body,
    Title,
    Edge,
}

/// Press event fields inspected by [`program_gets`] and [`claims_press`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressInput {
    /// DOM button index (`0` = left, `1` = middle, `2` = right, `3` = back, `4` = forward).
    pub button: i32,
    pub shift_key: bool,
    pub alt_key: bool,
}

impl PressInput {
    /// Construct from a DOM button index and modifier state.
    pub const fn new(button: i32, mods: Mods) -> Self {
        Self {
            button,
            shift_key: mods.shift,
            alt_key: mods.alt,
        }
    }

    /// Construct from a shared [`Button`] and [`Mods`].
    pub const fn from_button(button: Button, mods: Mods) -> Self {
        let index = match button {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            Button::Back => 3,
            Button::Forward => 4,
        };
        Self::new(index, mods)
    }
}

/// Target facts at the press point for [`program_gets`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetAt {
    pub part: HitPart,
    pub focused: bool,
    pub tracking: MouseTracking,
}

/// Whether a pointer press goes to the terminal program: on the focused
/// terminal's body, its program reporting, a known button, no Shift, and no Alt
/// on the left button (`programGets` in `wall/WallMouse.js`).
pub const fn program_gets(press: PressInput, at: TargetAt) -> bool {
    matches!(at.part, HitPart::Body)
        && at.focused
        && !matches!(at.tracking, MouseTracking::None)
        && button_of(press.button).is_some()
        && !press.shift_key
        && !(press.alt_key && press.button == 0)
}

/// What the program of `terminal_id` hears right now: its own `terminal_tracking`
/// when `focused_id == Some(terminal_id)`, else [`MouseTracking::None`]
/// (`WallMouse._tracking` in `wall/WallMouse.js`).
pub fn effective_tracking(
    terminal_id: &str,
    focused_id: Option<&str>,
    terminal_tracking: MouseTracking,
) -> MouseTracking {
    if focused_id == Some(terminal_id) {
        terminal_tracking
    } else {
        MouseTracking::None
    }
}

/// Plain-data description of a hit on a terminal window (`None` when the pointer
/// hit empty canvas, a group header, or a split divider).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalHit<'a> {
    pub id: &'a str,
    pub part: HitPart,
    pub tracking: MouseTracking,
}

/// Decide whether a press (or double-click) on `hit` is claimed by the hit
/// terminal's mouse-reporting program (`WallMouse.claims` and the gate of
/// `WallMouse.down` in `wall/WallMouse.js`).
pub fn claims_press(
    press: PressInput,
    hit: Option<TerminalHit<'_>>,
    focused_id: Option<&str>,
) -> bool {
    let Some(hit) = hit else {
        return false;
    };
    let focused = focused_id == Some(hit.id);
    let tracking = effective_tracking(hit.id, focused_id, hit.tracking);
    program_gets(
        press,
        TargetAt {
            part: hit.part,
            focused,
            tracking,
        },
    )
}

/// Pointer location in a terminal's cell coordinates (`TerminalGrid.cellAt`):
/// `x` and `y` are fractional cell coordinates clamped to `[0, cols]` × `[0, rows]`
/// (sent to `termd` as `col` and `row`), while `col` and `row` are the zero-based
/// integer cell indices used to deduplicate per-cell moves.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CellPoint {
    pub x: f64,
    pub y: f64,
    pub col: u32,
    pub row: u32,
}

/// Mouse action sent to `termd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseAction {
    Press,
    Release,
    Move,
}

/// One outgoing mouse report emitted by [`WallMouseState`].
#[derive(Clone, Debug, PartialEq)]
pub struct MouseReport {
    pub terminal_id: String,
    pub action: MouseAction,
    /// `None` corresponds to `'none'` (any-event hover or a move with no button bit).
    pub button: Option<ProgramButton>,
    pub col: f64,
    pub row: f64,
    pub mods: Vec<&'static str>,
}

#[derive(Clone, Debug)]
struct ActiveGesture {
    id: String,
    held: u8,
    cell: Option<(u32, u32)>,
    last: Option<(CellPoint, Mods)>,
}

/// Plain-data port of `WallMouse`'s gesture and hover state machine, decoupled
/// from DOM events, camera projection, and live grid instances.
#[derive(Clone, Debug, Default)]
pub struct WallMouseState {
    gesture: Option<ActiveGesture>,
    hover_cell: Option<(String, u32, u32)>,
}

impl WallMouseState {
    /// Construct an idle [`WallMouseState`].
    pub const fn new() -> Self {
        Self {
            gesture: None,
            hover_cell: None,
        }
    }

    /// Whether an active pointer gesture belongs to a terminal program.
    pub const fn busy(&self) -> bool {
        self.gesture.is_some()
    }

    /// Whether a press or double-click on `hit` would be claimed by the program.
    pub fn claims(
        &self,
        press: PressInput,
        hit: Option<TerminalHit<'_>>,
        focused_id: Option<&str>,
    ) -> bool {
        claims_press(press, hit, focused_id)
    }

    /// Handle `pointerdown`: if claimed by the program, starts a gesture and
    /// returns `Some(reports)` to send; otherwise returns `None` (`WallMouse.down`).
    pub fn down(
        &mut self,
        press: PressInput,
        mods: Mods,
        hit: Option<TerminalHit<'_>>,
        focused_id: Option<&str>,
        cell: CellPoint,
    ) -> Option<Vec<MouseReport>> {
        let target = hit?;
        if !claims_press(press, Some(target), focused_id) {
            return None;
        }
        let bit = button_of(press.button)?.dom_bit();
        self.gesture = Some(ActiveGesture {
            id: target.id.to_owned(),
            held: 0,
            cell: None,
            last: None,
        });
        let mut reports = Vec::new();
        self.sync_buttons(bit, cell, mods, &mut reports);
        Some(reports)
    }

    /// Handle `pointermove` while a program gesture is active (`WallMouse.move`).
    pub fn pointer_move(
        &mut self,
        buttons_mask: u8,
        mods: Mods,
        terminal_alive: bool,
        terminal_tracking: MouseTracking,
        cell: CellPoint,
    ) -> Vec<MouseReport> {
        let mut reports = Vec::new();
        if self.gesture.is_none() {
            return reports;
        }
        if !terminal_alive {
            self.gesture = None;
            return reports;
        }
        self.sync_buttons(buttons_mask, cell, mods, &mut reports);

        let Some(gesture) = self.gesture.as_mut() else {
            return reports;
        };
        if gesture.held == 0 {
            return reports;
        }
        if !matches!(terminal_tracking, MouseTracking::Button | MouseTracking::Any) {
            return reports;
        }
        let key = (cell.col, cell.row);
        if gesture.cell == Some(key) {
            return reports;
        }
        gesture.cell = Some(key);
        reports.push(MouseReport {
            terminal_id: gesture.id.clone(),
            action: MouseAction::Move,
            button: held_button(gesture.held),
            col: cell.x,
            row: cell.y,
            mods: mods_of(mods),
        });
        reports
    }

    /// Handle `pointerup` ending the active program gesture (`WallMouse.up`).
    pub fn up(
        &mut self,
        buttons_mask: u8,
        mods: Mods,
        terminal_alive: bool,
        cell: CellPoint,
    ) -> Vec<MouseReport> {
        let mut reports = Vec::new();
        if self.gesture.is_none() {
            return reports;
        }
        if terminal_alive {
            self.sync_buttons(buttons_mask, cell, mods, &mut reports);
        }
        self.gesture = None;
        reports
    }

    /// Handle `pointercancel` / lost pointer capture: release every button still
    /// held at the last button-transition state (`WallMouse.cancel`).
    pub fn cancel(&mut self, terminal_alive: bool) -> Vec<MouseReport> {
        let Some(gesture) = self.gesture.take() else {
            return Vec::new();
        };
        let mut reports = Vec::new();
        if !terminal_alive {
            return reports;
        }
        let Some((last_cell, last_mods)) = gesture.last else {
            return reports;
        };
        for (button, bit) in BUTTON_SLOTS {
            if (gesture.held & bit) != 0 {
                reports.push(MouseReport {
                    terminal_id: gesture.id.clone(),
                    action: MouseAction::Release,
                    button: Some(button),
                    col: last_cell.x,
                    row: last_cell.y,
                    mods: mods_of(last_mods),
                });
            }
        }
        reports
    }

    /// Handle unpressed pointer hover (`WallMouse.hover`): emits a `'move'` with
    /// button `None` once per cell over the focused body when DECSET 1003 (`Any`)
    /// tracking is active and Shift is not held.
    pub fn hover(
        &mut self,
        mods: Mods,
        hit: Option<TerminalHit<'_>>,
        focused_id: Option<&str>,
        cell: CellPoint,
    ) -> Option<MouseReport> {
        let body_hit = hit.filter(|h| h.part == HitPart::Body);
        let Some(target) = body_hit else {
            self.hover_cell = None;
            return None;
        };
        if effective_tracking(target.id, focused_id, target.tracking) != MouseTracking::Any
            || mods.shift
        {
            self.hover_cell = None;
            return None;
        }
        let key = (target.id.to_owned(), cell.col, cell.row);
        if self.hover_cell.as_ref() == Some(&key) {
            return None;
        }
        self.hover_cell = Some(key);
        Some(MouseReport {
            terminal_id: target.id.to_owned(),
            action: MouseAction::Move,
            button: None,
            col: cell.x,
            row: cell.y,
            mods: mods_of(mods),
        })
    }

    fn sync_buttons(
        &mut self,
        now_mask: u8,
        cell: CellPoint,
        mods: Mods,
        reports: &mut Vec<MouseReport>,
    ) {
        let Some(gesture) = self.gesture.as_mut() else {
            return;
        };
        if now_mask == gesture.held {
            return;
        }
        for (button, bit) in BUTTON_SLOTS {
            if (now_mask & bit) == (gesture.held & bit) {
                continue;
            }
            let action = if (now_mask & bit) != 0 {
                MouseAction::Press
            } else {
                MouseAction::Release
            };
            reports.push(MouseReport {
                terminal_id: gesture.id.clone(),
                action,
                button: Some(button),
                col: cell.x,
                row: cell.y,
                mods: mods_of(mods),
            });
        }
        gesture.held = now_mask;
        gesture.cell = Some((cell.col, cell.row));
        gesture.last = Some((cell, mods));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(button: i32, shift: bool, alt: bool) -> PressInput {
        PressInput {
            button,
            shift_key: shift,
            alt_key: alt,
        }
    }

    /// Stand-in 80 × 24 terminal cell lookup matching `tools/wall-mouse.test.mjs`'s
    /// `standIn`: client x = 10·world x, client y = −10·world y, stride (1, 2).
    fn cell_at_80x24(client_x: f64, client_y: f64) -> CellPoint {
        let world_x = client_x / 10.0;
        let world_y = -client_y / 10.0;
        let x = world_x.clamp(0.0, 80.0);
        let y = ((1.0 - world_y) / 2.0).clamp(0.0, 24.0);
        CellPoint {
            x,
            y,
            col: (x.floor() as u32).min(79),
            row: (y.floor() as u32).min(23),
        }
    }

    fn client_at(col: u32, row: u32, fx: f64, fy: f64) -> (f64, f64) {
        (
            10.0 * (f64::from(col) + fx),
            20.0 * (f64::from(row) + fy) - 10.0,
        )
    }

    fn summary(report: &MouseReport) -> String {
        let act = match report.action {
            MouseAction::Press => "press",
            MouseAction::Release => "release",
            MouseAction::Move => "move",
        };
        let btn = match report.button {
            Some(ProgramButton::Left) => "left",
            Some(ProgramButton::Middle) => "middle",
            Some(ProgramButton::Right) => "right",
            Some(ProgramButton::Back) => "8",
            Some(ProgramButton::Forward) => "9",
            None => "none",
        };
        format!(
            "{act} {btn} {},{}",
            report.col.floor() as i64,
            report.row.floor() as i64
        )
    }

    #[test]
    fn press_is_programs_on_focused_reporting_body_without_shift_or_left_alt() {
        let on = TargetAt {
            part: HitPart::Body,
            focused: true,
            tracking: MouseTracking::Normal,
        };
        assert!(program_gets(press(0, false, false), on));
        assert!(program_gets(press(1, false, false), on));
        assert!(program_gets(press(2, false, false), on));
        assert!(program_gets(press(3, false, false), on));
        assert!(!program_gets(press(0, true, false), on));
        assert!(!program_gets(press(2, true, false), on));
        assert!(!program_gets(press(0, false, true), on));
        assert!(program_gets(press(2, false, true), on));
        assert!(!program_gets(
            press(0, false, false),
            TargetAt {
                focused: false,
                ..on
            }
        ));
        assert!(!program_gets(
            press(0, false, false),
            TargetAt {
                tracking: MouseTracking::None,
                ..on
            }
        ));
        assert!(!program_gets(
            press(0, false, false),
            TargetAt {
                part: HitPart::Title,
                ..on
            }
        ));
        assert!(!program_gets(
            press(0, false, false),
            TargetAt {
                part: HitPart::Edge,
                ..on
            }
        ));
        assert!(!program_gets(press(5, false, false), on));

        let mapped: Vec<Option<ProgramButton>> = [0, 1, 2, 3, 4, 5].into_iter().map(button_of).collect();
        assert_eq!(
            mapped,
            vec![
                Some(ProgramButton::Left),
                Some(ProgramButton::Middle),
                Some(ProgramButton::Right),
                Some(ProgramButton::Back),
                Some(ProgramButton::Forward),
                None,
            ]
        );

        let held: Vec<Option<ProgramButton>> = [0, 1, 2, 4, 3, 8, 16]
            .into_iter()
            .map(held_button)
            .collect();
        assert_eq!(
            held,
            vec![
                None,
                Some(ProgramButton::Left),
                Some(ProgramButton::Right),
                Some(ProgramButton::Middle),
                Some(ProgramButton::Left),
                Some(ProgramButton::Back),
                Some(ProgramButton::Forward),
            ]
        );

        assert_eq!(
            mods_of(Mods {
                shift: true,
                ctrl: true,
                alt: false,
                meta: true,
            }),
            vec!["Shift", "Control", "Meta"]
        );
    }

    #[test]
    fn click_on_focused_body_reports_fractional_cell_and_modifiers() {
        let mut mouse = WallMouseState::new();
        let hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Normal,
        });
        let (cx, cy) = client_at(3, 5, 0.5, 0.25);
        let cell = cell_at_80x24(cx, cy);
        let down = mouse
            .down(
                press(0, false, false),
                Mods::default(),
                hit,
                Some("t"),
                cell,
            )
            .expect("claimed by focused normal-tracking terminal");
        assert!(mouse.busy());
        let up = mouse.up(0, Mods::default(), true, cell);
        assert!(!mouse.busy());
        assert_eq!(
            [down, up].concat(),
            vec![
                MouseReport {
                    terminal_id: "t".to_owned(),
                    action: MouseAction::Press,
                    button: Some(ProgramButton::Left),
                    col: 3.5,
                    row: 5.25,
                    mods: Vec::new(),
                },
                MouseReport {
                    terminal_id: "t".to_owned(),
                    action: MouseAction::Release,
                    button: Some(ProgramButton::Left),
                    col: 3.5,
                    row: 5.25,
                    mods: Vec::new(),
                },
            ]
        );

        // Control rides on a right-button press and release too.
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        let (rx, ry) = client_at(0, 0, 0.5, 0.5);
        let rcell = cell_at_80x24(rx, ry);
        let rdown = mouse
            .down(PressInput::new(2, ctrl), ctrl, hit, Some("t"), rcell)
            .expect("right+ctrl claimed");
        let rup = mouse.up(0, ctrl, true, rcell);
        assert_eq!(rdown[0].button, Some(ProgramButton::Right));
        assert_eq!(rdown[0].mods, vec!["Control"]);
        assert_eq!(rup[0].button, Some(ProgramButton::Right));
        assert_eq!(rup[0].mods, vec!["Control"]);
    }

    #[test]
    fn drag_moves_once_per_cell_clamped_off_edge_until_release() {
        let mut mouse = WallMouseState::new();
        let hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Button,
        });
        let mut sent = Vec::new();
        let (x0, y0) = client_at(1, 1, 0.5, 0.5);
        sent.extend(
            mouse
                .down(
                    press(0, false, false),
                    Mods::default(),
                    hit,
                    Some("t"),
                    cell_at_80x24(x0, y0),
                )
                .unwrap(),
        );
        // Same cell: nothing.
        let (x1, y1) = client_at(1, 1, 0.9, 0.9);
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT,
            Mods::default(),
            true,
            MouseTracking::Button,
            cell_at_80x24(x1, y1),
        ));
        // Next cell.
        let (x2, y2) = client_at(2, 1, 0.1, 0.5);
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT,
            Mods::default(),
            true,
            MouseTracking::Button,
            cell_at_80x24(x2, y2),
        ));
        // Same cell again: nothing.
        let (x3, y3) = client_at(2, 1, 0.8, 0.5);
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT,
            Mods::default(),
            true,
            MouseTracking::Button,
            cell_at_80x24(x3, y3),
        ));
        // Off the terminal up and right: clamped to its edge.
        let edge = cell_at_80x24(2000.0, -500.0);
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT,
            Mods::default(),
            true,
            MouseTracking::Button,
            edge,
        ));
        sent.extend(mouse.up(0, Mods::default(), true, edge));

        let lines: Vec<String> = sent.iter().map(summary).collect();
        assert_eq!(
            lines,
            vec![
                "press left 1,1",
                "move left 2,1",
                "move left 80,0",
                "release left 80,0",
            ]
        );
        assert_eq!((sent[2].col, sent[2].row), (80.0, 0.0));
    }

    #[test]
    fn normal_tracking_hears_no_drag_and_chords_second_button_in_gesture() {
        let mut mouse = WallMouseState::new();
        let hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Normal,
        });
        let mut sent = Vec::new();
        let (x0, y0) = client_at(1, 1, 0.5, 0.5);
        sent.extend(
            mouse
                .down(
                    press(0, false, false),
                    Mods::default(),
                    hit,
                    Some("t"),
                    cell_at_80x24(x0, y0),
                )
                .unwrap(),
        );
        let (x1, y1) = client_at(4, 2, 0.5, 0.5);
        let c42 = cell_at_80x24(x1, y1);
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT,
            Mods::default(),
            true,
            MouseTracking::Normal,
            c42,
        ));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].action, MouseAction::Press);

        // Right button joins on a move, then releases before left releases.
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT | BUTTON_BIT_RIGHT,
            Mods::default(),
            true,
            MouseTracking::Normal,
            c42,
        ));
        sent.extend(mouse.pointer_move(
            BUTTON_BIT_LEFT,
            Mods::default(),
            true,
            MouseTracking::Normal,
            c42,
        ));
        sent.extend(mouse.up(0, Mods::default(), true, c42));

        let lines: Vec<String> = sent.iter().map(summary).collect();
        assert_eq!(
            lines,
            vec![
                "press left 1,1",
                "press right 4,2",
                "release right 4,2",
                "release left 4,2",
            ]
        );
    }

    #[test]
    fn cancelled_gesture_releases_what_it_held() {
        let mut mouse = WallMouseState::new();
        let hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Any,
        });
        let (cx, cy) = client_at(6, 7, 0.5, 0.5);
        let down = mouse
            .down(
                press(0, false, false),
                Mods::default(),
                hit,
                Some("t"),
                cell_at_80x24(cx, cy),
            )
            .unwrap();
        let cancelled = mouse.cancel(true);
        assert!(!mouse.busy());
        let lines: Vec<String> = down.iter().chain(&cancelled).map(summary).collect();
        assert_eq!(lines, vec!["press left 6,7", "release left 6,7"]);
    }

    #[test]
    fn any_event_tracking_hears_hover_once_per_cell_and_button_tracking_does_not() {
        let mut mouse = WallMouseState::new();
        let hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Any,
        });
        let mut sent = Vec::new();
        let (x0, y0) = client_at(2, 2, 0.5, 0.5);
        sent.extend(mouse.hover(Mods::default(), hit, Some("t"), cell_at_80x24(x0, y0)));
        let (x1, y1) = client_at(2, 2, 0.9, 0.5);
        sent.extend(mouse.hover(Mods::default(), hit, Some("t"), cell_at_80x24(x1, y1)));
        let (x2, y2) = client_at(3, 2, 0.5, 0.5);
        sent.extend(mouse.hover(Mods::default(), hit, Some("t"), cell_at_80x24(x2, y2)));
        // Off the terminal: clears hover cell and emits nothing.
        sent.extend(mouse.hover(
            Mods::default(),
            None,
            Some("t"),
            cell_at_80x24(2000.0, 2000.0),
        ));
        // Back onto (3, 2): heard again.
        sent.extend(mouse.hover(Mods::default(), hit, Some("t"), cell_at_80x24(x2, y2)));
        // Shift held: belongs to the wall, not the program.
        let (x3, y3) = client_at(4, 2, 0.5, 0.5);
        sent.extend(mouse.hover(
            Mods {
                shift: true,
                ..Mods::default()
            },
            hit,
            Some("t"),
            cell_at_80x24(x3, y3),
        ));

        let lines: Vec<String> = sent.iter().map(summary).collect();
        assert_eq!(
            lines,
            vec!["move none 2,2", "move none 3,2", "move none 3,2"]
        );

        let mut btn_mouse = WallMouseState::new();
        let btn_hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Button,
        });
        assert_eq!(
            btn_mouse.hover(Mods::default(), btn_hit, Some("t"), cell_at_80x24(x0, y0)),
            None
        );
    }

    #[test]
    fn unfocused_shift_alt_and_title_bar_stay_walls_while_focused_claims_double_click() {
        let mut mouse = WallMouseState::new();
        let body_hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Any,
        });
        let (cx, cy) = client_at(1, 1, 0.5, 0.5);
        let c11 = cell_at_80x24(cx, cy);

        // Unfocused terminal: press is not claimed by the program.
        assert_eq!(
            mouse.down(press(0, false, false), Mods::default(), body_hit, None, c11),
            None
        );
        // Once focused, hover is heard.
        let (hx, hy) = client_at(9, 9, 0.5, 0.5);
        assert!(
            mouse
                .hover(Mods::default(), body_hit, Some("t"), cell_at_80x24(hx, hy))
                .is_some()
        );

        // Shift, left+Alt, and title bar all stay the wall's.
        assert_eq!(
            mouse.down(
                press(0, true, false),
                Mods {
                    shift: true,
                    ..Mods::default()
                },
                body_hit,
                Some("t"),
                c11,
            ),
            None
        );
        assert_eq!(
            mouse.down(
                press(0, false, true),
                Mods {
                    alt: true,
                    ..Mods::default()
                },
                body_hit,
                Some("t"),
                c11,
            ),
            None
        );
        let title_hit = Some(TerminalHit {
            id: "t",
            part: HitPart::Title,
            tracking: MouseTracking::Any,
        });
        assert_eq!(
            mouse.down(
                press(0, false, false),
                Mods::default(),
                title_hit,
                Some("t"),
                c11,
            ),
            None
        );

        // Double-click claim on focused reporting body (`WallMouse.claims`).
        let normal_body = Some(TerminalHit {
            id: "t",
            part: HitPart::Body,
            tracking: MouseTracking::Normal,
        });
        assert!(mouse.claims(press(0, false, false), normal_body, Some("t")));
        assert!(!mouse.claims(press(0, true, false), normal_body, Some("t")));
    }
}
