//! Byte stream -> terminal. Port of ghostty `src/terminal/stream.zig`
//! (scalar path; the SIMD fast paths are not ported).

use crate::term::parser::{Parser, State};
use crate::term::terminal::Terminal;
use crate::term::utf8::Utf8Decoder;

pub struct Stream {
    pub parser: Parser,
    pub utf8: Utf8Decoder,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            utf8: Utf8Decoder::new(),
        }
    }

    pub fn process(&mut self, bytes: &[u8], terminal: &mut Terminal) {
        let mut i = 0;
        let len = bytes.len();
        while i < len {
            if self.parser.state == State::Ground && self.utf8.is_ground() {
                let start = i;
                while i < len {
                    let b = bytes[i];
                    if (0x20..=0x7e).contains(&b) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if i > start {
                    terminal.print_ascii_slice(&bytes[start..i]);
                    if i >= len {
                        break;
                    }
                }
                // Fast ESC / CR / LF handling without routing through Utf8Decoder.
                match bytes[i] {
                    0x1b => {
                        self.parser.state = State::Escape;
                        self.parser.clear();
                        i += 1;
                        continue;
                    }
                    b'\r' => {
                        terminal.execute(b'\r');
                        i += 1;
                        if i < len && bytes[i] == b'\n' {
                            terminal.execute(b'\n');
                            i += 1;
                        }
                        continue;
                    }
                    b'\n' => {
                        terminal.execute(b'\n');
                        i += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            self.next(bytes[i], terminal);
            i += 1;
        }
    }

    #[inline]
    pub fn next(&mut self, byte: u8, terminal: &mut Terminal) {
        if self.parser.state == State::Ground {
            self.next_utf8(byte, terminal);
        } else {
            self.next_non_utf8(byte, terminal);
        }
    }

    #[inline]
    fn next_utf8(&mut self, byte: u8, terminal: &mut Terminal) {
        let (cp, consumed) = self.utf8.next(byte);
        if let Some(cp) = cp {
            self.handle_codepoint(cp, terminal);
        }
        if !consumed {
            let (cp, consumed2) = self.utf8.next(byte);
            debug_assert!(consumed2);
            if let Some(cp) = cp {
                self.handle_codepoint(cp, terminal);
            }
        }
    }

    /// A decoded codepoint in ground state: C0 executes, ESC enters the
    /// parser, UTF-8-decoded C1 is ignored (xterm behavior), the rest
    /// prints.
    #[inline]
    fn handle_codepoint(&mut self, cp: u32, terminal: &mut Terminal) {
        if (cp & !0x9f) == 0 {
            if cp == 0x1b {
                self.parser.state = State::Escape;
                self.parser.clear();
                return;
            }
            if cp > 0x1f {
                // C1 via UTF-8: ignore.
                return;
            }
            terminal.execute(cp as u8);
            return;
        }
        terminal.print(cp);
    }

    #[inline]
    fn next_non_utf8(&mut self, byte: u8, terminal: &mut Terminal) {
        debug_assert!(self.parser.state != State::Ground);
        self.parser.step(byte, |action| terminal.dispatch(action));
    }
}
