//! The definition engine: the "infinite definition" loop, UI-free.
//!
//! A reader holds a passage. They select a word in it (a tap on one glyph
//! resolves to the word around it, [`text::word_at`]), and the engine records
//! a [`Request`] for its expansion: definitions, translations, related
//! topics and substitutes, asked in the sentence the word stands in. An agent
//! answers it ([`Run::answer`]); the reader takes one of the answers, or
//! their own text, in the word's place ([`Run::substitute`]), and that
//! substitution is a [`Step`]: one diff, one card on the run's stack. The new
//! text can be selected in again, to any depth. A run's versions are its base
//! text with a prefix of its steps applied; any version can be viewed
//! ([`Run::checkout`]) or forked into a run of its own ([`Run::fork`]), which
//! is how a reader keeps a personal definition-run.
//!
//! - [`text`]: spans, the word a tap selects, the context it is asked in.
//! - [`expansion`]: what an agent answers with, validated before it is kept.
//! - [`run`]: a run, its requests and its diff stack, and the invariants a
//!   loaded run is held to.
//! - [`store`]: runs on disk, one JSON file each. The store directory is
//!   the shared state between the `defn` CLI, an agent and a renderer.
//! - [`layers`]: what a tree holds, by layer: code, raw binaries,
//!   directories and extension-named files. A path can sit in several.
//!
//! Not here yet: the invocation map (program and function invocations,
//! their parameters and returned values, read from agent transcripts). Its
//! one reader of transcripts is the renderer's `agent_transcript`, which
//! will build the map and hand it to this crate. This crate does not parse
//! transcripts.

pub mod expansion;
pub mod layers;
pub mod run;
pub mod store;
pub mod text;

pub use expansion::{Expansion, Sense, Topic, Translation};
pub use layers::{Layer, LayerSet};
pub use run::{Choice, ForkPoint, Request, Run, Source, Step};
pub use store::Store;
pub use text::Span;

/// Seconds since the Unix epoch: every timestamp a run records.
pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock reads before 1970")
        .as_secs()
}
