//! Line-in, length-prefixed-reply-out wire framing and CLI argument escaping.

use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

/// Maximum UTF-8 byte length of a single inbound command line (`1 MiB`).
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Maximum UTF-8 byte length of a single outbound reply payload (`16 MiB`).
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

/// Errors returned by [`crate::Server`] and [`crate::Client`].
#[derive(Debug)]
pub enum Error {
    /// Invalid namespace or overlong Unix socket path.
    InvalidTarget(String),
    /// Another live wall instance is already listening on this socket.
    AlreadyRunning {
        path: PathBuf,
        namespace: Option<String>,
    },
    /// Existing path at the socket address is not a Unix domain socket.
    NotASocket(PathBuf),
    /// Malformed or oversized wire frame.
    Protocol(String),
    /// Underlying socket or filesystem I/O error.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidTarget(msg) => f.write_str(msg),
            Error::AlreadyRunning {
                path,
                namespace: Some(ns),
            } => write!(
                f,
                "another wall is already running in namespace '{ns}' ({})",
                path.display()
            ),
            Error::AlreadyRunning {
                path,
                namespace: None,
            } => write!(f, "another wall is already listening on {}", path.display()),
            Error::NotASocket(path) => write!(
                f,
                "refusing to replace existing non-socket file at {}",
                path.display()
            ),
            Error::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Error::Io(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

/// Reply returned by the wall for one executed verb line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// Full reply text produced by the wall's command router.
    pub text: String,
}

impl Reply {
    pub fn new(text: impl Into<String>) -> Self {
        Reply { text: text.into() }
    }

    /// Whether the wall refused the command (`text.starts_with("ERR:")`, matching
    /// `wall_core::router::CommandReply::is_err`).
    pub fn is_err(&self) -> bool {
        self.text.starts_with("ERR:")
    }

    /// Whether the command succeeded (`!self.is_err()`).
    pub fn is_ok(&self) -> bool {
        !self.is_err()
    }
}

pub(crate) enum InboundLine {
    Eof,
    Line(String),
    TooLong,
    InvalidUtf8,
}

/// Read one `\n`-terminated request line (stripping `\r\n` or `\n`), bounded to
/// [`MAX_REQUEST_BYTES`].
pub(crate) fn read_request_line<R: BufRead>(reader: &mut R) -> io::Result<InboundLine> {
    let mut buf = Vec::new();
    let limit = (MAX_REQUEST_BYTES as u64) + 1;
    let n = reader.take(limit).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(InboundLine::Eof);
    }
    if buf.len() > MAX_REQUEST_BYTES && buf.last() != Some(&b'\n') {
        return Ok(InboundLine::TooLong);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    if buf.len() > MAX_REQUEST_BYTES {
        return Ok(InboundLine::TooLong);
    }
    match String::from_utf8(buf) {
        Ok(line) => Ok(InboundLine::Line(line)),
        Err(_) => Ok(InboundLine::InvalidUtf8),
    }
}

/// Write one outbound request line (`<line>\n`), rejecting raw `\r` or `\n` inside `line`
/// so a single call always corresponds to exactly one wire request.
pub(crate) fn write_request_line<W: Write>(writer: &mut W, line: &str) -> Result<(), Error> {
    if line.contains('\n') || line.contains('\r') {
        return Err(Error::Protocol(
            "command line must not contain raw CR or LF (use \\n escapes or format_command_line)"
                .into(),
        ));
    }
    if line.len() > MAX_REQUEST_BYTES {
        return Err(Error::Protocol(format!(
            "command line is {} bytes, exceeding MAX_REQUEST_BYTES ({MAX_REQUEST_BYTES})",
            line.len()
        )));
    }
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Write one length-prefixed reply frame (`<byte_len>\n<utf8_bytes>\n`).
pub(crate) fn write_reply_frame<W: Write>(writer: &mut W, text: &str) -> Result<(), Error> {
    if text.len() > MAX_REPLY_BYTES {
        return Err(Error::Protocol(format!(
            "reply is {} bytes, exceeding MAX_REPLY_BYTES ({MAX_REPLY_BYTES})",
            text.len()
        )));
    }
    let header = format!("{}\n", text.len());
    writer.write_all(header.as_bytes())?;
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Read one length-prefixed reply frame (`<byte_len>\n<utf8_bytes>\n`).
pub(crate) fn read_reply_frame<R: BufRead>(reader: &mut R) -> Result<Reply, Error> {
    let mut header = Vec::with_capacity(24);
    let n = reader.take(32).read_until(b'\n', &mut header)?;
    if n == 0 {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "wall closed the control connection before replying",
        )));
    }
    if header.last() != Some(&b'\n') {
        return Err(Error::Protocol(
            "missing newline after reply byte-length header".into(),
        ));
    }
    header.pop();
    if header.last() == Some(&b'\r') {
        header.pop();
    }
    let header_str = std::str::from_utf8(&header)
        .map_err(|_| Error::Protocol("non-UTF-8 reply byte-length header".into()))?;
    let byte_len: usize = header_str.trim().parse().map_err(|_| {
        Error::Protocol(format!(
            "expected decimal reply byte length, got {header_str:?}"
        ))
    })?;
    if byte_len > MAX_REPLY_BYTES {
        return Err(Error::Protocol(format!(
            "reply length {byte_len} exceeds MAX_REPLY_BYTES ({MAX_REPLY_BYTES})"
        )));
    }

    let mut payload = vec![0u8; byte_len];
    reader.read_exact(&mut payload)?;

    let mut term = [0u8; 1];
    reader.read_exact(&mut term)?;
    if term[0] != b'\n' {
        return Err(Error::Protocol(
            "missing trailing newline after reply payload".into(),
        ));
    }

    let text = String::from_utf8(payload)
        .map_err(|e| Error::Protocol(format!("reply payload is not valid UTF-8: {e}")))?;
    Ok(Reply { text })
}

/// Format an OS `argv` slice (`[verb, arg1, arg2, ...]`) into a single command line
/// matching `wall_core::router::CommandRouter::parse_line`'s quoting and escape rules.
///
/// Plain words (`wall.split`, `right`, `-10`) pass through bare; arguments containing
/// spaces, quotes, backslashes, newlines, or tabs are wrapped in `"..."` with `\\`,
/// `\"`, `\n`, and `\t` escaped so `CommandRouter::parse_line` reconstructs every
/// argument verbatim.
pub fn format_command_line<S: AsRef<str>>(argv: &[S]) -> String {
    let mut parts = Vec::with_capacity(argv.len());
    for raw in argv {
        let s = raw.as_ref();
        let needs_quotes = s.is_empty()
            || s.chars()
                .any(|c| c == ' ' || c == '"' || c == '\\' || c == '\n' || c == '\r' || c == '\t');
        if !needs_quotes {
            parts.push(s.to_string());
            continue;
        }
        let mut quoted = String::with_capacity(s.len() + 2);
        quoted.push('"');
        for ch in s.chars() {
            match ch {
                '\\' => quoted.push_str("\\\\"),
                '"' => quoted.push_str("\\\""),
                '\n' => quoted.push_str("\\n"),
                '\t' => quoted.push_str("\\t"),
                '\r' => {}
                other => quoted.push(other),
            }
        }
        quoted.push('"');
        parts.push(quoted);
    }
    parts.join(" ")
}
