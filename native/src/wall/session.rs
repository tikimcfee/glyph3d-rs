//! Live terminals: re-exports [`term_session`]'s multi-threaded [`Sessions`] and [`News`] seam
//! so `app.rs` runs every terminal's emulator on its own background thread without any change
//! outside its import.

pub use term_session::{NAMESPACE, News, Sessions};
