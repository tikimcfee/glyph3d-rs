//! `--layout-mode library` (2026-10-10): every file is a BOOK fitted onto a
//! uniform page, and the repository is a library of them — a port of the
//! retired JS renderer's "library" scheme and its Book carrier, built as a
//! PROBE: the first layout here that is not a flat wall. Files are scaled
//! (contain-fit), stacked in depth (a directory's volume is a rolodex deck),
//! nested (child directories one `depth_z` step back, in the parent's frame)
//! and animated (page turns, deck ↔ splay, stack and sort changes ease every
//! group). What that costs today's architecture is recorded in
//! `out/LIBRARY-LAYOUT-FINDINGS-2026-10-10.md`.
//!
//! - `book` — the carrier's arithmetic: slot laws (deck, splay), the splay
//!   grid, contain-fit, the easing step.
//! - `plan` — the scheme: directory tree, sort, stack extents, the
//!   serpentine child tier; measure post-order, place pre-order.
//! - `runtime` — the plan on the renderer's transform tree
//!   (`glyph-scene-graph` nodes: root → dir → volume → sheet → mount →
//!   file; bevy until 2026-10-10) and its animation.
//!
//! The contract with the renderer is the GroupRow, nothing else: a file's
//! group carries its world transform (T·R·S; the fit scale rides in S),
//! written by the scene graph's GPU resolve from the file node's chain; the
//! file's item origin is its local zero, and the slot formats and the
//! group-row format are untouched. Named values live in `[library]` of
//! `config/defaults.toml`.
//!
//! Runtime controls (all go through the verb path, so `--verb` scripts them
//! offscreen and the windowed keys call the same entry points): `page-next`,
//! `page-prev`, `page-first`, `page-last`, `page-to N` (keys `]`/`[` and
//! their aliases `n` `p` `.` `,` and the arrows, Home/End); `form
//! deck|splay|toggle` (key `v`); `library-stack x|y|z`; `library-sort
//! name|size|ext [reverse]`. Paging and form address the picked file's
//! volume, or every volume when nothing is picked.
//!
//! Bounding regions (`[library] regions`, the scene's
//! `glyph_scene/regions.rs`): a translucent box around every directory's
//! subtree; a click inside one that hits no file picks the innermost, and
//! `g` (or `c`) drags that directory through [`Library::move_zone`].

pub mod book;
pub mod plan;
pub mod runtime;

pub use plan::{Form, Sort, Stack};

/// Where a book's content depth sits against its page (`[library]
/// depth_align`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DepthAlign {
    /// The content's front (its reading surface) on the page plane; depth
    /// recedes into the book.
    Front,
    /// The content's depth centred on the page plane (the JS scheme).
    Center,
}
pub use runtime::{FaceSpec, FormCmd, LibNodes, Library, LibraryTick, Page};

/// One runtime control, as `--verb` spells it (and the windowed keys build).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryVerb {
    Page(Page),
    Form(FormCmd),
    Stack(Stack),
    /// The sort and whether it is reversed.
    Sort(Sort, bool),
}

impl LibraryVerb {
    /// `word` is the verb's first token, `args` the rest.
    pub fn parse(word: &str, args: &[&str]) -> Result<Self, String> {
        let arg = |i: usize, what: &str| args.get(i).copied().ok_or_else(|| format!("{word}: missing {what}"));
        Ok(match word {
            "page-next" => LibraryVerb::Page(Page::Next),
            "page-prev" => LibraryVerb::Page(Page::Prev),
            "page-first" => LibraryVerb::Page(Page::First),
            "page-last" => LibraryVerb::Page(Page::Last),
            "page-to" => {
                let n: usize = arg(0, "page number (1-based)")?
                    .parse()
                    .map_err(|_| format!("{word}: page number must be a positive integer"))?;
                if n == 0 {
                    return Err(format!("{word}: pages count from 1"));
                }
                LibraryVerb::Page(Page::To(n - 1))
            }
            "form" => LibraryVerb::Form(match arg(0, "deck|splay|toggle")? {
                "deck" => FormCmd::Set(Form::Deck),
                "splay" => FormCmd::Set(Form::Splay),
                "toggle" => FormCmd::Toggle,
                other => return Err(format!("{word}: {other:?} is not deck|splay|toggle")),
            }),
            "library-stack" => LibraryVerb::Stack(match arg(0, "x|y|z")? {
                "x" => Stack::X,
                "y" => Stack::Y,
                "z" => Stack::Z,
                other => return Err(format!("{word}: {other:?} is not x|y|z")),
            }),
            "library-sort" => {
                let sort = match arg(0, "name|size|ext")? {
                    "name" => Sort::Name,
                    "size" => Sort::Size,
                    "ext" => Sort::Ext,
                    other => return Err(format!("{word}: {other:?} is not name|size|ext")),
                };
                let reverse = match args.get(1).copied() {
                    None => false,
                    Some("reverse") => true,
                    Some(other) => return Err(format!("{word}: {other:?} is not 'reverse'")),
                };
                LibraryVerb::Sort(sort, reverse)
            }
            other => return Err(format!("{other:?} is not a library verb")),
        })
    }
}

/// Each file's group row as the tree resolves it (on the CPU, the same
/// arithmetic as the GPU resolve): world translation, rotation and the
/// uniform scale, the directory tint at alpha 1, blend 0 — the columns
/// `sync_all_to_group_rows` used to write off bevy's GlobalTransform, bit
/// for bit (`tests::the_tree_resolves_the_rows_bevy_composed`).
pub fn seated_rows(t: &glyph_scene_graph::NodeTables, nodes: &LibNodes, tints: &[[f32; 3]]) -> Vec<crate::glyph_scene::GroupRow> {
    nodes
        .file
        .iter()
        .zip(tints)
        .map(|(&f, tint)| {
            let (w, _) = t.world(f).expect("library nodes are live");
            let mut g = crate::glyph_scene::GroupRow::tinted([0.0; 3], *tint);
            g.cols[0] = [w.translation[0], w.translation[1], w.translation[2], 0.0];
            g.cols[1] = w.rotation;
            g.cols[3] = [w.scale, w.scale, w.scale, 0.0];
            g
        })
        .collect()
}

#[cfg(test)]
mod tests;
