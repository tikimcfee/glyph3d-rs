//! Shell-style command parsing, loose namespace/prefix resolution, middleware,
//! response formatting, and the built-in `help` and `call` verbs.
//!
//! Ports `packages/glyph3d-core/src/services/orchestration/CommandRouter.js`,
//! together with its response box/table helpers
//! (`packages/glyph3d-core/src/utils/formatResponse.js`), UTF-8 base64 helpers
//! (`packages/glyph3d-core/src/utils/encoding.js`), and relay-token secret
//! redaction (`redactRelaySecrets` / `redactCommandArgs` / `isSecretCommand` from
//! `packages/glyph3d-core/src/services/orchestration/WebSocketBridge.js`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ── Response formatting (`utils/formatResponse.js`) ──────────────────────────

const BOX_TL: char = '\u{2554}';
const BOX_TR: char = '\u{2557}';
const BOX_BL: char = '\u{255a}';
const BOX_BR: char = '\u{255d}';
const BOX_H: char = '\u{2550}';
const BOX_V: char = '\u{2551}';

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn pad_or_clip(s: &str, width: usize) -> String {
    let n = char_len(s);
    if n >= width {
        s.chars().take(width).collect()
    } else {
        let mut out = String::with_capacity(s.len() + (width - n));
        out.push_str(s);
        for _ in 0..(width - n) {
            out.push(' ');
        }
        out
    }
}

fn pad_end(s: &str, width: usize) -> String {
    let n = char_len(s);
    if n >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + (width - n));
        out.push_str(s);
        for _ in 0..(width - n) {
            out.push(' ');
        }
        out
    }
}

/// Draw a Unicode double-line box around `lines` with `title` in the top border
/// (`box` in `utils/formatResponse.js`, where `min_width` defaults to `40`).
pub fn format_box<S: AsRef<str>>(title: &str, lines: &[S], min_width: usize) -> String {
    let title_len = char_len(title);
    let max_line = lines.iter().map(|l| char_len(l.as_ref())).max().unwrap_or(0);
    let content_width = min_width.saturating_sub(4).max(title_len + 2).max(max_line);
    let inner = content_width + 2;
    let right_bars = inner.saturating_sub(title_len + 3);

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(format!(
        "{BOX_TL}{BOX_H}{BOX_H} {title} {}{BOX_TR}",
        std::iter::repeat_n(BOX_H, right_bars).collect::<String>()
    ));
    for line in lines {
        out.push(format!("{BOX_V} {} {BOX_V}", pad_or_clip(line.as_ref(), content_width)));
    }
    out.push(format!(
        "{BOX_BL}{}{BOX_BR}",
        std::iter::repeat_n(BOX_H, inner).collect::<String>()
    ));
    out.join("\n")
}

/// Format an aligned plain-text table with a `─` separator rule (`table` in `formatResponse.js`).
pub fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let data_max = rows
                .iter()
                .map(|r| r.get(i).map_or(0, |cell| char_len(cell)))
                .max()
                .unwrap_or(0);
            char_len(h).max(data_max)
        })
        .collect();

    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad_or_clip(h, widths[i]))
        .collect::<Vec<_>>()
        .join("  ");
    let separator = widths
        .iter()
        .map(|w| std::iter::repeat_n('\u{2500}', *w).collect::<String>())
        .collect::<Vec<_>>()
        .join("\u{2500}\u{2500}");
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(header_line);
    lines.push(separator);
    for row in rows {
        let line = widths
            .iter()
            .enumerate()
            .map(|(i, w)| pad_or_clip(row.get(i).map_or("", String::as_str), *w))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(line);
    }
    lines.join("\n")
}

/// Format key-value pairs with padded `key:` labels (`kvLines` in `formatResponse.js`).
pub fn kv_lines(entries: &[(&str, &str)], key_width: usize) -> Vec<String> {
    entries
        .iter()
        .map(|(k, v)| format!("{} {v}", pad_or_clip(&format!("{k}:"), key_width)))
        .collect()
}

// ── UTF-8 Base64 (`utils/encoding.js`) ───────────────────────────────────────

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode UTF-8 `text` as standard padded Base64 (`encodeBase64` in `encoding.js`).
pub fn encode_base64(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);

        out.push(B64_TABLE[(b0 >> 2) as usize] as char);
        out.push(B64_TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decode standard or URL-safe Base64 to UTF-8 (`decodeBase64` in `encoding.js`).
pub fn decode_base64(b64: &str) -> Result<String, String> {
    let filtered: Vec<u8> = b64
        .bytes()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c'))
        .collect();
    if filtered.is_empty() {
        return Ok(String::new());
    }

    let pad_count = filtered.iter().rev().take_while(|&&b| b == b'=').count();
    if pad_count > 2 {
        return Err("invalid base64 padding".into());
    }
    let data_len = filtered.len() - pad_count;
    if pad_count > 0 && !filtered.len().is_multiple_of(4) {
        return Err("invalid padded base64 length".into());
    }
    if data_len % 4 == 1 {
        return Err("invalid base64 length".into());
    }
    if filtered[..data_len].contains(&b'=') {
        return Err("unexpected '=' inside base64 payload".into());
    }

    let mut bytes = Vec::with_capacity((data_len * 3) / 4);
    let mut i = 0;
    while i < data_len {
        let rem = data_len - i;
        let v0 = b64_val(filtered[i]).ok_or_else(|| "invalid base64 character".to_string())?;
        let v1 = b64_val(filtered[i + 1]).ok_or_else(|| "invalid base64 character".to_string())?;
        bytes.push((v0 << 2) | (v1 >> 4));
        if rem > 2 {
            let v2 =
                b64_val(filtered[i + 2]).ok_or_else(|| "invalid base64 character".to_string())?;
            bytes.push(((v1 & 0x0f) << 4) | (v2 >> 2));
            if rem > 3 {
                let v3 = b64_val(filtered[i + 3])
                    .ok_or_else(|| "invalid base64 character".to_string())?;
                bytes.push(((v2 & 0x03) << 6) | v3);
            }
        }
        i += 4;
    }

    String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8 in base64 payload: {e}"))
}

// ── Relay secret redaction (`WebSocketBridge.js`) ────────────────────────────

/// Whether every character in `tok` is a valid RFC 7230 token character (`TOKEN_CHARS`).
pub fn is_token_chars(tok: &str) -> bool {
    !tok.is_empty()
        && tok.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                        | b'-'
                )
        })
}

fn is_word_boundary_prev(prev: Option<char>) -> bool {
    match prev {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric() && c != '_',
    }
}

fn redact_token_key_values(input: &str, hash_mode: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        if hash_mode {
            let Some(h_pos) = rest.find('#') else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..h_pos]);
            rest = &rest[h_pos..];
            // Scan inside this non-whitespace fragment up to whitespace.
            let frag_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let frag = &rest[..frag_end];
            out.push_str(&redact_fragment_tokens(frag));
            rest = &rest[frag_end..];
        } else {
            let Some(d_pos) = rest.find(['?', '&', ';']) else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..=d_pos]);
            rest = &rest[d_pos + 1..];
            if rest.len() >= 6 && rest.as_bytes()[..6].eq_ignore_ascii_case(b"token=") {
                out.push_str(&rest[..6]);
                rest = &rest[6..];
                let val_end = rest
                    .find(|c: char| {
                        c.is_whitespace() || matches!(c, '&' | ';' | '"' | '\'' | ')' | ']' | '}')
                    })
                    .unwrap_or(rest.len());
                if val_end > 0 {
                    out.push_str("[REDACTED]");
                    rest = &rest[val_end..];
                }
            }
        }
    }
    out
}

fn redact_fragment_tokens(frag: &str) -> String {
    let mut out = String::with_capacity(frag.len());
    let mut i = 0;
    let bytes = frag.as_bytes();
    while i < bytes.len() {
        let prev = frag[..i].chars().next_back();
        if is_word_boundary_prev(prev)
            && i + 6 <= bytes.len()
            && bytes[i..i + 6].eq_ignore_ascii_case(b"token=")
        {
            out.push_str(&frag[i..i + 6]);
            i += 6;
            let tail = &frag[i..];
            let val_end = tail
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '&' | ';' | '"' | '\'' | ')' | ']' | '}')
                })
                .unwrap_or(tail.len());
            out.push_str("[REDACTED]");
            i += val_end;
        } else {
            let ch = frag[i..].chars().next().expect("i < bytes.len()");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn redact_g3d_subproto(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let needle = b"g3d-token.";
    while i < input.len() {
        let prev = input[..i].chars().next_back();
        if is_word_boundary_prev(prev)
            && input.as_bytes()[i..].starts_with(needle)
        {
            out.push_str("g3d-token.[REDACTED]");
            i += needle.len();
            if input[i..].starts_with("[REDACTED]") {
                i += "[REDACTED]".len();
            } else {
                while i < input.len() {
                    let b = input.as_bytes()[i];
                    let ok = b.is_ascii_alphanumeric()
                        || matches!(
                            b,
                            b'!' | b'#'
                                | b'$'
                                | b'%'
                                | b'&'
                                | b'\''
                                | b'*'
                                | b'+'
                                | b'.'
                                | b'^'
                                | b'_'
                                | b'`'
                                | b'|'
                                | b'~'
                                | b'-'
                        );
                    if ok {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
        } else {
            let ch = input[i..].chars().next().expect("i < input.len()");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn redact_bearer(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let prev = input[..i].chars().next_back();
        if is_word_boundary_prev(prev)
            && input.len() >= i + 6
            && input.as_bytes()[i..i + 6].eq_ignore_ascii_case(b"bearer")
        {
            let after = &input[i + 6..];
            let ws_len: usize = after
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(char::len_utf8)
                .sum();
            if ws_len > 0 {
                let tok_part = &after[ws_len..];
                let tok_len: usize = tok_part
                    .chars()
                    .take_while(|c| {
                        !c.is_whitespace()
                            && !matches!(c, '"' | '\'' | '\\' | ',' | ')' | ']' | ';' | '}')
                    })
                    .map(char::len_utf8)
                    .sum();
                if tok_len > 0 {
                    out.push_str(&input[i..i + 6 + ws_len]);
                    out.push_str("[REDACTED]");
                    i += 6 + ws_len + tok_len;
                    continue;
                }
            }
        }
        let ch = input[i..].chars().next().expect("i < input.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Scrub `#token=...` fragments, `?token=...` query leaks, `g3d-token.<tok>` subprotocols,
/// and `Bearer <tok>` tokens from log/error text (`redactRelaySecrets` in `WebSocketBridge.js`).
pub fn redact_relay_secrets(text: &str) -> String {
    let s = redact_token_key_values(text, true);
    let s = redact_token_key_values(&s, false);
    let s = redact_g3d_subproto(&s);
    redact_bearer(&s)
}

fn tokenize_whitespace_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            match ch {
                'n' => current.push('\n'),
                't' => current.push('\t'),
                other => current.push(other),
            }
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quote = !in_quote;
            continue;
        }
        if ch.is_whitespace() && !in_quote {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn decode_call_invocation(b64: &str) -> Option<Vec<String>> {
    if b64.is_empty() {
        return None;
    }
    let decoded = decode_base64(b64).ok()?;
    let spec: Value = serde_json::from_str(&decoded).ok()?;
    let items: Vec<Value> = match spec {
        Value::Array(arr) => arr,
        Value::Object(mut map) => {
            let cmd = map.remove("cmd")?;
            if !cmd.is_string() {
                return None;
            }
            let mut v = vec![cmd];
            if let Some(Value::Array(args)) = map.remove("args") {
                v.extend(args);
            }
            v
        }
        _ => return None,
    };
    let head = items.first()?.as_str()?.to_string();
    let mut out = Vec::with_capacity(items.len());
    out.push(head);
    for a in items.into_iter().skip(1) {
        out.push(json_arg_to_string(a));
    }
    Some(out)
}

fn is_secret_tokens(raw_text: &str, tokens: &[String], depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    if redact_relay_secrets(raw_text) != raw_text {
        return true;
    }
    let Some(t0_raw) = tokens.first() else {
        return false;
    };
    let t0 = t0_raw.to_lowercase();
    let t1 = tokens.get(1).map(|s| s.to_lowercase()).unwrap_or_default();

    if t0 == "relay.token"
        || t0 == "wall.token"
        || t0.starts_with("relay.tok")
        || t0.starts_with("wall.tok")
    {
        return true;
    }
    if (t0 == "relay" || t0 == "wall") && (t1 == "token" || t1.starts_with("tok")) {
        return true;
    }

    let connect_args = if t0 == "relay.connect" {
        Some(&tokens[1..])
    } else if t0 == "relay" && t1 == "connect" {
        Some(&tokens[2..])
    } else {
        None
    };
    if let Some(cargs) = connect_args {
        if cargs.len() >= 2 {
            return true;
        }
        if cargs.len() == 1 {
            let a0 = cargs[0].trim();
            let all_digits = !a0.is_empty() && a0.bytes().all(|b| b.is_ascii_digit());
            if !a0.is_empty() && !all_digits && !a0.contains("://") {
                return true;
            }
        }
        return false;
    }

    if t0 == "call" {
        if let Some(b64) = tokens.get(1) {
            if let Some(inner) = decode_call_invocation(b64) {
                let joined = inner.join(" ");
                if is_secret_tokens(&joined, &inner, depth + 1) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether `line` invokes a secret-bearing command (`relay.token`, `wall.token`,
/// `relay.connect` with a token, or a `call` bundle wrapping one; `isSecretCommand`).
pub fn is_secret_command(line: &str) -> bool {
    let tokens = tokenize_whitespace_line(line.trim());
    is_secret_tokens(line, &tokens, 0)
}

/// Return a copy of `args` with capability tokens replaced by `"[REDACTED]"`
/// (`redactCommandArgs` in `WebSocketBridge.js`).
pub fn redact_command_args(cmd_name: &str, args: &[String]) -> Vec<String> {
    let cmd = cmd_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(".")
        .to_lowercase();
    if cmd == "relay.token" || cmd == "wall.token" {
        return vec!["[REDACTED]".to_string(); args.len()];
    }
    if cmd == "relay.connect" {
        return args
            .iter()
            .enumerate()
            .map(|(idx, arg)| {
                if idx >= 1 {
                    return "[REDACTED]".to_string();
                }
                let s0 = arg.trim();
                let all_digits = !s0.is_empty() && s0.bytes().all(|b| b.is_ascii_digit());
                if all_digits || s0.contains("://") {
                    redact_relay_secrets(arg)
                } else {
                    "[REDACTED]".to_string()
                }
            })
            .collect();
    }
    if cmd == "call" {
        if let Some(first) = args.first() {
            if let Some(inner) = decode_call_invocation(first) {
                let joined = inner.join(" ");
                if is_secret_tokens(&joined, &inner, 1) {
                    return vec!["[REDACTED]".to_string(); args.len()];
                }
            }
        }
    }
    args.iter().map(|a| redact_relay_secrets(a)).collect()
}

// ── CommandRouter (`services/orchestration/CommandRouter.js`) ────────────────

/// Dual `{ text, data }` reply returned by every command dispatch on [`CommandRouter`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub text: String,
    pub data: Option<Value>,
}

impl CommandReply {
    /// Reply with plain `text` and `data: None`.
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), data: None }
    }

    /// Reply with `"OK: {message}"` and optional structured `data` (`wallCommands.js`'s `OK`).
    pub fn ok(message: impl AsRef<str>, data: Option<Value>) -> Self {
        Self { text: format!("OK: {}", message.as_ref()), data }
    }

    /// Reply with `"ERR: {message}"` and `data: None` (`wallCommands.js`'s `ERR`).
    pub fn err(message: impl AsRef<str>) -> Self {
        Self { text: format!("ERR: {}", message.as_ref()), data: None }
    }

    /// Whether this reply represents an error (`text.starts_with("ERR:")`).
    pub fn is_err(&self) -> bool {
        self.text.starts_with("ERR:")
    }
}

impl From<&str> for CommandReply {
    fn from(s: &str) -> Self {
        Self::text(s)
    }
}

impl From<String> for CommandReply {
    fn from(s: String) -> Self {
        Self::text(s)
    }
}

/// Optional registration metadata for a command (`description`, `usage`, `returns`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegisterMeta<'a> {
    pub description: &'a str,
    pub usage: &'a str,
    pub returns: &'a str,
}

/// Public descriptor of a registered command (`listCommands` / `listNamespace` / `help`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandMeta {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub returns: String,
}

/// Per-call options passed to [`CommandRouter::execute_with_options`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispatchOptions {
    pub sender: Option<String>,
}

/// Per-dispatch metadata exposed to handlers and middleware (`ctx.sender`, `ctx.cid`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispatchMeta {
    pub sender: Option<String>,
    pub cid: u64,
}

/// Telemetry record captured for each executed command (`safeArgs` already redacted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchTrace {
    pub command: String,
    pub args: Vec<String>,
    pub status_ok: bool,
    pub cid: u64,
    pub sender: Option<String>,
    pub error: Option<String>,
}

/// Input accepted by [`CommandRouter::execute`]: either a raw command string or a
/// pre-split `[name, ...args]` vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandInput<'a> {
    Line(&'a str),
    OwnedLine(String),
    Argv(Vec<String>),
}

impl<'a> From<&'a str> for CommandInput<'a> {
    fn from(s: &'a str) -> Self {
        CommandInput::Line(s)
    }
}

impl<'a> From<&'a String> for CommandInput<'a> {
    fn from(s: &'a String) -> Self {
        CommandInput::Line(s.as_str())
    }
}

impl From<String> for CommandInput<'_> {
    fn from(s: String) -> Self {
        CommandInput::OwnedLine(s)
    }
}

impl<'a> From<&'a [&'a str]> for CommandInput<'a> {
    fn from(slice: &'a [&'a str]) -> Self {
        CommandInput::Argv(slice.iter().map(|s| (*s).to_string()).collect())
    }
}

impl<'a, const N: usize> From<[&'a str; N]> for CommandInput<'a> {
    fn from(arr: [&'a str; N]) -> Self {
        CommandInput::Argv(arr.into_iter().map(str::to_string).collect())
    }
}

impl<'a, const N: usize> From<&'a [&'a str; N]> for CommandInput<'a> {
    fn from(arr: &'a [&'a str; N]) -> Self {
        CommandInput::Argv(arr.iter().map(|s| (*s).to_string()).collect())
    }
}

impl<'a> From<&'a [String]> for CommandInput<'a> {
    fn from(slice: &'a [String]) -> Self {
        CommandInput::Argv(slice.to_vec())
    }
}

impl From<Vec<String>> for CommandInput<'_> {
    fn from(v: Vec<String>) -> Self {
        CommandInput::Argv(v)
    }
}

impl<'a> From<&'a Vec<String>> for CommandInput<'a> {
    fn from(v: &'a Vec<String>) -> Self {
        CommandInput::Argv(v.clone())
    }
}

type CustomHandler<C> =
    Box<dyn FnMut(&[String], &mut C, &DispatchMeta) -> Result<CommandReply, String>>;
type MiddlewareFn<C> = Box<dyn FnMut(&str, &[String], &mut C, &DispatchMeta)>;

enum HandlerKind<C> {
    BuiltinCall,
    BuiltinHelp,
    Custom(CustomHandler<C>),
}

struct CommandEntry<C> {
    handler: HandlerKind<C>,
    description: String,
    usage: String,
    returns: String,
}

enum LooseResolution {
    Hit { name: String, args: Vec<String> },
    Miss { typed: String, matches: Vec<String> },
}

fn json_arg_to_string(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Shell-style command router (`CommandRouter` in `CommandRouter.js`), parameterized
/// over the host context `C`.
pub struct CommandRouter<C> {
    pub context: C,
    commands: BTreeMap<String, CommandEntry<C>>,
    middleware: Vec<MiddlewareFn<C>>,
    next_cid: u64,
    traces: Vec<DispatchTrace>,
}

impl<C> CommandRouter<C> {
    /// Create a router holding `context`, pre-populated with the built-in `call` and `help` verbs.
    pub fn new(context: C) -> Self {
        let mut commands = BTreeMap::new();
        commands.insert(
            "call".to_string(),
            CommandEntry {
                handler: HandlerKind::BuiltinCall,
                description:
                    "Invoke any command from a base64-encoded JSON bundle ([name,...args] or {cmd,args})"
                        .to_string(),
                usage: "<base64-json>".to_string(),
                returns: String::new(),
            },
        );
        commands.insert(
            "help".to_string(),
            CommandEntry {
                handler: HandlerKind::BuiltinHelp,
                description: "Map the command bus; help <namespace|verb> for detail".to_string(),
                usage: "[namespace|verb]".to_string(),
                returns: String::new(),
            },
        );
        Self {
            context,
            commands,
            middleware: Vec::new(),
            next_cid: 0,
            traces: Vec::new(),
        }
    }

    /// Whether a verb with the exact name `name` (case-insensitive) is registered.
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(&name.to_lowercase())
    }

    /// Register (or replace) a command handler under `name` (normalized to lowercase).
    pub fn register<F>(&mut self, name: &str, meta: RegisterMeta<'_>, handler: F)
    where
        F: FnMut(&[String], &mut C, &DispatchMeta) -> Result<CommandReply, String> + 'static,
    {
        self.commands.insert(
            name.to_lowercase(),
            CommandEntry {
                handler: HandlerKind::Custom(Box::new(handler)),
                description: meta.description.to_string(),
                usage: meta.usage.to_string(),
                returns: meta.returns.to_string(),
            },
        );
    }

    /// Add middleware invoked before every resolved command (`use` in `CommandRouter.js`).
    pub fn use_middleware<M>(&mut self, middleware: M)
    where
        M: FnMut(&str, &[String], &mut C, &DispatchMeta) + 'static,
    {
        self.middleware.push(Box::new(middleware));
    }

    /// Redacted dispatch telemetry records accumulated by this router.
    pub fn traces(&self) -> &[DispatchTrace] {
        &self.traces
    }

    /// Clear accumulated dispatch telemetry records.
    pub fn clear_traces(&mut self) {
        self.traces.clear();
    }

    /// Tokenize `input` using `CommandRouter.parse`'s exact rules:
    /// ASCII `' '` separates tokens outside `"..."`, `"` toggles quoting, and `\n`, `\t`,
    /// `\<ch>` escape the following character both inside and outside quotes.
    pub fn parse_line(input: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        let mut escaped = false;

        for ch in input.chars() {
            if escaped {
                match ch {
                    'n' => current.push('\n'),
                    't' => current.push('\t'),
                    other => current.push(other),
                }
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_quote = !in_quote;
                continue;
            }
            if ch == ' ' && !in_quote {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                continue;
            }
            current.push(ch);
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    }

    /// Instance wrapper matching `router.parse(input)` in JS.
    pub fn parse(&self, input: &str) -> Vec<String> {
        Self::parse_line(input)
    }

    /// Sorted list of all registered commands (`listCommands` in `CommandRouter.js`).
    pub fn list_commands(&self) -> Vec<CommandMeta> {
        self.commands
            .iter()
            .map(|(name, e)| CommandMeta {
                name: name.clone(),
                description: e.description.clone(),
                usage: e.usage.clone(),
                returns: e.returns.clone(),
            })
            .collect()
    }

    /// Commands matching `namespace` or `<namespace>.*` (`listNamespace` in `CommandRouter.js`).
    pub fn list_namespace(&self, namespace: &str) -> Vec<CommandMeta> {
        let ns = namespace.to_lowercase();
        let prefix = format!("{ns}.");
        self.list_commands()
            .into_iter()
            .filter(|c| c.name == ns || c.name.starts_with(&prefix))
            .collect()
    }

    /// Execute a command line or pre-split `[name, ...args]` vector with default options.
    pub fn execute<'a, I: Into<CommandInput<'a>>>(&mut self, input: I) -> CommandReply {
        self.execute_with_options(input, &DispatchOptions::default())
    }

    /// Execute a command line or `[name, ...args]` vector carrying `options` (`sender`).
    pub fn execute_with_options<'a, I: Into<CommandInput<'a>>>(
        &mut self,
        input: I,
        options: &DispatchOptions,
    ) -> CommandReply {
        let (name, args) = match input.into() {
            CommandInput::Argv(argv) => {
                let Some(head) = argv.first() else {
                    return CommandReply::err("empty command");
                };
                (head.to_lowercase(), argv[1..].to_vec())
            }
            CommandInput::Line(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return CommandReply::err("empty command");
                }
                let tokens = Self::parse_line(trimmed);
                let Some(head) = tokens.first() else {
                    return CommandReply::err("empty command");
                };
                (head.to_lowercase(), tokens[1..].to_vec())
            }
            CommandInput::OwnedLine(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return CommandReply::err("empty command");
                }
                let tokens = Self::parse_line(trimmed);
                let Some(head) = tokens.first() else {
                    return CommandReply::err("empty command");
                };
                (head.to_lowercase(), tokens[1..].to_vec())
            }
        };

        if self.commands.contains_key(&name) {
            return self.run_resolved(&name, args, options);
        }

        match self.resolve_loose(&name, &args) {
            LooseResolution::Hit { name: resolved, args: rest } => {
                self.run_resolved(&resolved, rest, options)
            }
            LooseResolution::Miss { typed, matches } if !matches.is_empty() => CommandReply {
                text: format!(
                    "ERR: ambiguous command '{}'. Matches: {}",
                    redact_relay_secrets(&typed),
                    matches.join(", ")
                ),
                data: Some(json!({ "matches": matches })),
            },
            LooseResolution::Miss { .. } => CommandReply::err(format!(
                "unknown command '{}'. Try 'help'.",
                redact_relay_secrets(&name)
            )),
        }
    }

    /// Execute a batch of command strings sequentially (`executeBatch` in `CommandRouter.js`).
    pub fn execute_batch<S: AsRef<str>>(&mut self, commands: &[S]) -> Vec<CommandReply> {
        commands.iter().map(|c| self.execute(c.as_ref())).collect()
    }

    fn resolve_loose(&self, name: &str, args: &[String]) -> LooseResolution {
        let mut chain = name.to_string();
        let mut best: Option<(String, Vec<String>)> = None;
        for (i, arg) in args.iter().enumerate() {
            chain.push('.');
            chain.push_str(&arg.to_lowercase());
            if self.commands.contains_key(&chain) {
                best = Some((chain.clone(), args[i + 1..].to_vec()));
            }
        }
        if let Some((hit_name, hit_args)) = best {
            return LooseResolution::Hit { name: hit_name, args: hit_args };
        }

        let by_name: Vec<String> = self
            .commands
            .keys()
            .filter(|k| k.starts_with(name))
            .cloned()
            .collect();
        if by_name.len() == 1 {
            return LooseResolution::Hit { name: by_name[0].clone(), args: args.to_vec() };
        }
        if let Some(first_arg) = args.first() {
            let joined = format!("{name}.{}", first_arg.to_lowercase());
            let by_joined: Vec<String> = self
                .commands
                .keys()
                .filter(|k| k.starts_with(&joined))
                .cloned()
                .collect();
            if by_joined.len() == 1 {
                return LooseResolution::Hit {
                    name: by_joined[0].clone(),
                    args: args[1..].to_vec(),
                };
            }
            if by_joined.len() > 1 {
                return LooseResolution::Miss { typed: joined, matches: by_joined };
            }
        }
        LooseResolution::Miss { typed: name.to_string(), matches: by_name }
    }

    fn run_resolved(
        &mut self,
        name: &str,
        args: Vec<String>,
        options: &DispatchOptions,
    ) -> CommandReply {
        self.next_cid += 1;
        let meta = DispatchMeta { sender: options.sender.clone(), cid: self.next_cid };

        for mw in &mut self.middleware {
            mw(name, &args, &mut self.context, &meta);
        }

        let safe_args = redact_command_args(name, &args);

        enum BuiltinTag {
            Call,
            Help,
        }
        let builtin = match self.commands.get(name).map(|e| &e.handler) {
            Some(HandlerKind::BuiltinCall) => Some(BuiltinTag::Call),
            Some(HandlerKind::BuiltinHelp) => Some(BuiltinTag::Help),
            Some(HandlerKind::Custom(_)) => None,
            None => return CommandReply::err(format!("unknown command '{}'. Try 'help'.", redact_relay_secrets(name))),
        };

        if let Some(tag) = builtin {
            let reply = match tag {
                BuiltinTag::Help => self.builtin_help(&args),
                BuiltinTag::Call => self.builtin_call(&args, options),
            };
            self.traces.push(DispatchTrace {
                command: name.to_string(),
                args: safe_args,
                status_ok: true,
                cid: meta.cid,
                sender: meta.sender,
                error: None,
            });
            return reply;
        }

        let entry = self.commands.get_mut(name).expect("verified command exists");
        let HandlerKind::Custom(handler) = &mut entry.handler else {
            unreachable!()
        };
        match handler(&args, &mut self.context, &meta) {
            Ok(reply) => {
                self.traces.push(DispatchTrace {
                    command: name.to_string(),
                    args: safe_args,
                    status_ok: true,
                    cid: meta.cid,
                    sender: meta.sender,
                    error: None,
                });
                reply
            }
            Err(raw_err) => {
                let safe_err = redact_relay_secrets(&raw_err);
                self.traces.push(DispatchTrace {
                    command: name.to_string(),
                    args: safe_args,
                    status_ok: false,
                    cid: meta.cid,
                    sender: meta.sender,
                    error: Some(safe_err.clone()),
                });
                CommandReply::err(safe_err)
            }
        }
    }

    fn builtin_help(&self, args: &[String]) -> CommandReply {
        let cmds = self.list_commands();

        if !args.is_empty() {
            let q = args.join(".").to_lowercase();
            let matched: Vec<CommandMeta> =
                cmds.iter().filter(|c| c.name.starts_with(&q)).cloned().collect();
            if matched.is_empty() {
                return CommandReply::text(format!(
                    "No commands matching '{q}' — bare help maps the namespaces"
                ));
            }
            let exact = cmds.iter().find(|c| c.name == q);
            if exact.is_some() || matched.len() == 1 {
                let c = exact.unwrap_or(&matched[0]);
                let mut lines = vec![if c.description.is_empty() {
                    "(no description)".to_string()
                } else {
                    c.description.clone()
                }];
                if !c.usage.is_empty() {
                    lines.push(String::new());
                    lines.push(format!("usage    {} {}", c.name, c.usage));
                }
                if !c.returns.is_empty() {
                    lines.push(format!("returns  {}", c.returns));
                }
                if exact.is_some() && matched.len() > 1 {
                    lines.push(String::new());
                    lines.push(format!(
                        "deeper: {} more under {q}.* — help {q}.",
                        matched.len() - 1
                    ));
                }
                return CommandReply {
                    text: format_box(&c.name, &lines, 44),
                    data: Some(json!({ "commands": [c] })),
                };
            }

            let mut lines = Vec::new();
            for c in &matched {
                let head = if c.usage.is_empty() {
                    c.name.clone()
                } else {
                    format!("{} {}", c.name, c.usage)
                };
                let head_len = char_len(&head);
                let one = format!("{}{}", pad_end(&head, 30), c.description);
                if head_len <= 30 && char_len(&one) <= 76 {
                    lines.push(one.trim_end().to_string());
                } else if !c.description.is_empty() {
                    lines.push(head);
                    lines.push(format!("  {}", c.description));
                } else {
                    lines.push(head);
                }
            }
            return CommandReply {
                text: format_box(&format!("COMMANDS: {q}*"), &lines, 70),
                data: Some(json!({ "commands": matched })),
            };
        }

        let mut ns: BTreeMap<String, usize> = BTreeMap::new();
        let mut solo: Vec<String> = Vec::new();
        for c in &cmds {
            if let Some(dot) = c.name.find('.') {
                *ns.entry(c.name[..dot].to_string()).or_insert(0) += 1;
            } else {
                solo.push(c.name.clone());
            }
        }

        let mut ns_sorted: Vec<(String, usize)> = ns.iter().map(|(k, v)| (k.clone(), *v)).collect();
        ns_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let cells: Vec<String> =
            ns_sorted.iter().map(|(name, n)| format!("{name} {n}")).collect();
        let cell_w = cells.iter().map(|s| char_len(s)).max().map_or(1, |m| m + 2);
        let per_row = (64 / cell_w).max(1);
        let mut grid = Vec::new();
        for chunk in cells.chunks(per_row) {
            let row = chunk
                .iter()
                .map(|s| pad_end(s, cell_w))
                .collect::<Vec<_>>()
                .join("");
            grid.push(row.trim_end().to_string());
        }

        solo.sort();
        let mut lines = vec![format!("{} verbs · {} namespaces", cmds.len(), ns.len()), String::new()];
        lines.extend(grid);
        lines.push(String::new());
        lines.push(format!("single-word: {}", solo.join(" · ")));
        lines.push(String::new());
        lines.push("help <namespace>   every verb in one namespace".to_string());
        lines.push("help <verb>        one verb in full (usage · returns)".to_string());
        lines.push("the dot is optional — \"grid list\" runs grid.list".to_string());

        let ns_json: serde_json::Map<String, Value> =
            ns.into_iter().map(|(k, v)| (k, json!(v))).collect();
        CommandReply {
            text: format_box("COMMAND BUS", &lines, 70),
            data: Some(json!({
                "count": cmds.len(),
                "namespaces": ns_json,
                "solo": solo,
            })),
        }
    }

    fn builtin_call(&mut self, args: &[String], options: &DispatchOptions) -> CommandReply {
        let Some(first) = args.first() else {
            return CommandReply::err(
                "usage: call <base64-json>  (JSON: [name, ...args] or {cmd, args})",
            );
        };
        let decoded = match decode_base64(first) {
            Ok(s) => s,
            Err(e) => {
                return CommandReply::err(format!(
                    "call payload must be base64-encoded JSON: {e}"
                ))
            }
        };
        let spec: Value = match serde_json::from_str(&decoded) {
            Ok(v) => v,
            Err(e) => {
                return CommandReply::err(format!(
                    "call payload must be base64-encoded JSON: {e}"
                ))
            }
        };

        let invocation: Vec<Value> = match spec {
            Value::Array(arr) => arr,
            Value::Object(mut map) => match map.remove("cmd") {
                Some(Value::String(cmd)) => {
                    let mut v = vec![Value::String(cmd)];
                    if let Some(Value::Array(a)) = map.remove("args") {
                        v.extend(a);
                    }
                    v
                }
                _ => {
                    return CommandReply::err(
                        "call payload must be [name, ...args] or {cmd, args}",
                    )
                }
            },
            _ => return CommandReply::err("call payload must be [name, ...args] or {cmd, args}"),
        };

        let Some(Value::String(head)) = invocation.first() else {
            return CommandReply::err("call payload has no command name");
        };
        if head.eq_ignore_ascii_case("call") {
            return CommandReply::err("call cannot wrap call");
        }

        let mut argv = Vec::with_capacity(invocation.len());
        argv.push(head.clone());
        for a in invocation.into_iter().skip(1) {
            argv.push(json_arg_to_string(a));
        }
        self.execute_with_options(argv, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(spec: &Value) -> String {
        format!("call {}", encode_base64(&serde_json::to_string(spec).unwrap()))
    }

    fn probe_router() -> CommandRouter<()> {
        let mut router = CommandRouter::new(());
        router.register("probe", RegisterMeta::default(), |args, (), meta| {
            Ok(CommandReply {
                text: "OK".into(),
                data: Some(json!({ "got": args, "sender": meta.sender })),
            })
        });
        router
    }

    // ── Port of tools/command-call.test.mjs ──────────────────────────────────

    #[test]
    fn bare_router_answers_call() {
        assert!(CommandRouter::new(()).has_command("call"));
    }

    #[test]
    fn bundle_round_trips_every_corpus_input_byte_identical_while_naive_corrupts_gnarly_ones() {
        let corpus: &[(&str, &str)] = &[
            ("plain", "analyzed"),
            ("spaces", "rg -n TODO src/"),
            ("dquotes-pipe", "rg -n \"TODO|FIXME\" src/"),
            ("squotes", "grep -E 'foo|bar' ."),
            ("backslash", "find . -name \\*.js"),
            ("win-path", "C:\\Users\\someone\\x"),
            ("tab", "col1\tcol2"),
            ("newline", "line1\nline2"),
            ("json-blob", "{\"mode\":\"newspaper\",\"n\":3}"),
            ("looks-like-flag", "--mode"),
            ("unicode-emoji", "café ☕ 日本語 — slug 🐌"),
            ("empty", ""),
            ("embedded-quotes", "he said \"hi\" then \\ left"),
            ("git-commit", "git commit -m \"fix: pipe | and quotes\""),
            ("multiline-task", "Map the system.\nReport file:line refs."),
            ("path-with-space", "my notes/draft file.md"),
            ("kitchen-sink", "awk '{print $1}' | sed \"s/x/y/\" > /tmp/o\tand"),
        ];
        let must_corrupt_naive = [
            "spaces",
            "dquotes-pipe",
            "backslash",
            "json-blob",
            "git-commit",
            "empty",
        ];

        let mut router = probe_router();
        for &(name, text) in corpus {
            let via_bundle = router.execute(bundle(&json!(["probe", text])));
            assert_eq!(
                via_bundle.data.as_ref().and_then(|d| d.get("got")),
                Some(&json!([text])),
                "bundle failed on {name}"
            );

            let naive = router.execute(format!("probe {text}"));
            let survived = naive
                .data
                .as_ref()
                .and_then(|d| d.get("got"))
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.len() == 1 && arr[0].as_str() == Some(text));
            if must_corrupt_naive.contains(&name) {
                assert!(!survived, "{name} unexpectedly survived naive tokenization");
            }
            if name == "plain" {
                assert!(survived);
            }
        }
    }

    #[test]
    fn object_form_several_args_non_string_coerced_and_sender_carried_through() {
        let mut router = probe_router();
        let r = router.execute_with_options(
            bundle(&json!({ "cmd": "probe", "args": ["a b", 3, true, null] })),
            &DispatchOptions { sender: Some("ctrl-7".into()) },
        );
        assert_eq!(
            r.data,
            Some(json!({ "got": ["a b", "3", "true", "null"], "sender": "ctrl-7" }))
        );
    }

    #[test]
    fn call_refusals_no_payload_bad_base64_bad_shape_no_name_and_call_wrapping_call() {
        let mut router = probe_router();
        assert!(router.execute("call").text.starts_with("ERR: usage: call"));
        assert!(router
            .execute("call !!!notbase64")
            .text
            .starts_with("ERR: call payload must be base64-encoded JSON"));
        assert_eq!(
            router.execute(bundle(&json!({ "args": ["x"] }))).text,
            "ERR: call payload must be [name, ...args] or {cmd, args}"
        );
        assert_eq!(
            router.execute(bundle(&json!([]))).text,
            "ERR: call payload has no command name"
        );
        assert_eq!(
            router.execute(bundle(&json!(["CALL", "x"]))).text,
            "ERR: call cannot wrap call"
        );
    }

    // ── Port of tools/command-help.test.mjs ──────────────────────────────────

    fn sample_help_router() -> CommandRouter<()> {
        let mut r = CommandRouter::new(());
        r.register(
            "grid.list",
            RegisterMeta { description: "List grids", usage: "", returns: "{ grids }" },
            |_, (), _| Ok("OK".into()),
        );
        r.register(
            "grid.layout",
            RegisterMeta {
                description: "Lay a grid out",
                usage: "<id> --mode <m>",
                returns: "",
            },
            |_, (), _| Ok("OK".into()),
        );
        r.register(
            "wall.hide",
            RegisterMeta { description: "Hide a terminal", usage: "<id>", returns: "" },
            |_, (), _| Ok("OK".into()),
        );
        r
    }

    #[test]
    fn bare_router_answers_help_and_unknown_command_points_there() {
        let mut r = CommandRouter::new(());
        assert!(r.has_command("help"));
        assert_eq!(
            r.execute("nope.nothing").text,
            "ERR: unknown command 'nope.nothing'. Try 'help'."
        );
        assert!(r.execute("help").text.contains("COMMAND BUS"));
    }

    #[test]
    fn bare_help_maps_namespaces_by_weight_and_lists_single_word_verbs() {
        let mut r = sample_help_router();
        let reply = r.execute("help");
        let data = reply.data.unwrap();
        assert_eq!(data["namespaces"], json!({ "grid": 2, "wall": 1 }));
        assert_eq!(data["solo"], json!(["call", "help"]));
        assert_eq!(data["count"], json!(5));
        assert!(reply.text.contains("5 verbs · 2 namespaces"));
        assert!(reply.text.find("grid 2").unwrap() < reply.text.find("wall 1").unwrap());
    }

    #[test]
    fn help_verb_shows_card_with_usage_and_returns_and_dot_is_optional() {
        let mut r = sample_help_router();
        let card = r.execute("help grid.list");
        let names: Vec<&str> = card.data.as_ref().unwrap()["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["grid.list"]);
        assert!(card.text.contains("returns  { grids }"));
        assert_eq!(r.execute("help grid list").text, card.text);
        assert!(r
            .execute("help grid.layout")
            .text
            .contains("usage    grid.layout <id> --mode <m>"));
    }

    #[test]
    fn help_namespace_shows_table_and_miss_says_so() {
        let mut r = sample_help_router();
        let ns = r.execute("help grid");
        let names: Vec<&str> = ns.data.as_ref().unwrap()["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["grid.layout", "grid.list"]);
        assert!(ns.text.contains("COMMANDS: grid*"));
        assert_eq!(
            r.execute("help zzz").text,
            "No commands matching 'zzz' — bare help maps the namespaces"
        );
    }

    // ── Port of tools/command-resolve.test.mjs ───────────────────────────────

    #[test]
    fn command_name_resolution_exact_dot_free_longest_chain_prefix_and_ambiguity() {
        let mut router = CommandRouter::new(());
        for name in [
            "help",
            "select",
            "select.grid",
            "grid.list",
            "grid.layout",
            "camera.frame",
            "camera.frame.bounds",
            "camera.reset",
            "workspace.save",
        ] {
            let owned = name.to_string();
            router.register(name, RegisterMeta::default(), move |args, (), _| {
                Ok(CommandReply {
                    text: "OK".into(),
                    data: Some(json!({ "name": owned, "args": args })),
                })
            });
        }

        let res_line = |router: &mut CommandRouter<()>, input: &str| -> Value {
            let r = router.execute(input);
            r.data
                .filter(|d| d.get("name").is_some())
                .unwrap_or_else(|| json!(r.text))
        };

        // Exact names
        assert_eq!(
            res_line(&mut router, "grid.list"),
            json!({ "name": "grid.list", "args": [] })
        );
        assert_eq!(
            res_line(&mut router, "select grid"),
            json!({ "name": "select", "args": ["grid"] })
        );
        assert_eq!(
            res_line(&mut router, "select.grid 1"),
            json!({ "name": "select.grid", "args": ["1"] })
        );

        // Dot-free spelling & longest chain
        assert_eq!(
            res_line(&mut router, "grid list"),
            json!({ "name": "grid.list", "args": [] })
        );
        assert_eq!(
            res_line(&mut router, "grid layout jellyfish 2"),
            json!({ "name": "grid.layout", "args": ["jellyfish", "2"] })
        );
        assert_eq!(
            res_line(&mut router, "camera frame bounds"),
            json!({ "name": "camera.frame.bounds", "args": [] })
        );
        assert_eq!(
            res_line(&mut router, "camera frame bounds 12"),
            json!({ "name": "camera.frame.bounds", "args": ["12"] })
        );
        assert_eq!(
            res_line(&mut router, "camera frame g3"),
            json!({ "name": "camera.frame", "args": ["g3"] })
        );
        assert_eq!(
            res_line(&mut router, "Camera Frame"),
            json!({ "name": "camera.frame", "args": [] })
        );
        assert_eq!(
            router.execute(["grid", "list"]).data,
            Some(json!({ "name": "grid.list", "args": [] }))
        );
        assert_eq!(
            res_line(&mut router, "grid layout \"jelly fish\""),
            json!({ "name": "grid.layout", "args": ["jelly fish"] })
        );

        // Unambiguous prefix completion
        assert_eq!(
            res_line(&mut router, "worksp"),
            json!({ "name": "workspace.save", "args": [] })
        );
        assert_eq!(
            res_line(&mut router, "worksp 5"),
            json!({ "name": "workspace.save", "args": ["5"] })
        );
        assert_eq!(
            res_line(&mut router, "grid li"),
            json!({ "name": "grid.list", "args": [] })
        );
        assert_eq!(
            res_line(&mut router, "grid li extra"),
            json!({ "name": "grid.list", "args": ["extra"] })
        );
        assert_eq!(
            res_line(&mut router, "camera res 1"),
            json!({ "name": "camera.reset", "args": ["1"] })
        );

        // Ambiguity and unknown misses
        let amb = router.execute("grid l");
        assert!(amb.text.starts_with("ERR: ambiguous"));
        assert_eq!(
            amb.data.unwrap()["matches"],
            json!(["grid.layout", "grid.list"])
        );

        let bare_ns = router.execute("camera");
        assert!(bare_ns.text.starts_with("ERR: ambiguous"));
        assert_eq!(bare_ns.data.unwrap()["matches"].as_array().unwrap().len(), 3);

        assert!(router.execute("zzz nope").text.starts_with("ERR: unknown"));
    }

    // ── Port of secret redaction & middleware checks (tools/relay-auth.test.mjs) ──

    #[test]
    fn middleware_runs_before_commands_and_telemetry_redacts_secrets() {
        let scrubbed = redact_relay_secrets(
            "dial http://localhost:8180/#view=1&token=MY_HASH_TOK;x=1 and g3d-token.MY_PROTO_TOK and Bearer MY_BEARER_TOK",
        );
        assert!(!scrubbed.contains("MY_HASH_TOK"));
        assert!(!scrubbed.contains("MY_PROTO_TOK"));
        assert!(!scrubbed.contains("MY_BEARER_TOK"));
        assert!(scrubbed.contains("token=[REDACTED]"));
        assert!(scrubbed.contains("g3d-token.[REDACTED]"));
        assert!(scrubbed.contains("Bearer [REDACTED]"));

        assert_eq!(redact_command_args("relay.token", &["SECRET_1".into()]), vec!["[REDACTED]"]);
        assert_eq!(redact_command_args("wall.token", &["SECRET_2".into()]), vec!["[REDACTED]"]);
        assert_eq!(
            redact_command_args("relay.connect", &["8180".into(), "SECRET_3".into()]),
            vec!["8180", "[REDACTED]"]
        );
        assert_eq!(
            redact_command_args("relay.connect", &["SWAPPED_SECRET_4".into()]),
            vec!["[REDACTED]"]
        );
        assert_eq!(
            redact_command_args(
                "relay.connect",
                &["http://localhost:5180/?relay=8180#token=SECRET_5".into()]
            ),
            vec!["http://localhost:5180/?relay=8180#token=[REDACTED]"]
        );

        assert!(is_secret_command("relay.token abc"));
        assert!(is_secret_command("wall token abc"));
        assert!(is_secret_command("relay.connect 8180 abc"));
        assert!(is_secret_command("relay.connect raw_secret"));
        assert!(is_secret_command("relay.connect http://localhost:8180/#token=abc"));
        assert!(!is_secret_command("relay.connect 8180"));
        assert!(!is_secret_command("relay.connect"));
        assert!(!is_secret_command("grid.list"));

        let mut router = CommandRouter::new((false, 0usize));
        router.use_middleware(|_name, _args, ctx, _meta| {
            ctx.1 += 1;
        });
        router.register("relay.token", RegisterMeta::default(), |_args, ctx, _| {
            if ctx.0 {
                Err("relay.token boom".into())
            } else {
                Ok("OK".into())
            }
        });
        router.register("wall.token", RegisterMeta::default(), |_args, ctx, _| {
            if ctx.0 {
                Err("wall.token boom".into())
            } else {
                Ok("OK".into())
            }
        });
        router.register("relay.connect", RegisterMeta::default(), |_args, ctx, _| {
            if ctx.0 {
                Err("relay.connect boom".into())
            } else {
                Ok("OK".into())
            }
        });

        let secrets = [
            "TOP_SECRET_RELAY_OK_111",
            "TOP_SECRET_WALL_OK_222",
            "TOP_SECRET_WALL_SPACE_333",
            "TOP_SECRET_CONNECT_ARG_444",
            "TOP_SECRET_CONNECT_HASH_555",
            "TOP_SECRET_RELAY_ERR_666",
            "TOP_SECRET_WALL_ERR_777",
            "TOP_SECRET_CONNECT_ERR_888",
        ];

        router.execute(format!("relay.token {}", secrets[0]));
        router.execute(format!("wall.token {}", secrets[1]));
        router.execute(format!("wall token {}", secrets[2]));
        router.execute(format!("relay.connect 8180 {}", secrets[3]));
        router.execute(format!(
            "relay.connect http://localhost:5180/?relay=8180#token={}",
            secrets[4]
        ));

        router.context.0 = true;
        assert!(router.execute(format!("relay.token {}", secrets[5])).is_err());
        assert!(router.execute(format!("wall.token {}", secrets[6])).is_err());
        assert!(router
            .execute(format!("relay.connect 8180 {}", secrets[7]))
            .is_err());

        assert_eq!(router.context.1, 8);

        assert_eq!(router.traces().len(), 8);
        for trace in router.traces() {
            let joined = format!("{trace:?}");
            for s in secrets {
                assert!(!joined.contains(s), "secret {s} leaked into trace: {joined}");
            }
            assert!(trace.args.iter().any(|a| a.contains("[REDACTED]")));
        }
    }
}
