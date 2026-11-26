//! JSON control payload types for `g3d-termd` wire protocol 1.5.
//!
//! Both peers ignore unknown fields (`serde`'s default) so future 1.x minor versions remain
//! wire-compatible without breaking older clients. Adapted from `g3d-wire` (MIT, same owner).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Protocol `[major, minor]` spoken by this client.
pub const PROTO: [u32; 2] = [1, 5];

/// Wire error codes returned in `REPLY.error`, `ERROR` frames, and `error` events.
pub mod code {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNKNOWN_OP: &str = "unknown_op";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const NO_TERMINAL: &str = "no_terminal";
    pub const EXISTS: &str = "exists";
    pub const NOT_SUBSCRIBED: &str = "not_subscribed";
    pub const ALREADY_SUBSCRIBED: &str = "already_subscribed";
    pub const READ_ONLY: &str = "read_only";
    pub const NOT_OWNER: &str = "not_owner";
    pub const ALT_SCREEN: &str = "alt_screen";
    pub const SPAWN_FAILED: &str = "spawn_failed";
    pub const TOO_LARGE: &str = "too_large";
    pub const BUSY: &str = "busy";
    pub const UNSAFE_PASTE: &str = "unsafe_paste";
    pub const PASTE_TIMEOUT: &str = "paste_timeout";

    // Connection-closing `ERROR` codes.
    pub const BAD_FRAME: &str = "bad_frame";
    pub const VERSION: &str = "version";
    pub const AUTH: &str = "auth";
    pub const RESTART: &str = "restart";
    pub const BRAIN_FAILED: &str = "brain_failed";

    // Terminal `error` event codes.
    pub const INPUT_FULL: &str = "input_full";
    pub const PANIC: &str = "panic";
}

/// `HELLO`, the first frame sent by a client upon connecting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: [u32; 2],
    pub client: String,
}

/// `WELCOME`, the daemon's answer to `HELLO`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub proto: [u32; 2],
    pub daemon: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ns: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub features: Vec<String>,
    pub max_frame: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<HolderInfo>,
}

impl Welcome {
    /// Reports whether the daemon advertised `feature` in `WELCOME.features`.
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|item| item == feature)
    }
}

/// `WELCOME.holder`: identity of the holder process keeping PTYs alive across brain restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderInfo {
    pub pid: u32,
    pub version: String,
}

/// Structured error returned by `g3d-termd` in a failed `REPLY` or connection-level `ERROR` frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

impl WireError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_owned(), message: message.into() }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

/// Options for the `create` request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Create {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Variables added to (or, with `None`, removed from) the program's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, Option<String>>>,
    /// Caller's base environment. When `None`, `g3d-termd` uses the desktop session's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_base: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_exists: Option<IfExists>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colors: Option<Colors>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_px: Option<[u32; 2]>,
}

/// Behaviour when `create` names an `id` that already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IfExists {
    Error,
    Adopt,
}

/// Default terminal colours (`#rrggbb`) reported to programs querying `OSC 10`/`11`/`12`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Reply returned by `create`.
///
/// Protocol 1.5's `create` reply carries `created` and `login` and omits `list`'s `title`, `alt`,
/// `seq`, `created_at` and `subscribers`, so it is distinct from [`TermInfo`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReply {
    pub id: String,
    pub handle: u32,
    pub pid: u32,
    pub created: bool,
    pub cols: u16,
    pub rows: u16,
    pub argv: Vec<String>,
    pub login: bool,
    pub cwd: String,
}

/// One live terminal entry returned by `list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermInfo {
    pub id: String,
    pub handle: u32,
    pub pid: u32,
    pub argv: Vec<String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub alt: bool,
    pub seq: u64,
    pub created_at: u64,
    pub subscribers: SubscriberCounts,
}

/// Active subscriber counts per role on a [`TermInfo`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriberCounts {
    pub own: u32,
    pub attach: u32,
    pub watch: u32,
}

/// Output delivery mode of a subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Raw,
    None,
    Cells,
}

/// Subscription role governing write and resize rights.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Read-only observer.
    Watch,
    /// May send input, keys, pastes and mouse events; may not resize.
    Attach,
    /// Single size owner; may send input and resize.
    Own,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Watch => "watch",
            Role::Attach => "attach",
            Role::Own => "own",
        }
    }
}

/// Backpressure policy for a subscription queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    Lossy,
    Block,
}

/// Full options for `subscribe` when overriding mode or flow control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    pub id: String,
    pub mode: Mode,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_rows: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<Flow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
}

/// Active subscription details returned by `subscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub handle: u32,
    pub role: Role,
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
}

/// Scrollback view movement for [`crate::TermdHandle::view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// Scroll by `delta` rows (`> 0` back into history, `< 0` forward toward live).
    Delta(i64),
    /// Return immediately to the live screen (`offset` 0).
    Live,
}

/// Reply to a `view` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewReply {
    /// Rows back from live (`0` means live).
    pub offset: u64,
    /// Deepest scrollback offset currently available.
    pub max: u64,
}

/// One key event with W3C `KeyboardEvent` semantics, encoded by `g3d-termd` against the
/// terminal's live keyboard modes (Kitty flags, `modifyOtherKeys`, `DECCKM`, `DECKPAM`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// Physical key (`KeyboardEvent.code`, e.g. `"KeyA"`, `"Enter"`, `"ArrowLeft"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    /// Logical key (`KeyboardEvent.key`, e.g. `"a"`, `"A"`, `"Enter"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Active modifier names (`"Shift"`, `"Control"`, `"Alt"`, `"Meta"`, `"CapsLock"`, `"NumLock"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
    /// Text produced by the key in the OS keyboard layout; empty when it typed nothing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Press, repeat or release (`None` defaults to `"press"` on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<KeyAction>,
    /// macOS Option-as-Alt handling (protocol 1.3).
    #[serde(default, skip_serializing_if = "OptionAsAlt::is_auto")]
    pub option_as_alt: OptionAsAlt,
}

/// Key transition kind for [`Key`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

/// `key.option_as_alt` on the wire: `"auto"` (default), `true` or `false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OptionAsAlt {
    #[default]
    Auto,
    True,
    False,
}

impl OptionAsAlt {
    pub fn is_auto(&self) -> bool {
        *self == OptionAsAlt::Auto
    }
}

impl std::fmt::Display for OptionAsAlt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OptionAsAlt::Auto => "auto",
            OptionAsAlt::True => "true",
            OptionAsAlt::False => "false",
        })
    }
}

impl std::str::FromStr for OptionAsAlt {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        match input {
            "auto" => Ok(OptionAsAlt::Auto),
            "true" => Ok(OptionAsAlt::True),
            "false" => Ok(OptionAsAlt::False),
            _ => Err(format!("option_as_alt must be auto, true or false, not {input:?}")),
        }
    }
}

impl Serialize for OptionAsAlt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            OptionAsAlt::Auto => serializer.serialize_str("auto"),
            OptionAsAlt::True => serializer.serialize_bool(true),
            OptionAsAlt::False => serializer.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for OptionAsAlt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OptionVisitor;
        impl serde::de::Visitor<'_> for OptionVisitor {
            type Value = OptionAsAlt;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(r#""auto", true or false"#)
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<OptionAsAlt, E> {
                Ok(if value { OptionAsAlt::True } else { OptionAsAlt::False })
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<OptionAsAlt, E> {
                match value {
                    "auto" => Ok(OptionAsAlt::Auto),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(value), &self)),
                }
            }
        }
        deserializer.deserialize_any(OptionVisitor)
    }
}

/// One pointer event on a terminal's cell grid (protocol 1.4).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Mouse {
    #[serde(default)]
    pub action: MouseAction,
    pub button: MouseButton,
    /// Column from the left edge, 0-based (fractional part feeds SGR-pixels mode 1016).
    pub col: f64,
    /// Row from the top edge, 0-based (fractional part feeds SGR-pixels mode 1016).
    pub row: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
}

/// Pointer transition kind for [`Mouse`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    #[default]
    Press,
    Release,
    Move,
}

/// Pointer button for [`Mouse`]: named buttons 1-7 or `"none"`, or `8..=11` as integers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    #[default]
    None,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    /// X11 extra buttons `8..=11` (8 back, 9 forward).
    Extra(u8),
}

impl MouseButton {
    const NAMES: &'static str =
        "left, middle, right, none, wheel-up, wheel-down, wheel-left, wheel-right or 8..11";

    pub fn is_wheel(self) -> bool {
        matches!(
            self,
            MouseButton::WheelUp
                | MouseButton::WheelDown
                | MouseButton::WheelLeft
                | MouseButton::WheelRight
        )
    }

    fn extra(number: u64) -> Result<Self, String> {
        match number {
            8..=11 => Ok(MouseButton::Extra(number as u8)),
            1..=7 => Err(format!("button {number} goes by its name ({})", Self::NAMES)),
            _ => Err(format!("button is {}, not {number}", Self::NAMES)),
        }
    }
}

impl std::fmt::Display for MouseButton {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MouseButton::Left => f.write_str("left"),
            MouseButton::Middle => f.write_str("middle"),
            MouseButton::Right => f.write_str("right"),
            MouseButton::None => f.write_str("none"),
            MouseButton::WheelUp => f.write_str("wheel-up"),
            MouseButton::WheelDown => f.write_str("wheel-down"),
            MouseButton::WheelLeft => f.write_str("wheel-left"),
            MouseButton::WheelRight => f.write_str("wheel-right"),
            MouseButton::Extra(number) => write!(f, "{number}"),
        }
    }
}

impl std::str::FromStr for MouseButton {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        match input {
            "left" => Ok(MouseButton::Left),
            "middle" => Ok(MouseButton::Middle),
            "right" => Ok(MouseButton::Right),
            "none" => Ok(MouseButton::None),
            "wheel-up" => Ok(MouseButton::WheelUp),
            "wheel-down" => Ok(MouseButton::WheelDown),
            "wheel-left" => Ok(MouseButton::WheelLeft),
            "wheel-right" => Ok(MouseButton::WheelRight),
            other => match other.parse::<u64>() {
                Ok(number) => Self::extra(number),
                Err(_) => Err(format!("button is {}, not {input:?}", Self::NAMES)),
            },
        }
    }
}

impl Serialize for MouseButton {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            MouseButton::Extra(number) => serializer.serialize_u8(*number),
            named => serializer.serialize_str(&named.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for MouseButton {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ButtonVisitor;
        impl serde::de::Visitor<'_> for ButtonVisitor {
            type Value = MouseButton;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(MouseButton::NAMES)
            }

            fn visit_u64<E: serde::de::Error>(self, number: u64) -> Result<MouseButton, E> {
                MouseButton::extra(number).map_err(E::custom)
            }

            fn visit_i64<E: serde::de::Error>(self, number: i64) -> Result<MouseButton, E> {
                MouseButton::extra(u64::try_from(number).unwrap_or(0)).map_err(E::custom)
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<MouseButton, E> {
                match value.parse::<MouseButton>() {
                    Ok(MouseButton::Extra(_)) | Err(_) => {
                        Err(E::custom(format!("button is {}, not {value:?}", MouseButton::NAMES)))
                    }
                    Ok(button) => Ok(button),
                }
            }
        }
        deserializer.deserialize_any(ButtonVisitor)
    }
}

// ── Internal wire request/reply envelopes ────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct WireRequest<'a> {
    pub req: u64,
    #[serde(flatten)]
    pub op: WireOp<'a>,
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum WireOp<'a> {
    List,
    Create(&'a Create),
    Subscribe(&'a Subscribe),
    Unsubscribe { handle: u32 },
    Take {
        handle: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
    },
    Resize { handle: u32, cols: u16, rows: u16 },
    Repaint {
        handle: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        history_rows: Option<u32>,
    },
    View {
        handle: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        live: Option<bool>,
    },
    Key {
        handle: u32,
        #[serde(flatten)]
        key: &'a Key,
    },
    Paste {
        handle: u32,
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        allow_unsafe: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        more: Option<bool>,
    },
    Mouse {
        handle: u32,
        #[serde(flatten)]
        mouse: &'a Mouse,
    },
    Kill {
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        signal: Option<&'a str>,
    },
    Status,
    StatusFollow {
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
    },
    StatusUnfollow,
    Ping,
    Stop,
    Upgrade {
        #[serde(skip_serializing_if = "Option::is_none")]
        binary: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        holder: Option<bool>,
    },
}

impl WireOp<'_> {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            WireOp::List => "list",
            WireOp::Create(_) => "create",
            WireOp::Subscribe(_) => "subscribe",
            WireOp::Unsubscribe { .. } => "unsubscribe",
            WireOp::Take { .. } => "take",
            WireOp::Resize { .. } => "resize",
            WireOp::Repaint { .. } => "repaint",
            WireOp::View { .. } => "view",
            WireOp::Key { .. } => "key",
            WireOp::Paste { .. } => "paste",
            WireOp::Mouse { .. } => "mouse",
            WireOp::Kill { .. } => "kill",
            WireOp::Status => "status",
            WireOp::StatusFollow { .. } => "status_follow",
            WireOp::StatusUnfollow => "status_unfollow",
            WireOp::Ping => "ping",
            WireOp::Stop => "stop",
            WireOp::Upgrade { .. } => "upgrade",
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct ReplyHead {
    pub req: u64,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<WireError>,
}

#[derive(Deserialize)]
pub(crate) struct ListReply {
    pub terminals: Vec<TermInfo>,
}

#[derive(Deserialize)]
pub(crate) struct TakeReply {
    pub role: Role,
}

#[derive(Deserialize)]
pub(crate) struct WrittenReply {
    #[serde(rename = "written")]
    pub _written: u64,
}

#[derive(Deserialize)]
pub(crate) struct StatusFollowReply {
    pub interval_ms: u64,
}

#[derive(Deserialize)]
pub(crate) struct StatusReply {
    pub status: serde_json::Value,
}

#[derive(Deserialize)]
pub(crate) struct UpgradeReply {
    pub generation: u64,
}

#[derive(Deserialize)]
pub(crate) struct EmptyReply {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_as_alt_serializes_and_parses_wire_forms() {
        let auto_key = Key::default();
        assert_eq!(serde_json::to_string(&auto_key).unwrap(), "{}");

        let forced_true = Key { option_as_alt: OptionAsAlt::True, ..Key::default() };
        assert_eq!(serde_json::to_string(&forced_true).unwrap(), r#"{"option_as_alt":true}"#);

        let parsed: Key = serde_json::from_str(r#"{"option_as_alt":false,"future_field":1}"#).unwrap();
        assert_eq!(parsed.option_as_alt, OptionAsAlt::False);

        let parsed_auto: Key = serde_json::from_str(r#"{"option_as_alt":"auto"}"#).unwrap();
        assert_eq!(parsed_auto.option_as_alt, OptionAsAlt::Auto);

        assert!(serde_json::from_str::<Key>(r#"{"option_as_alt":"left"}"#).is_err());
    }

    #[test]
    fn mouse_button_serializes_names_and_extra_numbers() {
        assert_eq!(serde_json::to_string(&MouseButton::Left).unwrap(), r#""left""#);
        assert_eq!(serde_json::to_string(&MouseButton::WheelUp).unwrap(), r#""wheel-up""#);
        assert_eq!(serde_json::to_string(&MouseButton::Extra(9)).unwrap(), "9");

        assert_eq!(serde_json::from_str::<MouseButton>(r#""right""#).unwrap(), MouseButton::Right);
        assert_eq!(serde_json::from_str::<MouseButton>("8").unwrap(), MouseButton::Extra(8));
        assert!(serde_json::from_str::<MouseButton>("4").is_err());
        assert!(serde_json::from_str::<MouseButton>(r#""8""#).is_err());
    }

    #[test]
    fn wire_request_flattens_handle_with_key_and_mouse() {
        let key = Key { code: "Enter".into(), key: "Enter".into(), ..Key::default() };
        let req = WireRequest { req: 17, op: WireOp::Key { handle: 7, key: &key } };
        let val = serde_json::to_value(&req).unwrap();
        assert_eq!(
            val,
            serde_json::json!({
                "req": 17,
                "op": "key",
                "handle": 7,
                "code": "Enter",
                "key": "Enter",
            })
        );
    }
}
