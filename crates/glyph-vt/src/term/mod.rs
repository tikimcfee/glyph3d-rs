//! terminal's VT core: a clean Rust port of Ghostty's terminal subsystem.
//!
//! Ported from github.com/ghostty-org/ghostty (`src/terminal/`, `src/input/`),
//! MIT License, Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors.
//! The UTF-8 decoder additionally derives from Bjoern Hoehrmann's DFA decoder
//! (<http://bjoern.hoehrmann.de/utf-8/decoder/dfa>, MIT).
//!
//! Layering (bottom to top), mirroring Ghostty:
//!
//!   utf8      DFA UTF-8 decoder (ground-state text path)
//!   parser    vt100.net DEC ANSI state machine -> Action (CSI/ESC/OSC/DCS/APC)
//!   osc       OSC command parser, driven byte-wise by `parser`
//!   sgr       SGR (ESC [ ... m) attribute parser incl. colon subparams
//!   modes     DEC/ANSI mode registry with save/restore + DECRQM reports
//!   charsets  G0..G3 slots, DEC Special Graphics
//!   tabstops  tab stop bitmap
//!   cell_width  codepoint width + grapheme break, mode 2027
//!   page      cell/row storage with scrollback
//!   screen    cursor + page view (primary/alternate)
//!   terminal  the control functions (CSI/ESC/OSC semantics)
//!   stream    bytes -> utf8/parser -> terminal dispatch
//!
//! Input encoders (`key_encode`, `mouse_encode`) from makepad's port are
//! omitted here because `g3d-termd` encodes input on the daemon side.
//! Everything in this module is pure: no Makepad, no PTY, no I/O.

pub mod charsets;
pub mod color;
pub mod modes;
pub mod osc;
pub mod page;
pub mod parser;
pub mod screen;
pub mod sgr;
pub mod stream;
pub mod style;
pub mod tabstops;
pub mod terminal;
pub mod utf8;
