//! The library's page faces, drawn from the transform tree (2026-10-10).
//!
//! One quad behind every sheet (`[library] page_faces`), drawn by the scene
//! mesh pipeline (`mesh.wgsl`, unchanged) from an instance buffer the host
//! keeps current by sheet: after the frame's resolve, each face whose sheet
//! the resolve reached is rebuilt from the sheet's world (`face_instance`)
//! in the host's image of the buffer, and only the faces whose bytes
//! changed go up, coalesced into runs (`nodes::upload_runs`). A page turn
//! costs the faces of the turned volume, a directory drag the faces under
//! it, and a settled frame nothing; before the library moved onto the tree,
//! every face was a bevy scene mesh and any moved sheet re-extracted and
//! re-uploaded all of them (2.35 MB at 29k files; findings E5, E16). From
//! 2026-10-10 to 2026-10-26 a compute pass (`library_faces.wgsl`, retired)
//! wrote every face on the GPU after any resolve; `face_instance` was its
//! twin and is now the path, held to bevy's extraction in `library::tests`.

use std::cell::{Cell, Ref, RefCell};
use std::ops::Range;

use bytemuck::Zeroable;
use glyph_scene_graph::{FramePlan, NodeTables, NONE};

use super::mesh::MeshInstance;
use super::nodes::{run_rows, upload_runs, write_runs, UploadCost};
use crate::library::FaceSpec;

/// The face of a sheet whose world is `sheet`: bevy's extraction of the
/// face as it was spawned — a child of the sheet at (0, 0, -gap), drawing a
/// unit quad scaled to the page and centred on `origin + size / 2` —
/// written as the similarity it is: the translation is the sheet's plus the
/// offset carried through the sheet's rotation and scale, the matrix the
/// sheet's rotation (glam's `Mat3::from_quat`) times its scale times the
/// page size. For the library's sheets, which never rotate, every product
/// is with 1 or 0 and every sum is with 0 except the offset's, so the
/// instance comes out bit for bit as bevy's extraction made it.
pub(crate) fn face_instance(sheet: &glyph_scene_graph::Similarity, spec: &FaceSpec) -> MeshInstance {
    let s = sheet.scale;
    let q = sheet.rotation;
    let off = [0.0, 0.0, -spec.gap];
    let r = glyph_scene_graph::transform::rotate(q, [off[0] * s, off[1] * s, off[2] * s]);
    let t = [sheet.translation[0] + r[0], sheet.translation[1] + r[1], sheet.translation[2] + r[2]];
    let (x2, y2, z2) = (q[0] + q[0], q[1] + q[1], q[2] + q[2]);
    let (xx, xy, xz) = (q[0] * x2, q[0] * y2, q[0] * z2);
    let (yy, yz, zz) = (q[1] * y2, q[1] * z2, q[2] * z2);
    let (wx, wy, wz) = (q[3] * x2, q[3] * y2, q[3] * z2);
    let mx = [(1.0 - (yy + zz)) * s, (xy + wz) * s, (xz - wy) * s];
    let my = [(xy - wz) * s, (1.0 - (xx + zz)) * s, (yz + wx) * s];
    let mz = [(xz + wy) * s, (yz - wx) * s, (1.0 - (xx + yy)) * s];
    // bevy's extraction skipped a face whose world had (almost) no scale; a
    // zero instance draws nothing.
    let dot = |v: [f32; 3]| v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if dot(mx) + dot(my) + dot(mz) < 1e-6 {
        return MeshInstance::zeroed();
    }
    let (size, origin) = (spec.size, [-spec.size[0] / 2.0, -spec.size[1] / 2.0]);
    let centre = [origin[0] + size[0] * 0.5, origin[1] + size[1] * 0.5];
    let at = [0, 1, 2].map(|k| t[k] + (mx[k] * centre[0] + my[k] * centre[1]));
    MeshInstance {
        world_col0: [mx[0] * size[0], mx[1] * size[0], mx[2] * size[0], at[0]],
        world_col1: [my[0] * size[1], my[1] * size[1], my[2] * size[1], at[1]],
        world_col2: [mz[0], mz[1], mz[2], at[2]],
        color: spec.color,
        material_params: [0.0; 4],
    }
}

pub(crate) struct LibraryFaces {
    spec: FaceSpec,
    /// Face → its sheet's pool slot (draw order), and slot → face.
    sheets: Vec<u32>,
    face_of_slot: Vec<u32>,
    /// The instances mesh.wgsl draws, as the host last built them (the GPU
    /// buffer, [`LibraryFaces::buffer`], starts zeroed as wgpu creates it,
    /// and so does this).
    image: RefCell<Vec<MeshInstance>>,
    last: Cell<UploadCost>,
    /// Bumped by every `prepare` that changed a face (`FrameData::faces_generation`).
    generation: Cell<u64>,
}

impl LibraryFaces {
    /// One face per entry of `sheet_slots` (each a sheet node's pool slot),
    /// in draw order. The first frame's resolve reaches every sheet, so it
    /// writes every face.
    pub(crate) fn new(spec: &FaceSpec, sheet_slots: &[u32]) -> Self {
        let count = sheet_slots.len();
        let slots = sheet_slots.iter().map(|&s| s as usize + 1).max().unwrap_or(0);
        let mut face_of_slot = vec![NONE; slots];
        for (k, &s) in sheet_slots.iter().enumerate() {
            face_of_slot[s as usize] = k as u32;
        }
        Self {
            spec: *spec,
            sheets: sheet_slots.to_vec(),
            face_of_slot,
            image: RefCell::new(vec![MeshInstance::zeroed(); count]),
            last: Cell::new(UploadCost::default()),
            generation: Cell::new(0),
        }
    }

    pub(crate) fn count(&self) -> u32 {
        self.sheets.len() as u32
    }

    /// The GPU instance buffer the faces are drawn from, zeroed, sized for
    /// every face (the renderer's; `upload` writes it).
    pub(crate) fn buffer(&self, device: &wgpu::Device) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("library face instances"),
            size: (self.sheets.len().max(1) * std::mem::size_of::<MeshInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    }

    /// The CPU half, after the frame's resolve: rebuild the face of every
    /// sheet the resolve reached and plan the runs the changed faces go up
    /// in (bumping the generation when there are any). Nothing resolved,
    /// nothing done.
    pub(crate) fn prepare(&self, t: &NodeTables, plan: &FramePlan) -> Vec<Range<u32>> {
        if plan.is_empty() || self.sheets.is_empty() {
            return Vec::new();
        }
        let t0 = std::time::Instant::now();
        let mut image = self.image.borrow_mut();
        let mut changed = Vec::new();
        for slot in plan.slots(t) {
            let Some(&k) = self.face_of_slot.get(slot as usize) else { continue };
            if k == NONE {
                continue;
            }
            let face = face_instance(&t.resolved_world(slot).0, &self.spec);
            if bytemuck::bytes_of(&face) != bytemuck::bytes_of(&image[k as usize]) {
                image[k as usize] = face;
                changed.push(k);
            }
        }
        changed.sort_unstable();
        let runs = upload_runs(&changed, image.len() as u32);
        if !runs.is_empty() {
            self.generation.set(self.generation.get() + 1);
        }
        let bytes = run_rows(&runs) * std::mem::size_of::<MeshInstance>();
        self.last.set(UploadCost { rows: changed.len(), bytes, ms: t0.elapsed().as_secs_f64() * 1e3 });
        runs
    }

    /// The wgpu half: write `runs` (`prepare`'s) into the instance buffer.
    pub(crate) fn upload(&self, queue: &wgpu::Queue, buffer: &wgpu::Buffer, runs: &[Range<u32>]) {
        if runs.is_empty() {
            return;
        }
        let t0 = std::time::Instant::now();
        write_runs(queue, buffer, bytemuck::cast_slice(&self.image.borrow()), std::mem::size_of::<MeshInstance>(), runs);
        let mut cost = self.last.get();
        cost.ms += t0.elapsed().as_secs_f64() * 1e3;
        self.last.set(cost);
    }

    /// Every face as the host last built it (what the GPU holds once the
    /// frame's runs are written).
    pub(crate) fn image(&self) -> Ref<'_, Vec<MeshInstance>> {
        self.image.borrow()
    }

    /// The face generation: bumped whenever a `prepare` planned any run.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// The last frame's face upload that did anything (the instruments).
    pub(crate) fn last_update(&self) -> UploadCost {
        self.last.get()
    }
}

#[cfg(test)]
mod tests {
    use glyph_scene_graph::{NodeHandle, Similarity};

    use super::{face_instance, LibraryFaces, MeshInstance};
    use crate::glyph_scene::nodes::{read_buffer, GroupNodes};
    use crate::glyph_scene::GroupRow;
    use crate::library::plan::LibFile;
    use crate::library::{Form, FormCmd, Library, Page};

    /// What reaches the GPU (2026-10-26, the twins as the path): after
    /// each frame's flush and face update, the group table and the face
    /// instances read back from the device are the CPU rows bit for bit —
    /// `seated_rows` (held to bevy's composition by
    /// `library::tests::the_tree_resolves_the_rows_bevy_composed`) and
    /// `face_instance` of every sheet's chain-walked world — seated, mid
    /// form change, after a drag and mid page turn; and a settled frame
    /// uploads nothing. A row or face the dirty tracking missed reads back
    /// stale.
    #[test]
    fn the_gpu_tables_hold_the_rows_and_faces_the_tree_composes() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let s = &crate::config::settings().library;
        let file = |p: &str, w: f32, h: f32| LibFile { rel_path: p.to_string(), ink_min: [0.0, -h, -3.0], ink_max: [w, 0.0, 0.0] };
        let files = vec![
            file("a.rs", 41.3, 17.9),
            file("src/b.rs", 512.7, 1033.1),
            file("src/c.rs", 3.1, 1.25),
            file("src/deep/x/y.py", 97.0, 211.6),
            file("src/deep/z.md", 13.37, 900.0),
            file("docs/readme.md", 77.7, 66.6),
        ];
        let tints: Vec<[f32; 3]> = (0..files.len()).map(|i| [0.15 * i as f32, 0.5, 0.9]).collect();
        let mut lib = Library::new(files.clone(), s);
        let n = files.len();
        let table = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test group table"),
            size: (n * std::mem::size_of::<GroupRow>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let start: Vec<GroupRow> = tints.iter().map(|t| GroupRow::tinted([0.0; 3], *t)).collect();
        ctx.queue.write_buffer(&table, 0, bytemuck::cast_slice(&start));
        let mut g = GroupNodes::new(&start);
        let file_nodes: Vec<NodeHandle> = (0..n as u32).map(|i| g.node(i).unwrap()).collect();
        let root = g.root();
        lib.attach(g.tables_mut(), Some(root), &file_nodes);
        let spec = lib.face.expect("the default settings draw faces");
        let nodes = lib.nodes.clone().unwrap();
        let slots: Vec<u32> = nodes.sheet.iter().map(|h| h.index()).collect();
        let faces = LibraryFaces::new(&spec, &slots);
        let face_buf = faces.buffer(&ctx.device);

        let frame = |g: &GroupNodes| {
            let (plan, rows) = g.prepare();
            g.upload(&ctx.queue, &table, &rows);
            let runs = faces.prepare(&g.tables(), &plan);
            faces.upload(&ctx.queue, &face_buf, &runs);
            (g.last_flush().0.bytes, faces.last_update().bytes * usize::from(!runs.is_empty()))
        };
        let check = |g: &GroupNodes, what: &str| {
            let (rows_up, faces_up) = frame(g);
            assert!(rows_up > 0 && faces_up > 0, "{what}: something moved ({rows_up} B of rows, {faces_up} B of faces)");
            let rows: Vec<GroupRow> = bytemuck::cast_slice(&read_buffer(&ctx, &table, (n * 96) as u64)).to_vec();
            let want = crate::library::seated_rows(&g.tables(), &nodes, &tints);
            for (i, (a, b)) in rows.iter().zip(&want).enumerate() {
                let (a, b): (&[u32; 24], &[u32; 24]) = (bytemuck::cast_ref(&a.cols), bytemuck::cast_ref(&b.cols));
                assert_eq!(a[..16], b[..16], "{what}: file {i}'s group row on the GPU");
            }
            let got: Vec<MeshInstance> = bytemuck::cast_slice(&read_buffer(&ctx, &face_buf, (n * 80) as u64)).to_vec();
            for (i, (a, &sheet)) in got.iter().zip(&nodes.sheet).enumerate() {
                let b = face_instance(&g.tables().world(sheet).unwrap().0, &spec);
                assert_eq!(bytemuck::bytes_of(a), bytemuck::bytes_of(&b), "{what}: face {i} on the GPU {a:?}, composed {b:?}");
            }
        };
        check(&g, "seated");
        assert_eq!(frame(&g), (0, 0), "a settled frame uploads nothing");
        lib.form(&g.tables(), None, FormCmd::Set(Form::Splay));
        for _ in 0..7 {
            lib.tick(g.tables_mut(), 1.0 / 60.0).expect("animating");
        }
        check(&g, "mid splay");
        let zone = lib.zone_node("dir:src/deep").unwrap();
        let l = g.tables().local(zone).unwrap();
        g.tables_mut().set_local(zone, Similarity { translation: [l.translation[0] + 13.25, l.translation[1] - 7.5, l.translation[2]], ..l }).unwrap();
        check(&g, "dragged");
        lib.page(&g.tables(), None, Page::Next);
        for _ in 0..5 {
            lib.tick(g.tables_mut(), 1.0 / 60.0);
        }
        check(&g, "mid page turn");
    }
}
