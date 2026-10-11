//! Stage G — CPU picking + live instance/group manipulation: the pick
//! types, the one-entry re-derived-file cache, ray/hit resolution and the
//! verb write-back path. Extracted from `glyph_scene.rs` in the 2026-09
//! code-shape refactor — a pure move; `pub(super)` stands in for the
//! same-module privacy these items had. The pick contract itself (why
//! CPU-side, the resolution order) lives in the `glyph_scene.rs` header.

use glam::DVec3;
use std::path::PathBuf;

use glyph_field_visible::{GlyphOverride, NO_GROUP};

use super::{fov_y_deg, CameraMode, GlyphPlacement, GlyphScene, GroupRow, Selection};
use crate::gpu::GpuContext;
use crate::layout::{GlyphRecord, ItemParams};

// ── Stage G — picking & manipulation types ─────────────────────────────────

/// Per-file pick record: everything needed to (a) hit-test the file's AABB
/// under the LIVE group TRS and (b) re-derive its glyph geometry via a
/// deterministic engine re-run (`repo::rederive_cached` with `item`).
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
    pub files: Vec<PickFileInfo>,
    pub content: Option<std::collections::HashMap<String, std::sync::Arc<Vec<u8>>>>,
    pub folds: std::collections::HashMap<String, Vec<std::ops::Range<u32>>>,
}

/// A pick request — scripted (CLI) or interactive (click).
#[derive(Clone, Debug)]
pub enum PickCommand {
    /// Group-level pick: first file whose rel path contains the substring.
    File(String),
    /// Group-level pick: exact group ID.
    Group(u32),
    /// Deterministic glyph pick: exact folded (row, col) within that file.
    RowCol { file: String, row: u32, col: u32 },
    /// Ray pick through a physical pixel of the current viewport.
    Pixel { x: f32, y: f32 },
    /// Clear the active selection.
    Clear,
}

/// A manipulation verb, applied to the current pick. Instance verbs need a
/// glyph pick; group verbs need at least a file pick.
#[derive(Clone, Debug)]
pub enum Verb {
    /// Recolor the picked glyph (packed sRGB rgb, alpha kept 255). None =
    /// `[verbs] recolor_glyph`, resolved when applied — the verb parser runs
    /// inside clap, before the launch config's settings are installed.
    RecolorGlyph(Option<[u8; 3]>),
    /// Recolor every glyph on the picked glyph's folded row ("highlight
    /// line"). None = `[verbs] recolor_line`, resolved when applied.
    RecolorLine(Option<[u8; 3]>),
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
    /// Set a per-glyph background color (RGBA) by allocating/repointing its group.
    SetGlyphBackground([f32; 4]),
    /// Set a per-glyph transform (translation, rotation quat, scale) by repointing its group.
    SetGlyphTransform([f32; 3], [f32; 4], [f32; 3]),
    /// Reset a glyph's group back to its original file group.
    ResetGlyphGroup,
    /// `--layout-mode library` controls (page, form, stack, sort): they need
    /// no pick (paging and form address the picked file's volume when there
    /// is one, else every volume) and only retarget — the frames ease.
    Library(crate::library::LibraryVerb),
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
    /// The file's length in bytes — the end of a whole-item byte range.
    byte_len: usize,
}

// ── M3: the Visible field's keys — (item, byte) in place of a slot ─────────
//
// The Visible field keeps no slot per glyph, so everything the scene keyed by
// slot is re-keyed there by the item (the file's index in the load, == its
// group id at load) and the glyph's LEADER byte offset within the file. The
// CPU pick already yields both (`PickFileInfo::group_id`, `PickGlyph::byte_off`);
// the helpers below are the pure half, so a test can hold the construction
// without a device.

/// The selection a pick resolves to in the Visible field: a glyph with a
/// slot is its own leader bytes `[byte, byte + utf8 len)`; a file-level pick
/// or a blank glyph (which draws nothing) is the whole item `[0, byte_len)`
/// (`u32::MAX` when the length is unknown — every leader lies below it).
pub(super) fn byte_range_selection(item: u32, glyph: Option<&PickGlyph>, byte_len: Option<u32>) -> Selection {
    match glyph {
        Some(g) if g.slot.is_some() => {
            let (start, end) = glyph_byte_range(g.byte_off, g.ch);
            Selection::ByteRange { item, start, end }
        }
        _ => Selection::ByteRange { item, start: 0, end: byte_len.unwrap_or(u32::MAX) },
    }
}

/// One glyph's leader bytes: `[byte_off, byte_off + utf8 len)`.
pub(super) fn glyph_byte_range(byte_off: usize, ch: char) -> (u32, u32) {
    (byte_off as u32, (byte_off + ch.len_utf8()) as u32)
}

/// The byte range `[start, end)` covering every record on folded row `row`:
/// `records` yields `(row, leader byte, codepoint)` per record, as the pick
/// cache holds them. The records of a row are byte-contiguous, so the range
/// is the first leader to the last leader's end. None when the row is empty.
pub(super) fn row_byte_range(records: impl Iterator<Item = (u32, usize, u32)>, row: u32) -> Option<(u32, u32)> {
    let mut range: Option<(usize, usize)> = None;
    for (r, byte, cp) in records {
        if r != row {
            continue;
        }
        let end = byte + char::from_u32(cp).map_or(1, char::len_utf8);
        range = Some(match range {
            None => (byte, end),
            Some((s, e)) => (s.min(byte), e.max(end)),
        });
    }
    range.map(|(s, e)| (s as u32, e as u32))
}

/// One verb's effect on a glyph's override.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum OverrideEdit {
    /// `recolor-glyph`: replace the colour (packed RGBA8, alpha 255 — never
    /// zero, which the kernel reads as "no colour override").
    Color(u32),
    /// `nudge-glyph`: add to the x nudge (the slot carries no y/z, as in
    /// Derived mode).
    NudgeX(f32),
    /// `set-glyph-background` / `set-glyph-transform`: the allocated group row.
    Group(u32),
    /// `reset-glyph-group`: back to the item's own group.
    ClearGroup,
}

/// Merge one edit into a glyph's override: the other lanes keep what earlier
/// verbs set (a recolour after a nudge keeps the nudge, and vice versa).
pub(super) fn merged_override(existing: Option<GlyphOverride>, item: u32, byte: u32, edit: OverrideEdit) -> GlyphOverride {
    let mut ov = existing.unwrap_or(GlyphOverride { item, byte, color: 0, x_nudge: 0.0, group: NO_GROUP });
    debug_assert_eq!((ov.item, ov.byte), (item, byte), "an override is merged under its own key");
    match edit {
        OverrideEdit::Color(c) => ov.color = c,
        OverrideEdit::NudgeX(dx) => ov.x_nudge += dx,
        OverrideEdit::Group(g) => ov.group = g,
        OverrideEdit::ClearGroup => ov.group = NO_GROUP,
    }
    ov
}

/// An override that overrides nothing — cleared from the field rather than
/// kept as a no-op entry.
pub(super) fn override_is_empty(ov: &GlyphOverride) -> bool {
    ov.color == 0 && ov.x_nudge == 0.0 && ov.group == NO_GROUP
}

/// The verb's CLI name, for the reply lines.
fn verb_name(verb: &Verb) -> &'static str {
    match verb {
        Verb::RecolorGlyph(_) => "recolor-glyph",
        Verb::RecolorLine(_) => "recolor-line",
        Verb::NudgeGlyph(_) => "nudge-glyph",
        Verb::ScaleGlyph(_) => "scale-glyph",
        Verb::MoveGroup(_) => "move-group",
        Verb::ScaleGroup(_) => "scale-group",
        Verb::TintGroup(_) => "tint-group",
        Verb::TintCycle => "tint-cycle",
        Verb::SetHidden(true) => "hide-group",
        Verb::SetHidden(false) => "show-group",
        Verb::ToggleHidden => "toggle-hidden",
        Verb::SetGlyphBackground(_) => "set-glyph-background",
        Verb::SetGlyphTransform(..) => "set-glyph-transform",
        Verb::ResetGlyphGroup => "reset-glyph-group",
        Verb::Library(_) => "library",
    }
}

impl GlyphScene {
    /// The Visible field's key for the picked file: its item index (the
    /// file's position in the load, == its group id at load). None for the
    /// stored modes, which keep their slot paths.
    pub(super) fn visible_item_of(&self, gid: u32) -> Option<u32> {
        self.field.visible()?;
        let pctx = self.pick.as_ref()?;
        // A repo load files item i under group i, so the direct probe answers
        // every load group; the scan is for anything else. Measured
        // 2026-10-10: the scan alone, once per moved group, was 217 ms of a
        // library frame animating 29,377 files (O(N²)); the probe makes it
        // vanish from the profile.
        if pctx.files.get(gid as usize).is_some_and(|f| f.group_id == gid) {
            return Some(gid);
        }
        pctx.files.iter().position(|f| f.group_id == gid).map(|i| i as u32)
    }

    /// Merge `edit` into the glyph's override (the scene's map is the source
    /// of truth) and push the result to the Visible field — or clear it when
    /// nothing is left to override.
    fn set_visible_override(&mut self, ctx: &GpuContext, key: (u32, u32), edit: OverrideEdit) -> GlyphOverride {
        let ov = merged_override(self.glyph_overrides.get(&key).copied(), key.0, key.1, edit);
        let visible = self.field.visible().expect("set_visible_override: the field is Visible");
        if override_is_empty(&ov) {
            self.glyph_overrides.remove(&key);
            visible.clear_glyph_override(&ctx.queue, key.0, key.1);
        } else {
            self.glyph_overrides.insert(key, ov);
            visible.set_glyph_override(&ctx.queue, ov);
        }
        ov
    }

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
        let Ok(records) = crate::repo::rederive_cached(&bytes, &info.item) else {
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
            byte_len: bytes.len(),
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
        let tan = (fov_y_deg() as f64 * 0.5).to_radians().tan();
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
        if matches!(cmd, PickCommand::Clear) {
            self.selection = None;
            self.picked = None;
            return Some("pick: cleared".to_string());
        }
        if self.pick.is_none() {
            return Some("pick: this scene has no pick context (repo mode only)".to_string());
        }
        let hit = match cmd {
            PickCommand::Clear => unreachable!(),
            PickCommand::Group(gid) => {
                let pctx = self.pick.as_ref().expect("pick context checked Some at apply_pick entry");
                pctx.files
                    .iter()
                    .find(|i| i.group_id == *gid)
                    .map(|i| PickHit {
                        group_id: i.group_id,
                        rel_path: i.rel_path.clone(),
                        glyph: None,
                    })
            }
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
    fn selection_from_hit(&mut self, h: &PickHit) -> Option<Selection> {
        // The Visible field keeps no slot per glyph: the selection is the
        // picked glyph's leader bytes, or the whole item for a file-level
        // (or blank-glyph) pick — the length comes from the pick cache, which
        // a glyph pick has already filled and a file pick fills here.
        if let Some(item) = self.visible_item_of(h.group_id) {
            let whole_item = !h.glyph.as_ref().is_some_and(|g| g.slot.is_some());
            let byte_len = (whole_item && self.ensure_pick_cache(h.group_id))
                .then(|| self.cache.as_ref().map(|c| c.byte_len as u32))
                .flatten();
            return Some(byte_range_selection(item, h.glyph.as_ref(), byte_len));
        }
        if let Some(g) = &h.glyph {
            if let Some(slot) = g.slot {
                let chunk_capacity = self.field.chunk_capacity();
                return Some(Selection::Glyph {
                    chunk: slot / chunk_capacity,
                    local: slot % chunk_capacity,
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

    /// Upload one edited group row (96 B) — never the whole table.
    pub(super) fn write_group_row(&self, ctx: &GpuContext, gid: u32) {
        if let Some(g) = self.groups_cpu.get(gid as usize) {
            ctx.queue
                .write_buffer(&self.group_buf, gid as u64 * 96, bytemuck::bytes_of(g));
        }
    }

    /// Find an existing matching group row or allocate a new one (up to max_groups).
    pub(super) fn allocate_group_row(&mut self, ctx: &GpuContext, row: crate::glyph_scene::GroupRow) -> Option<u32> {
        let max_groups = (self.group_buf.size() / 96) as u32;
        for (i, r) in self.groups_cpu.iter().enumerate() {
            if bytemuck::bytes_of(r) == bytemuck::bytes_of(&row) {
                return Some(i as u32);
            }
        }
        if (self.groups_cpu.len() as u32) < max_groups {
            let gid = self.groups_cpu.len() as u32;
            self.groups_cpu.push(row);
            // The whole row goes up (clip and background are not the
            // tree's), and the row becomes a node like every other group.
            self.write_group_row(ctx, gid);
            debug_assert_eq!(self.nodes.len(), gid as usize, "group ids and nodes stay in step");
            self.nodes.push_row(&row);
            return Some(gid);
        }
        None
    }

    /// Rows an EXTERNAL writer (the layout controller's bevy sync: carrel
    /// zones, decks, the library's animation) left in the CPU mirror: adopt
    /// them into the group nodes, which upload the changed local rows and
    /// resolve them into the table at the next frame (2026-10-10; before
    /// this, the rows themselves went up, min..max span or row by row). The
    /// adoption keeps a verb's tint and hide that the writer did not change
    /// (`GroupNodes::adopt`, the library probe's E4).
    pub(super) fn write_group_rows(&mut self, _ctx: &GpuContext, gids: &[u32]) {
        for &gid in gids {
            if let Some(row) = self.groups_cpu.get_mut(gid as usize) {
                self.nodes.adopt(gid, row);
            }
        }
    }

    /// A group verb has written group `gid`'s node: refresh the CPU mirror
    /// from the tables (what the GPU will resolve) and re-derive its cull
    /// segment from it.
    pub(super) fn group_node_edited(&mut self, ctx: &GpuContext, gid: u32) {
        if let Some(row) = self.groups_cpu.get_mut(gid as usize) {
            self.nodes.mirror(gid, row);
        }
        self.sync_segment(ctx, gid);
    }

    /// Re-derive a cull segment from the live group TRS: the world AABB
    /// follows offset/scale, and the backdrop tint follows the group color
    /// relative to its as-staged value (so untouched segments keep their
    /// Stage F-fitted tint exactly). The Visible field culls its items by
    /// its own copy of that box, so the recomputed one is pushed to it too
    /// (M3) — every group edit lands here, whichever verb or drag moved it.
    pub(super) fn sync_segment(&mut self, ctx: &GpuContext, gid: u32) {
        let Some(g) = self.groups_cpu.get(gid as usize).copied() else {
            return;
        };
        let (ox, oy, oz) = (g.cols[0][0], g.cols[0][1], g.cols[0][2]);
        let (sx, sy, sz) = (
            g.cols[3][0].max(0.0),
            g.cols[3][1].max(0.0),
            g.cols[3][2].max(0.0),
        );
        let i = gid as usize;
        let mut world_box: Option<([f32; 3], [f32; 3])> = None;
        if let Some(cull) = &mut self.cull {
            if i >= cull.segments.len() {
                return;
            }
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
            if let Some(e) = cull.em_scale.get_mut(i) {
                *e = sy;
            }
            if let Some(lblks) = cull.local_blocks.get(i) {
                for (b_idx, lb) in lblks.iter().enumerate() {
                    if let Some(b) = cull.segments[i].blocks.get_mut(b_idx) {
                        b.min = [
                            lb.min[0] * sx + ox,
                            lb.min[1] * sy + oy,
                            lb.min[2] * sz + oz,
                        ];
                        b.max = [
                            lb.max[0] * sx + ox,
                            lb.max[1] * sy + oy,
                            lb.max[2] * sz + oz,
                        ];
                    }
                }
            }
            let bt = cull.base_tint[i];
            let orig = cull.orig_group_rgb[i];
            let mut t = bt;
            for (c, tc) in t.iter_mut().enumerate().take(3) {
                let newc = g.cols[2][c].max(0.0).powf(2.2);
                let oldc = orig[c].max(1e-6).powf(2.2);
                *tc = (bt[c] * newc / oldc).min(1.0);
            }
            cull.segments[i].tint = t;
            world_box = Some((cull.segments[i].min, cull.segments[i].max));
        } else if let Some(info) = self.pick.as_ref().and_then(|p| p.files.iter().find(|f| f.group_id == gid)) {
            // --no-cull: no segment table, but the Visible field still culls
            // itself — the pick AABB carries the same margins (plus a z pad).
            world_box = Some((
                [info.aabb_min[0] * sx + ox, info.aabb_min[1] * sy + oy, info.aabb_min[2] * sz + oz],
                [info.aabb_max[0] * sx + ox, info.aabb_max[1] * sy + oy, info.aabb_max[2] * sz + oz],
            ));
        }
        if let (Some((min, max)), Some(item)) = (world_box, self.visible_item_of(gid)) {
            if let Some(visible) = self.field.visible() {
                visible.set_item_bbox(&ctx.queue, item, min, max);
            }
        }
    }

    /// Apply a manipulation verb to the current pick. All GPU writes are
    /// partial uploads; the return string is the audit log line.
    pub(super) fn apply_verb(&mut self, ctx: &GpuContext, verb: &Verb) -> String {
        if let Verb::Library(v) = verb {
            return self.apply_library_verb(v);
        }
        let Some(hit) = &self.picked else {
            return "verb: nothing picked yet — ignored".to_string();
        };
        let gid = hit.group_id;
        let rel = hit.rel_path.clone();
        let glyph = hit.glyph.clone();
        // The glyph verbs address a SLOT; the Visible field has none (its
        // glyphs are re-laid every frame), so there they are keyed by
        // (item, byte) instead — `apply_glyph_verb_visible`. The group verbs
        // edit the group table either way and share the path below (where
        // `sync_segment` and `SetHidden` also tell the field).
        if let Some(item) = self.visible_item_of(gid) {
            if matches!(
                verb,
                Verb::RecolorGlyph(_)
                    | Verb::RecolorLine(_)
                    | Verb::NudgeGlyph(_)
                    | Verb::ScaleGlyph(_)
                    | Verb::SetGlyphBackground(_)
                    | Verb::SetGlyphTransform(..)
                    | Verb::ResetGlyphGroup
            ) {
                return self.apply_glyph_verb_visible(ctx, item, verb);
            }
        }
        let pack = |rgb: [u8; 3]| -> u32 {
            rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 | 0xFF00_0000
        };
        match verb {
            Verb::RecolorGlyph(rgb) => {
                let rgb = &rgb.unwrap_or(crate::config::settings().verbs.recolor_glyph);
                let Some(g) = &glyph else {
                    return format!("verb recolor-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb recolor-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                self.field.write_color(&ctx.queue, slot, pack(*rgb));
                format!(
                    "verb recolor-glyph: {rel} row {} col {} slot {slot} -> #{:02x}{:02x}{:02x} (4 B)",
                    g.row, g.col, rgb[0], rgb[1], rgb[2]
                )
            }
            Verb::RecolorLine(rgb) => {
                let rgb = &rgb.unwrap_or(crate::config::settings().verbs.recolor_line);
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
                // CONTIGUOUS SLOTS, then rebuild the full placement for each
                // run from the cache (the color field is strided apart in
                // slot storage — a raw color-only byte range would stomp
                // neighboring fields; a rebuilt-placement range write keeps
                // it to ONE write per run, per chunk).
                let chunk_capacity = self.field.chunk_capacity();
                let mut runs: Vec<(u32, Vec<GlyphPlacement>)> = Vec::new();
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
                    let inst = GlyphPlacement {
                        position: pos,
                        glyph_id: r.glyph_id(),
                        color: packed,
                        group_id: gid,
                        advance,
                        height,
                    };
                    match runs.last_mut() {
                        Some((start, insts))
                            if *start + insts.len() as u32 == s
                                && *start / chunk_capacity == s / chunk_capacity =>
                        {
                            insts.push(inst);
                        }
                        _ => runs.push((s, vec![inst])),
                    }
                }
                let mut bytes = 0u64;
                for (start, insts) in &runs {
                    self.field.write_placements(&ctx.queue, *start, insts);
                    bytes += insts.len() as u64 * u64::from(self.field.slot_bytes());
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
                self.field.write_position(&ctx.queue, slot, new);
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
                self.field.write_extent(&ctx.queue, slot, new_ah[0], new_ah[1]);
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
            // The group verbs write the group's NODE — one table each (the
            // local row, or the appearance row) — and the resolve pass writes
            // the group row; the CPU mirror follows from the tables.
            Verb::MoveGroup(d) => {
                if !self.nodes.translate(gid, *d) {
                    return format!("verb move-group: group {gid} out of range");
                }
                self.group_node_edited(ctx, gid);
                let off = self.groups_cpu[gid as usize].cols[0];
                format!(
                    "verb move-group: {rel} group {gid} offset -> ({:.1},{:.1},{:.1}) (32 B local row)",
                    off[0], off[1], off[2]
                )
            }
            Verb::ScaleGroup(f) => {
                let Some(s) = self.nodes.scale_by(gid, *f) else {
                    return format!("verb scale-group: group {gid} out of range");
                };
                self.group_node_edited(ctx, gid);
                format!("verb scale-group: {rel} group {gid} scale -> {s:.3} (32 B local row)")
            }
            Verb::TintGroup(rgb) => {
                if !self.nodes.set_tint(gid, *rgb) {
                    return format!("verb tint-group: group {gid} out of range");
                }
                self.group_node_edited(ctx, gid);
                format!(
                    "verb tint-group: {rel} group {gid} tint -> ({:.2},{:.2},{:.2}) (32 B appearance row)",
                    rgb[0], rgb[1], rgb[2]
                )
            }
            Verb::TintCycle => {
                let i = gid as usize;
                let step = self.tint_step.get(i).copied().unwrap_or(0) + 1;
                if i < self.tint_step.len() {
                    self.tint_step[i] = step;
                }
                let tints = crate::repo::dir_tints();
                let rgb = tints[(step as usize) % tints.len()];
                let line = self.apply_verb(ctx, &Verb::TintGroup(rgb));
                format!("{line} [palette step {step}]")
            }
            Verb::SetGlyphBackground(bg_rgba) => {
                let Some(g) = &glyph else {
                    return format!("verb set-glyph-background: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb set-glyph-background: {rel} '{}' is blank (no instance)", g.ch);
                };
                // Read the glyph's CURRENT group (to inherit its TRS/tint)
                let current_gid = gid;
                
                let mut row = self.groups_cpu.get(current_gid as usize).copied().unwrap_or(crate::glyph_scene::GroupRow::identity([0.0; 3]));
                row.cols[5] = *bg_rgba; // Set background color
                
                if let Some(new_gid) = self.allocate_group_row(ctx, row) {
                    self.field.write_group_id(&ctx.queue, slot, gid, new_gid);
                    format!("verb set-glyph-background: {rel} row {} col {} slot {slot} -> group {new_gid}", g.row, g.col)
                } else {
                    format!("verb set-glyph-background: {rel} out of groups (max {})", self.group_buf.size() / 96)
                }
            }
            Verb::SetGlyphTransform(t, q, s) => {
                let Some(g) = &glyph else {
                    return format!("verb set-glyph-transform: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb set-glyph-transform: {rel} '{}' is blank (no instance)", g.ch);
                };
                let mut row = self.groups_cpu.get(gid as usize).copied().unwrap_or(crate::glyph_scene::GroupRow::identity([0.0; 3]));
                row.cols[0] = [t[0], t[1], t[2], 0.0];
                row.cols[1] = *q;
                row.cols[3] = [s[0], s[1], s[2], row.cols[3][3]];
                
                if let Some(new_gid) = self.allocate_group_row(ctx, row) {
                    self.field.write_group_id(&ctx.queue, slot, gid, new_gid);
                    format!("verb set-glyph-transform: {rel} row {} col {} slot {slot} -> group {new_gid}", g.row, g.col)
                } else {
                    format!("verb set-glyph-transform: {rel} out of groups (max {})", self.group_buf.size() / 96)
                }
            }
            Verb::ResetGlyphGroup => {
                let Some(g) = &glyph else {
                    return format!("verb reset-glyph-group: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb reset-glyph-group: {rel} '{}' is blank (no instance)", g.ch);
                };
                // Reset to the file's original group
                self.field.write_group_id(&ctx.queue, slot, gid, gid);
                format!("verb reset-glyph-group: {rel} row {} col {} slot {slot} -> group {gid}", g.row, g.col)
            }
            Verb::Library(_) => unreachable!("apply_verb answers the library verbs before the pick check"),
            Verb::SetHidden(hide) => {
                if !self.nodes.set_alpha(gid, if *hide { 0.0 } else { 1.0 }) {
                    return format!("verb hide/show: group {gid} out of range");
                }
                if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
                    self.nodes.mirror(gid, g);
                }
                if let Some(cull) = &mut self.cull {
                    if (gid as usize) < cull.hidden.len() {
                        cull.hidden[gid as usize] = *hide;
                    }
                }
                // The Visible field culls its items itself: tell it, so the
                // item leaves (or rejoins) every tier and the HUD's counts move.
                let item_note = match self.visible_item_of(gid) {
                    Some(item) => {
                        if let Some(visible) = self.field.visible() {
                            visible.set_item_hidden(&ctx.queue, item, *hide);
                        }
                        format!("; item {item} hidden={hide}")
                    }
                    None => String::new(),
                };
                format!(
                    "verb {}: {rel} group {gid} (alpha -> {}, 32 B appearance row; cull skips the segment{item_note})",
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

    /// The glyph verbs on the Visible field, keyed by (item, byte) — M3.
    /// `recolor-glyph`, `nudge-glyph`, `set-glyph-background`,
    /// `set-glyph-transform` and `reset-glyph-group` merge into the glyph's
    /// override (`glyph_overrides` is the source of truth; the field holds a
    /// copy); `recolor-line` is a byte-range span over the picked row;
    /// `scale-glyph` is not representable (the slot's advance and height come
    /// from the atlas table, as in Derived) and says so. Every reply names
    /// the item and byte it applied to.
    fn apply_glyph_verb_visible(&mut self, ctx: &GpuContext, item: u32, verb: &Verb) -> String {
        let hit = self.picked.clone().expect("apply_glyph_verb_visible: caller checked the pick");
        let gid = hit.group_id;
        let rel = hit.rel_path;
        let name = verb_name(verb);
        let Some(g) = hit.glyph else {
            return format!("verb {name}: {rel} pick has no glyph");
        };
        if g.slot.is_none() {
            return format!("verb {name}: {rel} '{}' is blank (no glyph to override)", g.ch);
        }
        let byte = g.byte_off as u32;
        let key = (item, byte);
        let pack = |rgb: [u8; 3]| -> u32 { rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 | 0xFF00_0000 };
        let hex = |rgb: [u8; 3]| format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
        match verb {
            Verb::RecolorGlyph(rgb) => {
                let rgb = rgb.unwrap_or(crate::config::settings().verbs.recolor_glyph);
                self.set_visible_override(ctx, key, OverrideEdit::Color(pack(rgb)));
                format!(
                    "verb recolor-glyph: {rel} item {item} byte {byte} (row {} col {} {:?}) -> {} (override)",
                    g.row,
                    g.col,
                    g.ch,
                    hex(rgb)
                )
            }
            Verb::RecolorLine(rgb) => {
                let rgb = rgb.unwrap_or(crate::config::settings().verbs.recolor_line);
                if !self.ensure_pick_cache(gid) {
                    return format!("verb recolor-line: {rel} pick cache unavailable");
                }
                let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
                let row = g.row;
                let range = row_byte_range(
                    c.records.iter().zip(&c.leaders).map(|(r, &(b, cp))| (r.row(), b, cp)),
                    row,
                );
                let Some((start, end)) = range else {
                    return format!("verb recolor-line: {rel} item {item} row {row} has no records");
                };
                let visible = self.field.visible().expect("apply_glyph_verb_visible: the field is Visible");
                visible.set_item_span_range(&ctx.queue, item, start, end, pack(rgb));
                format!("verb recolor-line: {rel} item {item} bytes {start}..{end} (row {row}) -> {} (span)", hex(rgb))
            }
            Verb::NudgeGlyph(d) => {
                let ov = self.set_visible_override(ctx, key, OverrideEdit::NudgeX(d[0]));
                if let Some(pg) = self.picked.as_mut().and_then(|h| h.glyph.as_mut()) {
                    pg.pos[0] += d[0];
                }
                let dropped = if d[1] != 0.0 || d[2] != 0.0 {
                    " — y/z nudge not representable (the slot carries x only, as in Derived)"
                } else {
                    ""
                };
                format!("verb nudge-glyph: {rel} item {item} byte {byte} x_nudge -> {:.2} (override){dropped}", ov.x_nudge)
            }
            Verb::ScaleGlyph(f) => format!(
                "verb scale-glyph: {rel} item {item} byte {byte} x{f} not representable in the Visible field \
                 (advance and height come from the atlas table, as in Derived) — ignored"
            ),
            Verb::SetGlyphBackground(bg_rgba) => {
                let mut row = self
                    .groups_cpu
                    .get(gid as usize)
                    .copied()
                    .unwrap_or(crate::glyph_scene::GroupRow::identity([0.0; 3]));
                row.cols[5] = *bg_rgba;
                match self.allocate_group_row(ctx, row) {
                    Some(new_gid) => {
                        self.set_visible_override(ctx, key, OverrideEdit::Group(new_gid));
                        format!("verb set-glyph-background: {rel} item {item} byte {byte} (row {} col {}) -> group {new_gid} (override)", g.row, g.col)
                    }
                    None => format!("verb set-glyph-background: {rel} out of groups (max {})", self.group_buf.size() / 96),
                }
            }
            Verb::SetGlyphTransform(t, q, s) => {
                let mut row = self
                    .groups_cpu
                    .get(gid as usize)
                    .copied()
                    .unwrap_or(crate::glyph_scene::GroupRow::identity([0.0; 3]));
                row.cols[0] = [t[0], t[1], t[2], 0.0];
                row.cols[1] = *q;
                row.cols[3] = [s[0], s[1], s[2], row.cols[3][3]];
                match self.allocate_group_row(ctx, row) {
                    Some(new_gid) => {
                        self.set_visible_override(ctx, key, OverrideEdit::Group(new_gid));
                        format!("verb set-glyph-transform: {rel} item {item} byte {byte} (row {} col {}) -> group {new_gid} (override)", g.row, g.col)
                    }
                    None => format!("verb set-glyph-transform: {rel} out of groups (max {})", self.group_buf.size() / 96),
                }
            }
            Verb::ResetGlyphGroup => {
                self.set_visible_override(ctx, key, OverrideEdit::ClearGroup);
                format!("verb reset-glyph-group: {rel} item {item} byte {byte} -> group {gid} (the item's own)")
            }
            _ => unreachable!("apply_verb routes only the glyph verbs here"),
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

/// M3: the (item, byte) re-keying, held without a device — the selection a
/// pick becomes in the Visible field, the row range `recolor-line` colours,
/// and the override merge every glyph verb goes through.
#[cfg(test)]
mod visible_key_tests {
    use super::*;

    fn glyph(byte_off: usize, ch: char, slot: Option<u32>) -> PickGlyph {
        PickGlyph { record: 0, slot, row: 2, col: 5, line: 2, byte_off, ch, pos: [0.0; 3], advance: 0.5, height: 1.0 }
    }

    #[test]
    fn a_glyph_pick_selects_its_leader_bytes_and_a_file_pick_the_whole_item() {
        // ASCII: one byte. A multi-byte leader: its whole UTF-8 run, so the
        // range's end is where the NEXT leader starts.
        assert_eq!(
            byte_range_selection(3, Some(&glyph(17, 'A', Some(40))), Some(100)),
            Selection::ByteRange { item: 3, start: 17, end: 18 }
        );
        assert_eq!(
            byte_range_selection(3, Some(&glyph(17, '\u{1F600}', Some(40))), Some(100)),
            Selection::ByteRange { item: 3, start: 17, end: 21 }
        );
        // A file-level pick is the whole item; so is a blank glyph (no slot —
        // nothing to light), as the stored modes select the whole segment.
        assert_eq!(byte_range_selection(3, None, Some(100)), Selection::ByteRange { item: 3, start: 0, end: 100 });
        assert_eq!(
            byte_range_selection(3, Some(&glyph(17, ' ', None)), Some(100)),
            Selection::ByteRange { item: 3, start: 0, end: 100 }
        );
        // Unknown length: every leader lies below u32::MAX.
        assert_eq!(byte_range_selection(0, None, None), Selection::ByteRange { item: 0, start: 0, end: u32::MAX });
        assert_eq!(Selection::ByteRange { item: 3, start: 17, end: 21 }.describe(), "3:17..21");
    }

    #[test]
    fn a_row_range_runs_from_its_first_leader_to_its_last_leaders_end() {
        // (row, byte, codepoint): row 1 holds bytes 4..7 ('d', 'é' (2 B), '\n').
        let recs = [
            (0, 0, 'a' as u32),
            (0, 1, 'b' as u32),
            (0, 2, 'c' as u32),
            (0, 3, '\n' as u32),
            (1, 4, 'd' as u32),
            (1, 5, 'é' as u32),
            (1, 7, '\n' as u32),
            (2, 8, 'x' as u32),
        ];
        assert_eq!(row_byte_range(recs.iter().copied(), 1), Some((4, 8)));
        assert_eq!(row_byte_range(recs.iter().copied(), 0), Some((0, 4)));
        assert_eq!(row_byte_range(recs.iter().copied(), 2), Some((8, 9)));
        assert_eq!(row_byte_range(recs.iter().copied(), 7), None);
        // Order-independent: the range is min/max, not first/last seen.
        assert_eq!(row_byte_range(recs.iter().rev().copied(), 1), Some((4, 8)));
    }

    #[test]
    fn override_edits_merge_lane_by_lane_and_clear_to_empty() {
        let c = merged_override(None, 2, 9, OverrideEdit::Color(0xFF00_00FF));
        assert_eq!(c, GlyphOverride { item: 2, byte: 9, color: 0xFF00_00FF, x_nudge: 0.0, group: NO_GROUP });
        // A nudge after a recolour keeps the colour; nudges accumulate.
        let n = merged_override(Some(c), 2, 9, OverrideEdit::NudgeX(0.25));
        let n = merged_override(Some(n), 2, 9, OverrideEdit::NudgeX(0.25));
        assert_eq!((n.color, n.x_nudge, n.group), (0xFF00_00FF, 0.5, NO_GROUP));
        // A group override rides beside both; clearing it leaves them.
        let g = merged_override(Some(n), 2, 9, OverrideEdit::Group(77));
        assert_eq!((g.color, g.x_nudge, g.group), (0xFF00_00FF, 0.5, 77));
        let cleared = merged_override(Some(g), 2, 9, OverrideEdit::ClearGroup);
        assert_eq!((cleared.color, cleared.x_nudge, cleared.group), (0xFF00_00FF, 0.5, NO_GROUP));
        assert!(!override_is_empty(&cleared));
        // Only a group set and then cleared is empty — and gets removed, not kept.
        let only_group = merged_override(None, 2, 9, OverrideEdit::Group(77));
        assert!(!override_is_empty(&only_group));
        assert!(override_is_empty(&merged_override(Some(only_group), 2, 9, OverrideEdit::ClearGroup)));
        // A nudge back to zero is empty too.
        let back = merged_override(Some(merged_override(None, 2, 9, OverrideEdit::NudgeX(1.0))), 2, 9, OverrideEdit::NudgeX(-1.0));
        assert!(override_is_empty(&back));
    }
}
