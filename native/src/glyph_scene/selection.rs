//! The scene's selection (moved out of `target.rs`, whose pooled targets
//! are the wgpu renderer's, 2026-10-31).

/// Stage L (L4): selection state — replaces the Stage G click-flash hack
/// (instance-byte write + restore). Set by apply_pick (the exact API the
/// CLI op-stream and the windowed click share): a glyph pick with a real
/// slot selects that glyph; a file-level pick (or a blank-glyph pick, which
/// has no slot) selects the whole segment; a pick MISS clears. Verbs never
/// touch it. Persistent until the next pick — this closes the "sticky
/// flash" gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Selection {
    /// One glyph slot (arena-global split into chunk + chunk-local index).
    Glyph { chunk: u32, local: u32 },
    /// A whole segment/file (arena slot range; split per chunk at draw).
    Segment { slot_base: u32, slot_count: u32 },
}

impl Selection {
    /// The HUD's one-word form: what is selected, in the field's own key.
    pub(super) fn describe(&self) -> String {
        match self {
            Selection::Glyph { chunk, local } => format!("slot chunk {chunk} local {local}"),
            Selection::Segment { slot_base, slot_count } => format!("slots {slot_base}..+{slot_count}"),
        }
    }
}
