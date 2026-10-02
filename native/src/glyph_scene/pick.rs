//! Stage G — CPU picking + live instance/group manipulation: the pick
//! types, the one-entry re-derived-file cache, ray/hit resolution and the
//! verb write-back path. Extracted from `glyph_scene.rs` in the 2026-09
//! code-shape refactor — a pure move; `pub(super)` stands in for the
//! same-module privacy these items had. The pick contract itself (why
//! CPU-side, the resolution order) lives in the `glyph_scene.rs` header.

use glam::DVec3;
use std::path::PathBuf;

use super::{CameraMode, GlyphScene, GroupRow, RenderSlot, Selection, FOV_Y};
use crate::gpu::GpuContext;
use crate::layout::{GlyphRecord, ItemParams};

// ── Stage G — picking & manipulation types ─────────────────────────────────

/// Per-file pick record: everything needed to (a) hit-test the file's AABB
/// under the LIVE group TRS and (b) re-derive its glyph geometry via a
/// deterministic engine re-run (`repo::rederive_records` with `item`).
pub struct PickFileInfo {
    pub rel_path: String,
    pub group_id: u32,
    pub slot_base: u32,
    pub slot_count: u32,
    /// The exact engine params this file was laid out with.
    pub item: ItemParams,
    /// Local-space (pre-TRS) 3D AABB, same margins as the cull segment.
    pub aabb_min: [f32; 3],
    pub aabb_max: [f32; 3],
}

/// Repo-mode pick context (one per staged scene).
pub struct PickContext {
    pub root: PathBuf,
    pub trie: PathBuf,
    pub files: Vec<PickFileInfo>,
    pub content: Option<std::collections::HashMap<String, std::sync::Arc<Vec<u8>>>>,
    pub folds: std::collections::HashMap<String, Vec<std::ops::Range<u32>>>,
}

/// A pick request — scripted (CLI) or interactive (click).
#[derive(Clone)]
pub enum PickCommand {
    /// Group-level pick: first file whose rel path contains the substring.
    File(String),
    /// Deterministic glyph pick: exact folded (row, col) within that file.
    RowCol { file: String, row: u32, col: u32 },
    /// Ray pick through a physical pixel of the current viewport.
    Pixel { x: f32, y: f32 },
}

/// A manipulation verb, applied to the current pick. Instance verbs need a
/// glyph pick; group verbs need at least a file pick.
#[derive(Clone)]
pub enum Verb {
    /// Recolor the picked glyph (packed sRGB rgb, alpha kept 255).
    RecolorGlyph([u8; 3]),
    /// Recolor every glyph on the picked glyph's folded row ("highlight line").
    RecolorLine([u8; 3]),
    /// Offset the picked glyph's local position (plumbing demo).
    NudgeGlyph([f32; 3]),
    /// Scale the picked glyph's quad (advance & height; plumbing demo).
    ScaleGlyph(f32),
    /// Move the picked file's group offset (world units).
    MoveGroup([f32; 3]),
    /// Multiply the picked file's group scale (uniform xyz).
    ScaleGroup(f32),
    /// Set the picked file's group color tint (sRGB floats).
    TintGroup([f32; 3]),
    /// Cycle the picked file through the per-directory tint palette.
    TintCycle,
    SetHidden(bool),
    ToggleHidden,
}

/// A resolved glyph within a file.
#[derive(Clone)]
pub struct PickGlyph {
    /// Record index within the file (== UTF-8 leader index).
    pub record: usize,
    /// Arena slot (global), None for blank/missing records (no instance).
    pub slot: Option<u32>,
    pub row: u32,
    pub col: u32,
    /// Source line (0-based) — differs from `row` under wrap/pagination.
    pub line: u32,
    pub byte_off: usize,
    pub ch: char,
    /// Local (pre-TRS) layout position/metrics — the verb write-back basis.
    pub pos: [f32; 3],
    pub advance: f32,
    pub height: f32,
}

/// A resolved pick: always the file; the glyph when one is close enough.
#[derive(Clone)]
pub struct PickHit {
    pub group_id: u32,
    pub rel_path: String,
    pub glyph: Option<PickGlyph>,
}

/// Per-file pick cache: the re-derived records plus the CPU-side walks over
/// the file bytes, all indexed by record (== leader) index.
pub(super) struct PickCacheEntry {
    group_id: u32,
    records: Vec<GlyphRecord>,
    /// (byte offset, codepoint) per record.
    leaders: Vec<(usize, u32)>,
    /// Source line per record.
    lines: Vec<u32>,
    /// Global arena slot per record; u32::MAX for blank/missing (no instance).
    slot_of: Vec<u32>,
}

impl GlyphScene {
    /// Ensure the one-entry pick cache holds `gid`'s re-derived file data:
    /// records (bit-identical engine re-run) + the CPU byte walks. The fold
    /// cross-check (CPU row/col vs engine ROW/COL lanes, every record) is the
    /// standing pick-correctness gate and runs on every cache fill.
    fn ensure_pick_cache(&mut self, gid: u32) -> bool {
        if self.cache.as_ref().is_some_and(|c| c.group_id == gid) {
            return true;
        }
        let Some(pctx) = &self.pick else { return false };
        let Some(info) = pctx.files.iter().find(|f| f.group_id == gid) else {
            return false;
        };
        let t = std::time::Instant::now();
        let bytes: std::sync::Arc<Vec<u8>> = if let Some(owned) = pctx
            .content
            .as_ref()
            .and_then(|m| m.get(&info.rel_path))
            .cloned()
        {
            owned
        } else {
            match std::fs::read(pctx.root.join(&info.rel_path)) {
                Ok(bytes) => std::sync::Arc::new(bytes),
                Err(_) => {
                    log::warn!("pick: failed to read {}", info.rel_path);
                    return false;
                }
            }
        };
        let Ok(records) = crate::repo::rederive_cached(&pctx.trie, &bytes, &info.item) else {
            log::warn!("pick: failed to re-run {}", info.rel_path);
            return false;
        };
        let (leaders, rows, cols, lines) =
            crate::text::fold_leaders(&bytes, info.item.wrap_width, info.item.wrap_mode);
        let mut mismatch = 0usize;
        if leaders.len() != records.len() {
            mismatch += 1;
        } else {
            for (i, r) in records.iter().enumerate() {
                if r.row() != rows[i] || r.col() != cols[i] {
                    mismatch += 1;
                }
            }
        }
        let mut slot_of = vec![u32::MAX; records.len()];
        let mut k = info.slot_base;
        for (i, r) in records.iter().enumerate() {
            if r.glyph_id() != 0 {
                slot_of[i] = k;
                k += 1;
            }
        }
        debug_assert_eq!(k, info.slot_base + info.slot_count);
        log::info!(
            "pick cache: {} — {} records re-derived in {:.1?} ({} B) | fold cross-check: {} ({} mismatch)",
            info.rel_path,
            records.len(),
            t.elapsed(),
            bytes.len(),
            if mismatch == 0 { "PASS" } else { "FAIL" },
            mismatch,
        );
        if mismatch != 0 {
            log::warn!("pick: fold/engine row-col mismatch on {} — char resolution unreliable", info.rel_path);
        }
        self.cache = Some(PickCacheEntry {
            group_id: gid,
            records,
            leaders,
            lines,
            slot_of,
        });
        true
    }

    /// Build a PickGlyph for record `rec` of the cached file `gid`.
    fn make_glyph(&self, gid: u32, rec: usize) -> Option<PickGlyph> {
        let c = self.cache.as_ref().filter(|c| c.group_id == gid)?;
        let r = c.records.get(rec)?;
        let (byte_off, cp) = c.leaders[rec];
        Some(PickGlyph {
            record: rec,
            slot: (c.slot_of[rec] != u32::MAX).then_some(c.slot_of[rec]),
            row: r.row(),
            col: r.col(),
            line: c.lines[rec],
            byte_off,
            ch: char::from_u32(cp).unwrap_or('\u{FFFD}'),
            pos: [r.x(), r.y(), r.z()],
            advance: r.advance(),
            height: r.height(),
        })
    }

    /// Unproject a physical pixel to a world ray under the CURRENT camera.
    ///
    /// ANALYTIC, in f64 — deliberately NOT the inverse of the f32 view-proj.
    /// The Fly projection spans near=0.05 … far=(fit·50).max(20000); at the
    /// full-field fit (far≈1.7e6) the near/far ratio is past f32 epsilon, so
    /// the inverted matrix unprojects the far-plane point to w≈0 and EVERY
    /// pick returned None (windowed clicks always MISSed); even at
    /// far=20000 the inverse's angular error grows linearly with distance
    /// and crosses the 0.8-world-unit acceptance at D≈50 (measured; see
    /// tools/repro_pick_oblique.py). The ray is derived exactly from the
    /// same eye/target the view matrix is built from — right/up/back basis +
    /// fov/aspect — so there is no matrix to invert and no near/far
    /// conditioning at all. (The GPU's forward f32 projection of a world
    /// point differs from this ray by ≲1e-4 px — subpixel.)
    pub(super) fn pixel_ray(&self, x: f32, y: f32) -> Option<(DVec3, DVec3)> {
        let (w, h) = self.viewport.get();
        if w == 0 || h == 0 {
            return None;
        }
        let aspect = w as f64 / h as f64;
        let (eye, target) = self.camera_eye_target(0.0, (w as f32) / (h as f32));
        // Forward direction WITHOUT the big-coordinate f32 roundtrip: for
        // Fly, `target` was formed as eye+fwd in f32, so eye−target loses
        // ~1e-3 rad to cancellation at field-scale coordinates — use the
        // camera's own forward (the same one camera_frame's look_to uses).
        let fwd = match self.camera_mode {
            CameraMode::Fly => self.fly.forward(),
            _ => (target - eye).normalize(),
        };
        let eye = eye.as_dvec3();
        let back = -fwd.as_dvec3().normalize(); // view z axis (backward)
        if back.length_squared() < 1e-24 {
            return None;
        }
        let right = DVec3::Y.cross(back).normalize(); // view x axis
        let up = back.cross(right); // view y axis
        let tan = (FOV_Y as f64 * 0.5).to_radians().tan();
        let nx = (x as f64 / w as f64) * 2.0 - 1.0;
        let ny = 1.0 - (y as f64 / h as f64) * 2.0;
        // View-space ray (nx·tan·aspect, ny·tan, −1) rotated to world.
        let dir = (right * (nx * tan * aspect) + up * (ny * tan) - back).normalize();
        Some((eye, dir))
    }

    /// Nearest non-hidden file whose live world AABB the ray pierces.
    fn ray_file(&self, ro: DVec3, rd: DVec3) -> Option<(u32, f64)> {
        let pctx = self.pick.as_ref()?;
        let mut best: Option<(u32, f64)> = None;
        for info in &pctx.files {
            if self.group_hidden(info.group_id) {
                continue;
            }
            let Some(affine) = self.group_affine(info.group_id) else {
                continue;
            };
            let inv_affine = affine.inverse();

            let ro_f32 = glam::Vec3::new(ro.x as f32, ro.y as f32, ro.z as f32);
            let rd_f32 = glam::Vec3::new(rd.x as f32, rd.y as f32, rd.z as f32);
            let ro_local = inv_affine.transform_point3(ro_f32);
            let rd_local = inv_affine.transform_vector3(rd_f32);
            let ro_loc_d = DVec3::new(ro_local.x as f64, ro_local.y as f64, ro_local.z as f64);
            let rd_loc_d = DVec3::new(rd_local.x as f64, rd_local.y as f64, rd_local.z as f64);

            let b_min = DVec3::new(
                info.aabb_min[0] as f64,
                info.aabb_min[1] as f64,
                info.aabb_min[2] as f64,
            );
            let b_max = DVec3::new(
                info.aabb_max[0] as f64,
                info.aabb_max[1] as f64,
                info.aabb_max[2] as f64,
            );
            let min = b_min.min(b_max);
            let max = b_min.max(b_max);
            if let Some(t) = ray_aabb(ro_loc_d, rd_loc_d, min, max) {
                if best.is_none_or(|(_, bt)| t < bt) {
                    best = Some((info.group_id, t));
                }
            }
        }
        best
    }

    /// Ray pick: nearest file AABB → ray ∩ glyph plane → nearest record cell.
    fn pick_ray(&mut self, x: f32, y: f32) -> Option<PickHit> {
        let dbg = std::env::var_os("GLYPH_PICK_DEBUG").is_some();
        let Some((ro, rd)) = self.pixel_ray(x, y) else {
            if dbg {
                println!("pickdbg: px ({x},{y}) — pixel_ray returned None (degenerate unprojection)");
            }
            return None;
        };
        let Some((gid, t_aabb)) = self.ray_file(ro, rd) else {
            if dbg {
                println!(
                    "pickdbg: px ({x},{y}) ro=({:.4},{:.4},{:.4}) rd=({:.6},{:.6},{:.6}) — no file AABB under the ray",
                    ro.x, ro.y, ro.z, rd.x, rd.y, rd.z
                );
            }
            return None;
        };
        let rel_path = self
            .pick
            .as_ref()?
            .files
            .iter()
            .find(|f| f.group_id == gid)?
            .rel_path
            .clone();
        let (_off, sc, _, _) = self.group_trs(gid)?;
        if dbg {
            println!(
                "pickdbg: px ({x},{y}) ro=({:.4},{:.4},{:.4}) rd=({:.6},{:.6},{:.6}) file={rel_path}",
                ro.x, ro.y, ro.z, rd.x, rd.y, rd.z
            );
        }
        if !self.ensure_pick_cache(gid) {
            return Some(PickHit {
                group_id: gid,
                rel_path,
                glyph: None,
            });
        }
        let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
        if c.records.is_empty() {
            return Some(PickHit {
                group_id: gid,
                rel_path,
                glyph: None,
            });
        }

        let affine = self.group_affine(gid)?;
        let inv_affine = affine.inverse();

        let ro_f32 = glam::Vec3::new(ro.x as f32, ro.y as f32, ro.z as f32);
        let rd_f32 = glam::Vec3::new(rd.x as f32, rd.y as f32, rd.z as f32);
        let ro_local = inv_affine.transform_point3(ro_f32);
        let rd_local = inv_affine.transform_vector3(rd_f32);
        let ro_loc_d = DVec3::new(ro_local.x as f64, ro_local.y as f64, ro_local.z as f64);
        let rd_loc_d = DVec3::new(rd_local.x as f64, rd_local.y as f64, rd_local.z as f64);

        // Hit-test glyph records in 3D: for each record r, intersect the ray
        // with the plane z = r.z() in file local space.
        // Records sharing the same z avoid recomputing the ray-plane intersection.
        let mut last_z = f32::NAN;
        let mut cur_t = 0.0f64;
        let mut cur_qx = 0.0f32;
        let mut cur_qy = 0.0f32;

        struct HitCandidate {
            rec: usize,
            dist_world: f32,
            t: f64,
        }

        let mut best_direct: Option<HitCandidate> = None;
        let mut best_near: Option<HitCandidate> = None;
        let mut nearest_overall = (f32::MAX, 0usize, 0.0f32);

        for (i, r) in c.records.iter().enumerate() {
            let rz = r.z();
            if rz != last_z {
                last_z = rz;
                cur_t = if rd_loc_d.z.abs() > 1e-12 {
                    (rz as f64 - ro_loc_d.z) / rd_loc_d.z
                } else {
                    t_aabb
                };
                let p = ro_loc_d + rd_loc_d * cur_t.max(0.0);
                cur_qx = p.x as f32;
                cur_qy = p.y as f32;
            }

            let x0 = r.x();
            let x1 = x0 + r.advance();
            let y0 = r.y() - r.height() * 0.5;
            let y1 = r.y() + r.height() * 0.5;
            let dx = (x0 - cur_qx).max(0.0).max(cur_qx - x1);
            let dy = (y0 - cur_qy).max(0.0).max(cur_qy - y1);
            let d = dx * dx + dy * dy;
            let dist_world = d.sqrt() * (sc.x + sc.y) * 0.5;

            if d < nearest_overall.0 {
                nearest_overall = (d, i, dist_world);
            }

            if dist_world <= 0.8 && cur_t > 0.0 {
                if d == 0.0 {
                    match &mut best_direct {
                        None => {
                            best_direct = Some(HitCandidate { rec: i, dist_world, t: cur_t });
                        }
                        Some(curr) => {
                            if cur_t < curr.t - 1e-5 {
                                *curr = HitCandidate { rec: i, dist_world, t: cur_t };
                            }
                        }
                    }
                } else {
                    match &mut best_near {
                        None => {
                            best_near = Some(HitCandidate { rec: i, dist_world, t: cur_t });
                        }
                        Some(curr) => {
                            let t_diff = cur_t - curr.t;
                            let replaces = if t_diff.abs() > 0.1 {
                                (cur_t < curr.t && dist_world <= curr.dist_world + 0.1)
                                    || (cur_t > curr.t && dist_world + 0.1 < curr.dist_world)
                            } else {
                                dist_world < curr.dist_world - 1e-6
                                    || ((dist_world - curr.dist_world).abs() <= 1e-6 && cur_t < curr.t - 1e-5)
                            };
                            if replaces {
                                *curr = HitCandidate { rec: i, dist_world, t: cur_t };
                            }
                        }
                    }
                }
            }
        }

        let chosen = best_direct.or(best_near);
        let (chosen_rec, chosen_dist) = match &chosen {
            Some(c) => (Some(c.rec), c.dist_world),
            None => (None, nearest_overall.2),
        };

        if dbg {
            let rec_for_dbg = chosen_rec.unwrap_or(nearest_overall.1);
            let r = c.records[rec_for_dbg];
            println!(
                "pickdbg: nearest rec={} row={} col={} cell=({:.4},{:.4}) adv={:.4} h={:.4} dist_world={chosen_dist:.4} (accept ≤ 0.8)",
                rec_for_dbg,
                r.row(),
                r.col(),
                r.x(),
                r.y(),
                r.advance(),
                r.height()
            );
        }

        let glyph = chosen_rec.and_then(|rec| self.make_glyph(gid, rec));
        Some(PickHit {
            group_id: gid,
            rel_path,
            glyph,
        })
    }

    /// Deterministic scripted pick: exact folded (row, col) within the first
    /// file whose path contains `file` (snaps to the nearest col on that row
    /// with a warning if there is no exact record).
    fn pick_row_col(&mut self, file: &str, row: u32, col: u32) -> Option<PickHit> {
        let pctx = self.pick.as_ref()?;
        let info = pctx.files.iter().find(|f| f.rel_path.contains(file))?;
        let gid = info.group_id;
        let rel_path = info.rel_path.clone();
        if !self.ensure_pick_cache(gid) {
            return Some(PickHit {
                group_id: gid,
                rel_path,
                glyph: None,
            });
        }
        let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
        let mut exact = None;
        let mut nearest: Option<(u32, usize)> = None;
        for (i, r) in c.records.iter().enumerate() {
            if r.row() == row {
                if r.col() == col {
                    exact = Some(i);
                    break;
                }
                let d = r.col().abs_diff(col);
                if nearest.is_none_or(|(bd, _)| d < bd) {
                    nearest = Some((d, i));
                }
            }
        }
        if exact.is_none() {
            match nearest {
                Some((_, i)) => {
                    let r = c.records[i];
                    log::warn!(
                        "pick: no exact record at row {row} col {col} in {rel_path}; \
                         snapped to row {} col {}",
                        r.row(),
                        r.col()
                    );
                }
                None => log::warn!("pick: no record on row {row} in {rel_path}"),
            }
        }
        let idx = exact.or(nearest.map(|(_, i)| i));
        let glyph = idx.and_then(|i| self.make_glyph(gid, i));
        if std::env::var_os("GLYPH_PICK_DEBUG").is_some() {
            if let (Some((off, sc, _, _)), Some(g)) = (self.group_trs(gid), &glyph) {
                println!(
                    "pickdbg: target {rel_path} rec={} local=({:.4},{:.4}) adv={:.4} h={:.4} \
                     off=({:.4},{:.4},{:.4}) sc=({:.4},{:.4}) — world cell center ({:.4},{:.4})",
                    g.record,
                    g.pos[0],
                    g.pos[1],
                    g.advance,
                    g.height,
                    off.x,
                    off.y,
                    off.z,
                    sc.x,
                    sc.y,
                    (g.pos[0] + g.advance * 0.5) * sc.x + off.x,
                    g.pos[1] * sc.y + off.y,
                );
            }
        }
        Some(PickHit {
            group_id: gid,
            rel_path,
            glyph,
        })
    }

    /// Resolve a pick command, store it as the current pick, return the log line.
    pub(super) fn apply_pick(&mut self, _ctx: &GpuContext, cmd: &PickCommand) -> Option<String> {
        if self.pick.is_none() {
            return Some("pick: this scene has no pick context (repo mode only)".to_string());
        }
        let hit = match cmd {
            PickCommand::File(f) => {
                let pctx = self.pick.as_ref().expect("pick context checked Some at apply_pick entry");
                pctx.files
                    .iter()
                    .find(|i| i.rel_path.contains(f.as_str()))
                    .map(|i| PickHit {
                        group_id: i.group_id,
                        rel_path: i.rel_path.clone(),
                        glyph: None,
                    })
            }
            PickCommand::RowCol { file, row, col } => self.pick_row_col(file, *row, *col),
            PickCommand::Pixel { x, y } => self.pick_ray(*x, *y),
        };
        match hit {
            Some(h) => {
                // Stage L (L4): drive the selection mask. A glyph pick with
                // a real slot selects that glyph; a file-level pick (or a
                // blank glyph, which has no slot) selects the whole segment.
                self.selection = self.selection_from_hit(&h);
                let line = format_pick(&h);
                self.picked = Some(h);
                Some(line)
            }
            None => {
                // Stage L (L4): a miss CLEARS the selection (click on empty
                // space = deselect). `picked` is untouched — verb semantics
                // (act on the most recent pick) are unchanged.
                self.selection = None;
                Some("pick: MISS (no file under the ray / no path match)".to_string())
            }
        }
    }

    /// Stage L (L4): pick hit → selection mask content.
    fn selection_from_hit(&self, h: &PickHit) -> Option<Selection> {
        if let Some(g) = &h.glyph {
            if let Some(slot) = g.slot {
                return Some(Selection::Glyph {
                    chunk: slot / self.chunk_cap,
                    local: slot % self.chunk_cap,
                });
            }
        }
        // File-level (or blank-glyph) pick: the whole segment's slot range.
        self.pick
            .as_ref()?
            .files
            .iter()
            .find(|f| f.group_id == h.group_id)
            .map(|f| Selection::Segment { slot_base: f.slot_base, slot_count: f.slot_count })
    }

    /// The device buffer holding instance `chunk` — the chunk's own buffer
    /// (the arena holds one buffer per chunk since the chunked mapped form).
    pub(super) fn chunk_buf(&self, chunk: usize) -> &wgpu::Buffer {
        &self.instance_bufs[chunk]
    }

    /// Byte offset of `local` within the chunk's data — the staged buffers
    /// start at their own index 0; the endpoint's extracted pool slices
    /// carry their offset in `chunk_offsets`.
    pub(super) fn chunk_off(&self, chunk: usize, local: u64) -> u64 {
        self.chunk_offsets[chunk] + local
    }

    /// Partial instance-field upload: `data` at byte `field_off` within a
    /// slot (32 B RenderSlot stride, 4-aligned offsets — write_buffer's
    /// requirement).
    fn write_instance(&self, ctx: &GpuContext, slot: u32, field_off: u64, data: &[u8]) {
        let chunk = (slot / self.chunk_cap) as usize;
        let local = (slot % self.chunk_cap) as u64;
        let off = self.chunk_off(chunk, local * 32 + field_off);
        ctx.queue.write_buffer(self.chunk_buf(chunk), off, data);
    }

    /// Upload one edited group row (80 B) — never the whole table.
    pub(super) fn write_group_row(&self, ctx: &GpuContext, gid: u32) {
        if let Some(g) = self.groups_cpu.get(gid as usize) {
            ctx.queue
                .write_buffer(&self.group_buf, gid as u64 * 80, bytemuck::bytes_of(g));
        }
    }

    /// Re-derive a cull segment from the live group TRS: the world AABB
    /// follows offset/scale, and the backdrop tint follows the group color
    /// relative to its as-staged value (so untouched segments keep their
    /// Stage F-fitted tint exactly).
    pub(super) fn sync_segment(&mut self, gid: u32) {
        let Some(g) = self.groups_cpu.get(gid as usize).copied() else {
            return;
        };
        let Some(cull) = &mut self.cull else { return };
        let i = gid as usize;
        if i >= cull.segments.len() {
            return;
        }
        let (ox, oy, oz) = (g.cols[0][0], g.cols[0][1], g.cols[0][2]);
        let (sx, sy, sz) = (
            g.cols[3][0].max(0.0),
            g.cols[3][1].max(0.0),
            g.cols[3][2].max(0.0),
        );
        cull.segments[i].min = [
            cull.local_min[i][0] * sx + ox,
            cull.local_min[i][1] * sy + oy,
            cull.local_min[i][2] * sz + oz,
        ];
        cull.segments[i].max = [
            cull.local_max[i][0] * sx + ox,
            cull.local_max[i][1] * sy + oy,
            cull.local_max[i][2] * sz + oz,
        ];
        let bt = cull.base_tint[i];
        let orig = cull.orig_group_rgb[i];
        let mut t = bt;
        for (c, tc) in t.iter_mut().enumerate().take(3) {
            let newc = g.cols[2][c].max(0.0).powf(2.2);
            let oldc = orig[c].max(1e-6).powf(2.2);
            *tc = (bt[c] * newc / oldc).min(1.0);
        }
        cull.segments[i].tint = t;
    }

    /// Apply a manipulation verb to the current pick. All GPU writes are
    /// partial uploads; the return string is the audit log line.
    pub(super) fn apply_verb(&mut self, ctx: &GpuContext, verb: &Verb) -> String {
        let Some(hit) = &self.picked else {
            return "verb: nothing picked yet — ignored".to_string();
        };
        let gid = hit.group_id;
        let rel = hit.rel_path.clone();
        let glyph = hit.glyph.clone();
        let pack = |rgb: [u8; 3]| -> u32 {
            rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 | 0xFF00_0000
        };
        match verb {
            Verb::RecolorGlyph(rgb) => {
                let Some(g) = &glyph else {
                    return format!("verb recolor-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb recolor-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                self.write_instance(ctx, slot, 16, &pack(*rgb).to_le_bytes());
                format!(
                    "verb recolor-glyph: {rel} row {} col {} slot {slot} -> #{:02x}{:02x}{:02x} (4 B)",
                    g.row, g.col, rgb[0], rgb[1], rgb[2]
                )
            }
            Verb::RecolorLine(rgb) => {
                let Some(g) = &glyph else {
                    return format!("verb recolor-line: {rel} pick has no glyph");
                };
                let row = g.row;
                if !self.ensure_pick_cache(gid) {
                    return format!("verb recolor-line: {rel} pick cache unavailable");
                }
                let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
                let packed = pack(*rgb);
                // Collect (slot, record) for the row, coalesce into runs of
                // CONTIGUOUS SLOTS, then rebuild the full 32 B slot for each
                // run from the cache (the color field is strided 32 B apart —
                // a raw color-only byte range would stomp neighboring fields;
                // a rebuilt-slot range write keeps it to ONE write_buffer per
                // run).
                let mut runs: Vec<(u32, Vec<RenderSlot>)> = Vec::new();
                let mut total = 0usize;
                for (i, r) in c.records.iter().enumerate() {
                    if r.row() != row {
                        continue;
                    }
                    let s = c.slot_of[i];
                    if s == u32::MAX {
                        continue;
                    }
                    total += 1;
                    let (mut pos, mut advance, mut height) =
                        ([r.x(), r.y(), r.z()], r.advance(), r.height());
                    // Preserve earlier nudge/scale-glyph edits on this slot.
                    if let Some((p, a, h)) = self.geom_overrides.get(&s) {
                        pos = *p;
                        advance = *a;
                        height = *h;
                    }
                    let inst = RenderSlot {
                        pos,
                        glyph_id: r.glyph_id(),
                        color: packed,
                        group_id: gid,
                        advance,
                        height,
                    };
                    match runs.last_mut() {
                        Some((start, insts))
                            if *start + insts.len() as u32 == s
                                && *start / self.chunk_cap == s / self.chunk_cap =>
                        {
                            insts.push(inst);
                        }
                        _ => runs.push((s, vec![inst])),
                    }
                }
                let mut bytes = 0u64;
                for (start, insts) in &runs {
                    let chunk = (*start / self.chunk_cap) as usize;
                    let local = (*start % self.chunk_cap) as u64;
                    let off = self.chunk_off(chunk, local * 32);
                    ctx.queue
                        .write_buffer(self.chunk_buf(chunk), off, bytemuck::cast_slice(insts));
                    bytes += insts.len() as u64 * 32;
                }
                format!(
                    "verb recolor-line: {rel} row {row} — {total} glyphs in {} run(s), {bytes} B uploaded",
                    runs.len()
                )
            }
            Verb::NudgeGlyph(d) => {
                let Some(g) = &glyph else {
                    return format!("verb nudge-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb nudge-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                let new = [g.pos[0] + d[0], g.pos[1] + d[1], g.pos[2] + d[2]];
                self.write_instance(ctx, slot, 0, bytemuck::cast_slice(&new));
                self.geom_overrides
                    .entry(slot)
                    .or_insert((new, g.advance, g.height))
                    .0 = new;
                if let Some(h) = &mut self.picked {
                    if let Some(pg) = &mut h.glyph {
                        pg.pos = new;
                    }
                }
                format!(
                    "verb nudge-glyph: {rel} slot {slot} pos -> ({:.2},{:.2},{:.2}) (12 B)",
                    new[0], new[1], new[2]
                )
            }
            Verb::ScaleGlyph(f) => {
                let Some(g) = &glyph else {
                    return format!("verb scale-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb scale-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                let new_ah = [g.advance * f, g.height * f];
                self.write_instance(ctx, slot, 24, bytemuck::cast_slice(&new_ah));
                let ov = self
                    .geom_overrides
                    .entry(slot)
                    .or_insert((g.pos, g.advance, g.height));
                ov.1 = new_ah[0];
                ov.2 = new_ah[1];
                if let Some(h) = &mut self.picked {
                    if let Some(pg) = &mut h.glyph {
                        pg.advance = new_ah[0];
                        pg.height = new_ah[1];
                    }
                }
                format!(
                    "verb scale-glyph: {rel} slot {slot} advance/height -> ({:.2},{:.2}) x{f} (8 B)",
                    new_ah[0], new_ah[1]
                )
            }
            Verb::MoveGroup(d) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb move-group: group {gid} out of range");
                };
                g.cols[0][0] += d[0];
                g.cols[0][1] += d[1];
                g.cols[0][2] += d[2];
                let off = [g.cols[0][0], g.cols[0][1], g.cols[0][2]];
                self.write_group_row(ctx, gid);
                self.sync_segment(gid);
                format!(
                    "verb move-group: {rel} group {gid} offset -> ({:.1},{:.1},{:.1}) (80 B row)",
                    off[0], off[1], off[2]
                )
            }
            Verb::ScaleGroup(f) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb scale-group: group {gid} out of range");
                };
                for c in 0..3 {
                    g.cols[3][c] = (g.cols[3][c] * f).clamp(0.001, 100.0);
                }
                let s = g.cols[3][0];
                self.write_group_row(ctx, gid);
                self.sync_segment(gid);
                format!("verb scale-group: {rel} group {gid} scale -> {s:.3} (80 B row)")
            }
            Verb::TintGroup(rgb) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb tint-group: group {gid} out of range");
                };
                g.cols[2][0] = rgb[0];
                g.cols[2][1] = rgb[1];
                g.cols[2][2] = rgb[2];
                self.write_group_row(ctx, gid);
                self.sync_segment(gid);
                format!(
                    "verb tint-group: {rel} group {gid} tint -> ({:.2},{:.2},{:.2}) (80 B row)",
                    rgb[0], rgb[1], rgb[2]
                )
            }
            Verb::TintCycle => {
                let i = gid as usize;
                let step = self.tint_step.get(i).copied().unwrap_or(0) + 1;
                if i < self.tint_step.len() {
                    self.tint_step[i] = step;
                }
                let rgb = crate::repo::DIR_TINTS[(step as usize) % crate::repo::DIR_TINTS.len()];
                let line = self.apply_verb(ctx, &Verb::TintGroup(rgb));
                format!("{line} [palette step {step}]")
            }
            Verb::SetHidden(hide) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb hide/show: group {gid} out of range");
                };
                g.cols[2][3] = if *hide { 0.0 } else { 1.0 };
                if let Some(cull) = &mut self.cull {
                    if (gid as usize) < cull.hidden.len() {
                        cull.hidden[gid as usize] = *hide;
                    }
                }
                self.write_group_row(ctx, gid);
                format!(
                    "verb {}: {rel} group {gid} (alpha -> {}, 80 B row; cull skips the segment)",
                    if *hide { "hide-group" } else { "show-group" },
                    g_alpha(self.groups_cpu.get(gid as usize)),
                )
            }
            Verb::ToggleHidden => {
                let hidden = self.group_hidden(gid);
                self.apply_verb(ctx, &Verb::SetHidden(!hidden))
            }
        }
    }
}

/// Slab ray-AABB test; returns the entry t (0 when the origin is inside).
fn ray_aabb(ro: DVec3, rd: DVec3, min: DVec3, max: DVec3) -> Option<f64> {
    let mut t0 = 0.0f64;
    let mut t1 = f64::MAX;
    for ax in 0..3 {
        let (o, d, lo, hi) = (ro[ax], rd[ax], min[ax], max[ax]);
        if d.abs() < 1e-15 {
            if o < lo || o > hi {
                return None;
            }
        } else {
            let inv = 1.0 / d;
            let (mut ta, mut tb) = ((lo - o) * inv, (hi - o) * inv);
            if ta > tb {
                std::mem::swap(&mut ta, &mut tb);
            }
            t0 = t0.max(ta);
            t1 = t1.min(tb);
            if t0 > t1 {
                return None;
            }
        }
    }
    Some(t0)
}

/// One-line pick report: file, group, folded row/col, source line, byte
/// offset, the actual character, and the arena slot.
pub(super) fn format_pick(h: &PickHit) -> String {
    match &h.glyph {
        Some(g) => format!(
            "pick: {} group={} rec={} row={} col={} line={} byte={} char={:?} slot={} pos=({:.2},{:.2},{:.2})",
            h.rel_path,
            h.group_id,
            g.record,
            g.row,
            g.col,
            g.line,
            g.byte_off,
            g.ch,
            g.slot.map_or("-".to_string(), |s| s.to_string()),
            g.pos[0],
            g.pos[1],
            g.pos[2],
        ),
        None => format!(
            "pick: {} group={} (file-level pick, no glyph resolved)",
            h.rel_path, h.group_id
        ),
    }
}

fn g_alpha(g: Option<&GroupRow>) -> f32 {
    g.map_or(f32::NAN, |g| g.cols[2][3])
}

#[cfg(test)]
mod pick_3d_tests {
    use super::*;

    #[test]
    fn ray_aabb_3d_depth_box() {
        let ro = DVec3::new(0.0, 0.0, 10.0);
        let rd = DVec3::new(0.0, 0.0, -1.0);
        let min = DVec3::new(-1.0, -1.0, -80.0);
        let max = DVec3::new(1.0, 1.0, -20.0);
        let t = ray_aabb(ro, rd, min, max);
        assert!(t.is_some(), "ray along -Z must pierce 3D depth AABB");
        assert!((t.unwrap() - 30.0).abs() < 1e-6);
    }
}
