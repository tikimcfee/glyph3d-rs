//! The library scheme's measure/place over a directory tree — a port of the
//! retired JS renderer's `collections/layouts/libraryLayout.js` (with
//! `flowBoxes.js` and `nodeUtils.partitionChildren`'s sort and chain
//! compression). Pure: files in, a `Plan` of LOCAL positions out (children in
//! their parent's frame, a node's origin at its footprint's top centre),
//! measured post-order and placed pre-order.
//!
//! One deliberate change of shape, not of result: the JS fits each book of
//! an `x` shelf or `y` pile individually and only binds a `z` stack into a
//! volume. Here EVERY directory with files has a volume node (at the page
//! centre under the stack origin, as the JS volume sits) and the shelf and
//! pile are two more slot laws beside deck and splay, so the transform
//! hierarchy (dir → volume → sheet → mount → file) is the same shape in
//! every stack and a stack change is a retarget that eases, never a rebuild.
//! World positions are the JS's: `volume + slot` equals the JS book position.

use std::collections::HashMap;

use super::book::{contain_fit, deck_slot, splay_grid, splay_slot, Fit, SplayDims};

/// How a directory's books stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    /// The VOLUME: the directory bound as one pageable book (deck or splay).
    Z,
    /// A shelf: books abreast left to right.
    X,
    /// A pile: books descending one below the other.
    Y,
}

/// Which question the sort asks of a directory's books.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    /// Case-insensitive file name.
    Name,
    /// Natural content area, biggest first.
    Size,
    /// Extension (genre shelves), then name.
    Ext,
}

/// A volume's presentation form (a `z` stack only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Form {
    /// The rolodex: the head fronts at z = 0, the rest recede by `gap`.
    Deck,
    /// The book laid open: every page in an m×n grid, the head lifted.
    Splay,
}

/// Every dial the scheme reads (`[library]` in config/defaults.toml).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opts {
    pub page_w: f64,
    pub page_h: f64,
    /// The FULL z step of a deck; the gap added between shelf/pile pages.
    pub gap: f64,
    pub stack: Stack,
    pub sort: Sort,
    pub reverse: bool,
    pub max_upscale: f64,
    pub splay_cols: u32,
    pub splay_gap_x: f64,
    pub splay_gap_y: f64,
    pub splay_aspect: f64,
    pub splay_lift: f64,
    /// Gap between a node's own stack and its child-directory tier.
    pub dir_gap: f64,
    /// Per-level z step back for child directories.
    pub depth_z: f64,
    /// Child-tier wrap target (w ≈ aspect × h).
    pub aspect: f64,
    /// Content front (true) or depth centre (false, the JS) on the page plane.
    pub depth_front: bool,
}

impl Opts {
    pub fn splay(&self) -> SplayDims {
        SplayDims {
            cols: self.splay_cols,
            aspect: self.splay_aspect,
            page_w: self.page_w,
            page_h: self.page_h,
            gap_x: self.splay_gap_x,
            gap_y: self.splay_gap_y,
        }
    }
}

/// One file as the scheme sees it: a name to sort by and its natural
/// (released) content box in its own local frame.
#[derive(Clone, Debug)]
pub struct LibFile {
    pub rel_path: String,
    pub ink_min: [f32; 3],
    pub ink_max: [f32; 3],
}

impl LibFile {
    fn name(&self) -> &str {
        self.rel_path.rsplit('/').next().unwrap_or(&self.rel_path)
    }

    /// `extOf`: the lower-cased extension of the base name, '' for none or
    /// a leading dot only.
    fn ext(&self) -> String {
        let base = self.name();
        match base.rfind('.') {
            Some(dot) if dot > 0 => base[dot + 1..].to_lowercase(),
            _ => String::new(),
        }
    }

    fn area(&self) -> f64 {
        let w = (f64::from(self.ink_max[0]) - f64::from(self.ink_min[0])).max(0.0);
        let h = (f64::from(self.ink_max[1]) - f64::from(self.ink_min[1])).max(0.0);
        w * h
    }
}

/// A directory node: its files (indices into the file list) and child
/// directories, both in the deterministic sibling order.
#[derive(Clone, Debug, Default)]
pub struct DirNode {
    /// Full relative path ('' for the root) — the key a volume's state and
    /// the `dir:<path>` zone are filed under.
    pub path: String,
    /// Display name; a compressed chain carries the joined segments.
    pub name: String,
    pub files: Vec<usize>,
    pub dirs: Vec<DirNode>,
}

/// Case-insensitive name order with a raw-bytes tie-break (the JS's
/// `localeCompare(…, { sensitivity: 'base' })` for the ASCII names a repo
/// carries; the tie-break only makes the order total).
fn by_name(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

/// The directory tree of `files` (paths split on '/'), siblings sorted
/// (names case-insensitively; dirs and files are separate lists here, so
/// the JS's dirs-first rule is structural) and every single-child chain
/// below the root compressed to its tail, as `partitionChildren` does.
pub fn build_tree(files: &[LibFile]) -> DirNode {
    let mut root = DirNode::default();
    for (i, f) in files.iter().enumerate() {
        let mut node = &mut root;
        let segs: Vec<&str> = f.rel_path.split('/').collect();
        let mut path = String::new();
        for seg in &segs[..segs.len().saturating_sub(1)] {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(seg);
            let pos = match node.dirs.iter().position(|d| d.name == *seg) {
                Some(p) => p,
                None => {
                    node.dirs.push(DirNode { path: path.clone(), name: seg.to_string(), ..Default::default() });
                    node.dirs.len() - 1
                }
            };
            node = &mut node.dirs[pos];
        }
        node.files.push(i);
    }
    fn finish(node: &mut DirNode, files: &[LibFile]) {
        node.files.sort_by(|&a, &b| by_name(files[a].name(), files[b].name()));
        node.dirs.sort_by(|a, b| by_name(&a.name, &b.name));
        let dirs = std::mem::take(&mut node.dirs);
        node.dirs = dirs.into_iter().map(collapse_chain).collect();
        for d in &mut node.dirs {
            finish(d, files);
        }
    }
    finish(&mut root, files);
    root
}

/// A directory holding exactly one directory and no files is a pass-through:
/// its tail is laid out in its place, under the joined name.
fn collapse_chain(mut dir: DirNode) -> DirNode {
    let mut names = vec![dir.name.clone()];
    while dir.files.is_empty() && dir.dirs.len() == 1 {
        dir = dir.dirs.pop().expect("one child");
        names.push(dir.name.clone());
    }
    dir.name = names.join("/");
    dir
}

/// A volume's navigation state, kept across relayouts by directory path
/// (the JS kept it on the dir node: `volumeHead`, `volumeForm`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeState {
    pub head: usize,
    pub form: Form,
}

/// One directory, placed.
#[derive(Clone, Debug)]
pub struct DirPlan {
    pub path: String,
    pub name: String,
    /// Index of the parent in `Plan::dirs` (None for the root).
    pub parent: Option<usize>,
    /// Position in the parent's frame (the root sits at the origin).
    pub pos: [f64; 3],
    /// Measured footprint (w, h, d).
    pub size: [f64; 3],
    /// The files, in stack order (sorted).
    pub books: Vec<usize>,
    /// The volume's position in this node's frame (meaningful when `books`
    /// is non-empty).
    pub volume_pos: [f64; 3],
    pub form: Form,
    pub head: usize,
}

/// One file, placed.
#[derive(Clone, Copy, Debug)]
pub struct FilePlan {
    /// Index of its directory in `Plan::dirs`.
    pub dir: usize,
    /// Its place in the directory's stack order.
    pub order: usize,
    /// The sheet's slot in the volume's frame.
    pub slot: [f64; 3],
    pub fit: Fit,
}

/// The whole library, placed: dirs in pre-order (indices stable for a given
/// file set, whatever the dials), files by their index in the load.
#[derive(Clone, Debug)]
pub struct Plan {
    pub dirs: Vec<DirPlan>,
    pub files: Vec<FilePlan>,
}

/// Order the books by the active question; sorts fall back to name.
pub fn sort_books(books: &mut [usize], files: &[LibFile], sort: Sort, reverse: bool) {
    match sort {
        Sort::Name => books.sort_by(|&a, &b| by_name(files[a].name(), files[b].name())),
        Sort::Size => books.sort_by(|&a, &b| {
            files[b]
                .area()
                .partial_cmp(&files[a].area())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| by_name(files[a].name(), files[b].name()))
        }),
        Sort::Ext => books.sort_by(|&a, &b| {
            files[a].ext().cmp(&files[b].ext()).then_with(|| by_name(files[a].name(), files[b].name()))
        }),
    }
    if reverse {
        books.reverse();
    }
}

/// The stack's bounding extent (w, h, d): pages are uniform, so this is pure
/// arithmetic. The deck overlaps on z (gap IS the step); shelf and pile step
/// a full page plus the gap; a splayed volume spans its grid.
pub fn stack_extent(n: usize, o: &Opts, form: Form) -> [f64; 3] {
    if n == 0 {
        return [0.0; 3];
    }
    let nf = n as f64;
    match o.stack {
        Stack::X => [nf * o.page_w + (nf - 1.0) * o.gap, o.page_h, 0.0],
        Stack::Y => [o.page_w, nf * o.page_h + (nf - 1.0) * o.gap, 0.0],
        Stack::Z => match form {
            Form::Splay => {
                let g = splay_grid(n, &o.splay());
                [g.w, g.h, o.splay_lift]
            }
            Form::Deck => [o.page_w, o.page_h, (nf - 1.0) * o.gap],
        },
    }
}

/// A packed box's top-left slot (y descends) and its row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlowSlot {
    pub x: f64,
    pub y: f64,
    pub row: usize,
}

/// `flowBoxes`: pack sized boxes into a wrapping shelf, top-aligned per row;
/// with `serpentine`, odd rows run right to left (mirrored across the
/// cluster width) so consecutive boxes stay adjacent across a row break.
/// Returns the slots and the cluster's (width, height).
pub fn flow_boxes(sizes: &[[f64; 2]], margin: f64, wrap_width: f64, serpentine: bool) -> (Vec<FlowSlot>, f64, f64) {
    let mut slots = Vec::with_capacity(sizes.len());
    let (mut cx, mut top_y, mut row_h, mut max_w, mut row) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0usize);
    for s in sizes {
        if cx > 0.0 && cx + s[0] > wrap_width {
            max_w = max_w.max(cx - margin);
            cx = 0.0;
            top_y -= row_h + margin;
            row_h = 0.0;
            row += 1;
        }
        slots.push(FlowSlot { x: cx, y: top_y, row });
        cx += s[0] + margin;
        row_h = row_h.max(s[1]);
    }
    max_w = max_w.max(cx - margin);
    if serpentine {
        for (slot, s) in slots.iter_mut().zip(sizes) {
            if slot.row & 1 == 1 {
                slot.x = max_w - slot.x - s[0];
            }
        }
    }
    (slots, max_w, -top_y + row_h)
}

/// The child-directory tier: canonical order, serpentine, wrap width from
/// the total area at the aspect target (`orderedPack`).
pub fn ordered_pack(sizes: &[[f64; 2]], gap: f64, aspect: f64) -> (Vec<FlowSlot>, f64, f64) {
    let (mut area, mut max_w) = (0.0f64, 0.0f64);
    for s in sizes {
        area += (s[0] + gap) * (s[1] + gap);
        max_w = max_w.max(s[0]);
    }
    let wrap = if sizes.len() <= 1 { f64::INFINITY } else { (area * aspect).sqrt().max(max_w + gap) };
    flow_boxes(sizes, gap, wrap, true)
}

/// Where sheet `i` of `n` rests in its volume's frame, by the stack's law.
pub fn slot_for(i: usize, n: usize, head: usize, form: Form, o: &Opts) -> [f64; 3] {
    match o.stack {
        Stack::X => {
            let w = stack_extent(n, o, form)[0];
            [-w / 2.0 + o.page_w / 2.0 + i as f64 * (o.page_w + o.gap), 0.0, 0.0]
        }
        Stack::Y => [0.0, -(i as f64) * (o.page_h + o.gap), 0.0],
        Stack::Z => match form {
            Form::Deck => deck_slot(i, head, n, 1, o.gap),
            Form::Splay => splay_slot(i, head, n, &o.splay(), o.splay_lift),
        },
    }
}

/// Measure post-order, place pre-order. `volumes` carries each directory's
/// head and form by path (a directory not in it opens at page 0 in
/// `default_form`); a head past the end is clamped.
pub fn plan(tree: &DirNode, files: &[LibFile], o: &Opts, volumes: &HashMap<String, VolumeState>, default_form: Form) -> Plan {
    struct Measured {
        books: Vec<usize>,
        form: Form,
        head: usize,
        ext: [f64; 3],
        size: [f64; 3],
        kids: Vec<Measured>,
        pack: Option<(Vec<FlowSlot>, f64, f64)>,
    }
    fn measure(node: &DirNode, files: &[LibFile], o: &Opts, volumes: &HashMap<String, VolumeState>, default_form: Form) -> Measured {
        let mut books = node.files.clone();
        sort_books(&mut books, files, o.sort, o.reverse);
        let st = volumes.get(&node.path).copied().unwrap_or(VolumeState { head: 0, form: default_form });
        let head = st.head.min(books.len().saturating_sub(1));
        let ext = stack_extent(books.len(), o, st.form);
        let kids: Vec<Measured> = node.dirs.iter().map(|d| measure(d, files, o, volumes, default_form)).collect();
        let sizes: Vec<[f64; 2]> = kids.iter().map(|k| [k.size[0], k.size[1]]).collect();
        let pack = (!kids.is_empty()).then(|| ordered_pack(&sizes, o.dir_gap, o.aspect));
        let empty = books.is_empty() && pack.is_none();
        let (pw, ph) = pack.as_ref().map_or((0.0, 0.0), |p| (p.1, p.2));
        let w = if empty { 0.0 } else { ext[0].max(pw) };
        let h = if empty {
            0.0
        } else {
            ext[1] + if !books.is_empty() && pack.is_some() { o.dir_gap } else { 0.0 } + ph
        };
        let deepest = kids.iter().map(|k| k.size[2]).fold(f64::NEG_INFINITY, f64::max);
        let d = ext[2].max(if kids.is_empty() { 0.0 } else { o.depth_z + deepest });
        Measured { books, form: st.form, head, ext, size: [w, h, d], kids, pack }
    }
    struct Placer<'a> {
        o: &'a Opts,
        files: &'a [LibFile],
        dirs: Vec<DirPlan>,
        out: Vec<Option<FilePlan>>,
    }
    impl Placer<'_> {
        fn place(&mut self, node: &DirNode, m: &Measured, parent: Option<usize>, pos: [f64; 3]) {
            let o = self.o;
            let me = self.dirs.len();
            self.dirs.push(DirPlan {
                path: node.path.clone(),
                name: node.name.clone(),
                parent,
                pos,
                size: m.size,
                books: m.books.clone(),
                volume_pos: [0.0, -o.page_h / 2.0, 0.0],
                form: m.form,
                head: m.head,
            });
            let n = m.books.len();
            for (i, &f) in m.books.iter().enumerate() {
                let file = &self.files[f];
                let fit = contain_fit(file.ink_min, file.ink_max, o.page_w, o.page_h, o.max_upscale, o.depth_front);
                self.out[f] = Some(FilePlan { dir: me, order: i, slot: slot_for(i, n, m.head, m.form, o), fit });
            }
            let Some((slots, pack_w, _)) = &m.pack else { return };
            let c_left = -pack_w / 2.0;
            let c_top = -(if n > 0 { m.ext[1] + o.dir_gap } else { 0.0 });
            for (j, (child, km)) in node.dirs.iter().zip(&m.kids).enumerate() {
                let s = slots[j];
                let cpos = [c_left + s.x + km.size[0] / 2.0, c_top + s.y, -o.depth_z];
                self.place(child, km, Some(me), cpos);
            }
        }
    }
    let m = measure(tree, files, o, volumes, default_form);
    let mut placer = Placer { o, files, dirs: Vec::new(), out: vec![None; files.len()] };
    placer.place(tree, &m, None, [0.0; 3]);
    Plan {
        dirs: placer.dirs,
        files: placer.out.into_iter().map(|f| f.expect("every file sits in exactly one directory")).collect(),
    }
}
