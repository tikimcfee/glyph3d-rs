//! The view registry: one row per view, naming everything a view is drawn,
//! culled, picked and edited with (2026-10-11; step A of
//! `out/DESIGN-VIEWS-SLABS-POSES-2026-10-11.md`).
//!
//! **A view** is a byte range of one buffer, laid out with one recipe and
//! drawn with one group row (and so one node of the transform tree). Today
//! every producer registers one view per item: a repo load one per walked
//! file, an agent session one per text block, `--render-file` one per copy.
//!
//! **The view's index is its item row**, in every field and every table
//! that is per item: the row of the GPU item table (`ItemParamsGpu`, which
//! [`Views::item_table`] writes), the item lane of a Derived slot, the
//! Visible field's item, the pick record (`PickContext::files[view]`) and
//! the layout's per-item outputs (placements, tints, blocks). That is one
//! name for one thing.
//!
//! **Everything else is a field of the row**, and code asks for it here:
//! the group row ([`Views::group_row`]), the node ([`Views::node`]), the
//! cull segment ([`Views::segment`]), the slot range ([`Views::slots`]).
//! Until this table existed one integer wore all of those names at a repo
//! load — file `i` was item `i`, group `i`, node `nodes[i]`, segment `i`,
//! `files[i]` and Visible item `i` — and a dozen readers relied on the
//! equality without saying so (the design note's § 1.3 lists them). The
//! witness that none does any more is `native/tests/view_registry.rs`: a
//! load whose group rows are permuted against its views renders the
//! unpermuted frames byte for byte.
//!
//! The reverse questions — which views draw with group row `g`, which group
//! row a segment's views share, which view a host record of group `g` is in
//! — are answered from indices built once, at [`Views::new`].
//!
//! Not yet here (later steps of the note): sub-range views (`bytes` is the
//! whole buffer today), the recipe/origin split (the recipe still carries
//! the origin), pose slabs, and freeing or repointing a view.

use std::ops::Range;

use glyph_field::ItemParamsGpu;
use glyph_scene_graph::NodeHandle;

/// "No view" in the reverse indices (a group row or segment no view uses).
const NO_VIEW: u32 = u32::MAX;

/// One view as its producer registers it.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    /// The buffer the view's bytes come from, in its producer's numbering:
    /// the walked file's index at a repo load, the text block's at an agent
    /// session, 0 at `--render-file` (every copy reads the one file).
    pub buffer: u32,
    /// The bytes of that buffer the view covers: the whole buffer today.
    pub bytes: Range<u32>,
    /// The layout recipe, in the form the item table holds. Its `group`
    /// lane is not read: [`Views::item_table`] writes it from `group_row`.
    /// (The CPU re-derivation keeps the f64 form, `PickFileInfo::item`.)
    pub recipe: ItemParamsGpu,
    /// The group-table row the view draws with — its transform, tint and
    /// alpha — and so its node (row `r` is node `GroupNodes::node(r)`).
    pub group_row: u32,
    /// Its cull segment (`StagedText::segments`). Several views may share
    /// one: `--render-file`'s copies sit under one cover segment.
    pub segment: u32,
    /// The arena slots its glyphs occupy (empty where none are stored: the
    /// Visible field lays glyphs out per frame).
    pub slots: Range<u32>,
    /// Its extent before the group's transform: a repo file's is its cull
    /// segment's local box (ink plus the segment pads), the box the Visible
    /// field culls the item by; an agent block's and a text copy's are the
    /// boxes their producers measured.
    pub local_min: [f32; 3],
    pub local_max: [f32; 3],
}

/// The registry: every view, and the reverse indices.
#[derive(Clone, Debug, Default)]
pub struct Views {
    rows: Vec<View>,
    /// Views drawn with each group row, as one list sliced per row:
    /// `views_by_group_row[group_view_starts[g]..group_view_starts[g + 1]]`.
    group_view_starts: Vec<u32>,
    views_by_group_row: Vec<u32>,
    /// Per segment: the group row its views share, `None` when no view or
    /// views of several group rows sit under it.
    group_row_of_segment: Vec<Option<u32>>,
    /// Each view's node, once the scene has made its group rows nodes
    /// ([`Views::attach_nodes`]); empty until then.
    nodes: Vec<NodeHandle>,
}

impl Views {
    pub fn new(rows: Vec<View>) -> Self {
        let group_rows = rows.iter().map(|v| v.group_row as usize + 1).max().unwrap_or(0);
        let mut group_view_starts = vec![0u32; group_rows + 1];
        for v in &rows {
            group_view_starts[v.group_row as usize + 1] += 1;
        }
        for g in 0..group_rows {
            group_view_starts[g + 1] += group_view_starts[g];
        }
        let mut fill = group_view_starts.clone();
        let mut views_by_group_row = vec![NO_VIEW; rows.len()];
        for (view, v) in rows.iter().enumerate() {
            let at = &mut fill[v.group_row as usize];
            views_by_group_row[*at as usize] = view as u32;
            *at += 1;
        }
        let segments = rows.iter().map(|v| v.segment as usize + 1).max().unwrap_or(0);
        let mut first_group_row: Vec<u32> = vec![NO_VIEW; segments];
        let mut shared = vec![true; segments];
        for v in &rows {
            let s = v.segment as usize;
            if first_group_row[s] == NO_VIEW {
                first_group_row[s] = v.group_row;
            } else if first_group_row[s] != v.group_row {
                shared[s] = false;
            }
        }
        let group_row_of_segment =
            first_group_row.iter().zip(&shared).map(|(&g, &one)| (g != NO_VIEW && one).then_some(g)).collect();
        Self { rows, group_view_starts, views_by_group_row, group_row_of_segment, nodes: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn row(&self, view: u32) -> &View {
        self.rows.get(view as usize).unwrap_or_else(|| panic!("view {view} is not registered ({} views)", self.rows.len()))
    }

    /// The group row view `view` draws with.
    pub fn group_row(&self, view: u32) -> u32 {
        self.row(view).group_row
    }

    /// View `view`'s cull segment.
    pub fn segment(&self, view: u32) -> u32 {
        self.row(view).segment
    }

    /// The arena slots view `view`'s glyphs occupy.
    pub fn slots(&self, view: u32) -> Range<u32> {
        self.row(view).slots.clone()
    }

    /// The buffer view `view` reads and the bytes of it it covers.
    pub fn bytes(&self, view: u32) -> (u32, Range<u32>) {
        let v = self.row(view);
        (v.buffer, v.bytes.clone())
    }

    /// View `view`'s local extent, `(min, max)`.
    pub fn local_extent(&self, view: u32) -> ([f32; 3], [f32; 3]) {
        let v = self.row(view);
        (v.local_min, v.local_max)
    }

    /// The views drawn with group row `group_row`, ascending (none for a row
    /// no view draws with: a spare, or one a glyph verb allocated).
    pub fn views_drawn_with(&self, group_row: u32) -> &[u32] {
        let g = group_row as usize;
        match (self.group_view_starts.get(g), self.group_view_starts.get(g + 1)) {
            (Some(&lo), Some(&hi)) => &self.views_by_group_row[lo as usize..hi as usize],
            _ => &[],
        }
    }

    /// The group row every view under `segment` draws with; `None` when no
    /// view sits under it or views of several rows do (a cover segment,
    /// whose box is world space and follows no one group).
    pub fn group_row_of_segment(&self, segment: u32) -> Option<u32> {
        self.group_row_of_segment.get(segment as usize).copied().flatten()
    }

    /// The segments that follow group row `group_row`: each segment of a
    /// view drawn with it whose views all draw with it, ascending, once
    /// each. A group edit re-derives these (and hides these), and no other.
    pub fn segments_following(&self, group_row: u32) -> Vec<u32> {
        let mut segments: Vec<u32> = self
            .views_drawn_with(group_row)
            .iter()
            .map(|&view| self.segment(view))
            .filter(|&segment| self.group_row_of_segment(segment) == Some(group_row))
            .collect();
        segments.sort_unstable();
        segments.dedup();
        segments
    }

    /// The GPU item table: row `view` is the view's recipe, with the group
    /// lane written from its group row — the one place that lane is set.
    pub fn item_table(&self) -> Vec<ItemParamsGpu> {
        (0..self.rows.len() as u32)
            .map(|view| ItemParamsGpu { group: self.group_row(view), ..self.row(view).recipe })
            .collect()
    }

    /// For each of `group_rows` group rows, the item (view) a host record
    /// naming it belongs to — `glyph_field::NO_ITEM` for a row no view, or
    /// several, draw with. A host record (`GlyphInstance`) carries its group
    /// row and not its item, so the Derived field's host transcode asks
    /// here which item lane to write (until step A it wrote the group id,
    /// and every host producer padded its item table to make them agree).
    pub fn item_of_group_rows(&self, group_rows: usize) -> Vec<u32> {
        (0..group_rows as u32)
            .map(|g| match self.views_drawn_with(g) {
                [view] => *view,
                _ => glyph_field::NO_ITEM,
            })
            .collect()
    }

    /// Record each view's node: the node of its group row, as the scene's
    /// group nodes answer (`node_of_group_row`).
    pub fn attach_nodes(&mut self, node_of_group_row: impl Fn(u32) -> Option<NodeHandle>) {
        self.nodes = (0..self.rows.len() as u32)
            .map(|view| {
                let group_row = self.group_row(view);
                node_of_group_row(group_row).unwrap_or_else(|| panic!("view {view}'s group row {group_row} has no node"))
            })
            .collect();
    }

    /// View `view`'s node (after [`Views::attach_nodes`]).
    pub fn node(&self, view: u32) -> NodeHandle {
        *self.nodes.get(view as usize).unwrap_or_else(|| panic!("view {view} has no node (attach_nodes not run?)"))
    }

    /// Every view's node, in view order (after [`Views::attach_nodes`]).
    pub fn nodes(&self) -> &[NodeHandle] {
        assert_eq!(self.nodes.len(), self.rows.len(), "attach_nodes runs before the nodes are read");
        &self.nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(group_row: u32, segment: u32) -> View {
        View {
            buffer: 0,
            bytes: 0..10,
            recipe: ItemParamsGpu { line_height: 1.25, group: 999, ..Default::default() },
            group_row,
            segment,
            slots: 0..0,
            local_min: [0.0; 3],
            local_max: [1.0; 3],
        }
    }

    /// The reverse questions over a table whose group rows are not the views'
    /// indices: three files filed under rows 3, 2, 1 (row 0 spare), plus a
    /// cover segment two views of different rows share.
    #[test]
    fn the_registry_answers_by_field_not_by_index() {
        let views = Views::new(vec![view(3, 0), view(2, 1), view(1, 2), view(2, 3), view(1, 3)]);
        assert_eq!((0..5).map(|v| views.group_row(v)).collect::<Vec<_>>(), [3, 2, 1, 2, 1]);
        assert_eq!(views.views_drawn_with(0), &[] as &[u32], "the spare row draws nothing");
        assert_eq!(views.views_drawn_with(2), &[1, 3]);
        assert_eq!(views.views_drawn_with(3), &[0]);
        assert_eq!(views.views_drawn_with(77), &[] as &[u32], "a verb-allocated row past the load");
        assert_eq!(views.group_row_of_segment(0), Some(3));
        assert_eq!(views.group_row_of_segment(3), None, "two group rows under one segment");
        assert_eq!(views.group_row_of_segment(9), None);
        assert_eq!(views.segments_following(3), [0]);
        assert_eq!(views.segments_following(2), [1], "the shared segment 3 follows neither of its rows");
        assert_eq!(views.segments_following(0), [] as [u32; 0]);
        let table = views.item_table();
        assert_eq!(table.iter().map(|p| p.group).collect::<Vec<_>>(), [3, 2, 1, 2, 1], "the group lane is the registry's, never the recipe's");
        assert!(table.iter().all(|p| p.line_height == 1.25));
        assert_eq!(views.item_of_group_rows(5), [glyph_field::NO_ITEM, glyph_field::NO_ITEM, glyph_field::NO_ITEM, 0, glyph_field::NO_ITEM]);
    }
}
