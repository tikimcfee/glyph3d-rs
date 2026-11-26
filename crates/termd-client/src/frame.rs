//! Wire frames: the 12-byte little-endian header every message carries, and the binary
//! prefixes of `OUTPUT` and `REPAINT` data frames (termd protocol 1.5).
//!
//! Adapted from `g3d-wire` (MIT, same owner) for edition 2021.

/// Default maximum frame payload when `WELCOME.max_frame` does not specify otherwise: 16 MiB.
pub const DEFAULT_MAX_FRAME: usize = 16 * 1024 * 1024;

/// Header bytes counted inside `len`: `type` (1) + `flags` (1) + `reserved` (2) + `stream` (4).
const HEADER_AFTER_LEN: usize = 8;

/// Total wire header size: 4-byte `len` plus the 8 bytes after it.
pub const HEADER_LEN: usize = 4 + HEADER_AFTER_LEN;

/// `OUTPUT` binary prefix length (`u64` `seq` LE).
pub const OUTPUT_PREFIX: usize = 8;

/// `REPAINT` binary prefix length (`u64` `seq` + `u16` `cols` + `u16` `rows` + `u32` `flags`, LE).
pub const REPAINT_PREFIX: usize = 16;

/// `REPAINT` `flags` bit 0: repaint shows a scrollback view rather than the live screen.
pub const REPAINT_FLAG_VIEW: u32 = 1;

/// Wire frame type tags. Client-to-daemon tags sit below `0x80`; daemon-to-client above.
pub mod ty {
    pub const HELLO: u8 = 0x01;
    pub const REQUEST: u8 = 0x02;
    pub const INPUT: u8 = 0x03;
    pub const WELCOME: u8 = 0x81;
    pub const REPLY: u8 = 0x82;
    pub const OUTPUT: u8 = 0x83;
    pub const EVENT: u8 = 0x84;
    pub const REPAINT: u8 = 0x85;
    pub const CELLS: u8 = 0x86;
    pub const ERROR: u8 = 0xFF;
}

/// One complete frame decoded from the Unix stream socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub ty: u8,
    pub flags: u8,
    pub stream: u32,
    pub payload: Vec<u8>,
}

/// Structural framing violation on the byte stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// `len` below 8 cannot hold the mandatory header fields.
    BadLen(u32),
    /// Frame payload exceeded the negotiated `max_frame` ceiling.
    TooLarge { len: usize, max: usize },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadLen(len) => write!(f, "frame len {len} is below the 8-byte header"),
            FrameError::TooLarge { len, max } => {
                write!(f, "frame payload {len} B exceeds max_frame {max} B")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Appends a 12-byte frame header into `out` for a payload of `payload_len` bytes.
pub fn put_header(out: &mut Vec<u8>, frame_type: u8, stream: u32, payload_len: usize) {
    let len = u32::try_from(HEADER_AFTER_LEN + payload_len).expect("frame length fits u32");
    out.extend_from_slice(&len.to_le_bytes());
    out.push(frame_type);
    out.push(0); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&stream.to_le_bytes());
}

/// Encodes a complete binary frame.
pub fn encode(frame_type: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    put_header(&mut out, frame_type, stream, payload.len());
    out.extend_from_slice(payload);
    out
}

/// Encodes a JSON control frame (`HELLO`, `REQUEST`, `WELCOME`, `REPLY`, `EVENT`, `ERROR`).
pub fn encode_json<T: serde::Serialize>(frame_type: u8, stream: u32, value: &T) -> Vec<u8> {
    let json = serde_json::to_vec(value).expect("protocol control payload serializes to JSON");
    encode(frame_type, stream, &json)
}

/// Streaming decoder that reassembles whole [`Frame`]s across arbitrary socket read boundaries.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
    max_frame: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME)
    }
}

impl FrameDecoder {
    pub fn new(max_frame: usize) -> Self {
        Self { buf: Vec::new(), start: 0, max_frame }
    }

    pub fn set_max_frame(&mut self, max_frame: usize) {
        self.max_frame = max_frame;
    }

    /// Appends freshly read socket bytes into the reassembly buffer.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 && self.start * 2 > self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Yields the next complete frame, or `Ok(None)` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let avail = &self.buf[self.start..];
        if avail.len() < 4 {
            return Ok(None);
        }
        let len_bytes: [u8; 4] = avail[..4].try_into().expect("slice checked to have 4 bytes");
        let len = u32::from_le_bytes(len_bytes);
        if (len as usize) < HEADER_AFTER_LEN {
            return Err(FrameError::BadLen(len));
        }
        let payload_len = len as usize - HEADER_AFTER_LEN;
        if payload_len > self.max_frame {
            return Err(FrameError::TooLarge { len: payload_len, max: self.max_frame });
        }
        if avail.len() < 4 + len as usize {
            return Ok(None);
        }
        let stream_bytes: [u8; 4] = avail[8..12].try_into().expect("slice checked to hold header");
        let frame = Frame {
            ty: avail[4],
            flags: avail[5],
            stream: u32::from_le_bytes(stream_bytes),
            payload: avail[HEADER_LEN..HEADER_LEN + payload_len].to_vec(),
        };
        self.start += 4 + len as usize;
        Ok(Some(frame))
    }

    /// Number of unparsed bytes currently buffered.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }
}

/// Encodes a complete `OUTPUT` frame (`u64` `seq` LE followed by raw PTY bytes).
pub fn encode_output(stream: u32, seq: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + OUTPUT_PREFIX + bytes.len());
    put_header(&mut out, ty::OUTPUT, stream, OUTPUT_PREFIX + bytes.len());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Splits an `OUTPUT` payload into `(seq, pty_bytes)`.
pub fn parse_output(payload: &[u8]) -> Option<(u64, &[u8])> {
    if payload.len() < OUTPUT_PREFIX {
        return None;
    }
    let seq_bytes: [u8; 8] = payload[..8].try_into().expect("checked 8-byte prefix");
    Some((u64::from_le_bytes(seq_bytes), &payload[OUTPUT_PREFIX..]))
}

/// Fixed binary header of a `REPAINT` payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepaintHead {
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub flags: u32,
}

/// Encodes a complete `REPAINT` frame.
pub fn encode_repaint(stream: u32, head: RepaintHead, vt: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + REPAINT_PREFIX + vt.len());
    put_header(&mut out, ty::REPAINT, stream, REPAINT_PREFIX + vt.len());
    out.extend_from_slice(&head.seq.to_le_bytes());
    out.extend_from_slice(&head.cols.to_le_bytes());
    out.extend_from_slice(&head.rows.to_le_bytes());
    out.extend_from_slice(&head.flags.to_le_bytes());
    out.extend_from_slice(vt);
    out
}

/// Splits a `REPAINT` payload into `(RepaintHead, vt_bytes)`.
pub fn parse_repaint(payload: &[u8]) -> Option<(RepaintHead, &[u8])> {
    if payload.len() < REPAINT_PREFIX {
        return None;
    }
    let seq_bytes: [u8; 8] = payload[..8].try_into().expect("checked 16-byte prefix");
    let col_bytes: [u8; 2] = payload[8..10].try_into().expect("checked 16-byte prefix");
    let row_bytes: [u8; 2] = payload[10..12].try_into().expect("checked 16-byte prefix");
    let flag_bytes: [u8; 4] = payload[12..16].try_into().expect("checked 16-byte prefix");
    let head = RepaintHead {
        seq: u64::from_le_bytes(seq_bytes),
        cols: u16::from_le_bytes(col_bytes),
        rows: u16::from_le_bytes(row_bytes),
        flags: u32::from_le_bytes(flag_bytes),
    };
    Some((head, &payload[REPAINT_PREFIX..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_reassembles_arbitrary_chunk_cuts() {
        let mut wire = Vec::new();
        for index in 0..250u32 {
            wire.extend_from_slice(&encode_output(index + 1, u64::from(index) * 4, b"ping"));
        }
        let mut decoder = FrameDecoder::default();
        let mut decoded = 0u32;
        for chunk in wire.chunks(5) {
            decoder.feed(chunk);
            while let Some(frame) = decoder.next_frame().unwrap() {
                assert_eq!(frame.ty, ty::OUTPUT);
                assert_eq!(frame.stream, decoded + 1);
                assert_eq!(
                    parse_output(&frame.payload).unwrap(),
                    (u64::from(decoded) * 4, b"ping".as_slice())
                );
                decoded += 1;
            }
        }
        assert_eq!(decoded, 250);
        assert_eq!(decoder.pending(), 0);
    }

    #[test]
    fn decoder_rejects_short_and_oversized_frames() {
        let mut decoder = FrameDecoder::default();
        decoder.feed(&[5, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(decoder.next_frame(), Err(FrameError::BadLen(5)));

        let mut capped = FrameDecoder::new(12);
        capped.feed(&encode(ty::INPUT, 1, &[0u8; 13]));
        assert_eq!(capped.next_frame(), Err(FrameError::TooLarge { len: 13, max: 12 }));
    }

    #[test]
    fn repaint_prefix_round_trips_view_flag() {
        let head = RepaintHead { seq: 99, cols: 120, rows: 40, flags: REPAINT_FLAG_VIEW };
        let encoded = encode_repaint(7, head, b"\x1b[!phello");
        let mut decoder = FrameDecoder::default();
        decoder.feed(&encoded);
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!((frame.ty, frame.stream), (ty::REPAINT, 7));
        let (parsed_head, bytes) = parse_repaint(&frame.payload).unwrap();
        assert_eq!(parsed_head, head);
        assert_eq!(bytes, b"\x1b[!phello");
    }
}
