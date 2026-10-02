use crate::gpu::GpuContext;
use super::instance::RenderSlot;
use super::{GlyphScene, pick::PickFileInfo};

pub enum FileStyle {
    Ok { colored: usize, unstyled: usize },
    VersionMismatch,
    Failed,
}

impl GlyphScene {
    pub fn write_slot_colors(&self, ctx: &GpuContext, slot_base: u32, colors: &[u32]) {
        if colors.is_empty() {
            return;
        }
        if let Some(addr) = self.mapped_slots {
            let ptr = addr as *mut RenderSlot;
            let total = self.instance_count as usize;
            let base = slot_base as usize;
            let count = colors.len().min(total.saturating_sub(base));
            unsafe {
                for (i, &c) in colors.iter().take(count).enumerate() {
                    (*ptr.add(base + i)).color = c;
                }
            }
        } else {
            for (i, &color) in colors.iter().enumerate() {
                let slot = slot_base + i as u32;
                if slot >= self.instance_count {
                    break;
                }
                let chunk = (slot / self.chunk_cap) as usize;
                let local = (slot % self.chunk_cap) as u64;
                let off = self.chunk_offsets[chunk] + local * 32 + 16;
                ctx.queue.write_buffer(&self.instance_bufs[chunk], off, bytemuck::bytes_of(&color));
            }
        }
    }

    /// Recolors a file group in-place given its source bytes and AST/LSP byte spans.
    /// Resolves survivor glyph slots using the provided engine trie and writes colors
    /// directly to the GPU instance buffers.
    /// Returns the number of slots recolored.
    pub fn apply_file_spans(
        &self,
        ctx: &GpuContext,
        group_id: u32,
        file_bytes: &[u8],
        spans: &[crate::layout::ByteSpan],
        trie: &crate::atlas::TrieTable,
    ) -> usize {
        let (slot_base, slot_count) = if let Some(pctx) = &self.pick {
            if let Some(f) = pctx.files.iter().find(|f| f.group_id == group_id) {
                (f.slot_base, f.slot_count)
            } else {
                return 0;
            }
        } else if group_id == 0 {
            (0, self.instance_count)
        } else {
            return 0;
        };

        let colors = crate::layout_hyper::resolve_spans_to_slot_colors(file_bytes, spans, trie);
        let to_write = colors.len().min(slot_count as usize);
        self.write_slot_colors(ctx, slot_base, &colors[..to_write]);
        to_write
    }

    /// Camera eye/target for the mode at time `t` — the SINGLE source both
    /// the render frame and the pick ray derive from, so they always see the
    /// same camera.
    pub fn apply_highlight_sidecar(&self, ctx: &GpuContext, path: &std::path::Path) -> String {
        let map = match parse_highlight_sidecar(path) {
            Ok(map) => map,
            Err(e) => return format!("highlight: {e}"),
        };
        let Some(pctx) = &self.pick else {
            return "highlight: no pick context (repo scenes only) — ignored".to_string();
        };
        let mut files = 0usize;
        let mut colored = 0usize;
        let mut unstyled = 0usize;
        for info in &pctx.files {
            if let Some(runs) = map.get(&info.rel_path) {
                files += 1;
                if let FileStyle::Ok { colored: c, unstyled: u } =
                    self.style_file(ctx, info, runs, None)
                {
                    colored += c;
                    unstyled += u;
                }
            }
        }
        format!(
            "highlight: {} file(s), {} glyphs colored, {} left default — {}",
            files,
            colored,
            unstyled,
            path.display()
        )
    }

    /// P1c — the seam's CONTRACT consumer: apply provider envelopes
    /// (`seam::SurfaceUpdate`) to this scene. For each update whose file is in
    /// the field, the file's bytes are re-derived and hashed; the update is
    /// applied iff `seam::joins(hash, update)` — the version law, made
    /// load-bearing in the renderer. Mismatches are DROPPED and counted, never
    /// translated. Structure/decorations planes are accepted and ignored until
    /// their stages ship (the anti-sprawl law: one walk, fields arrive later).
    pub fn apply_surface_updates(
        &self,
        ctx: &GpuContext,
        updates: &[crate::seam::SurfaceUpdate],
    ) -> String {
        let Some(pctx) = &self.pick else {
            return "seam: no pick context (repo scenes only) — ignored".to_string();
        };
        let mut files = 0usize;
        let mut colored = 0usize;
        let mut unstyled = 0usize;
        let mut dropped = 0usize;
        for update in updates {
            let Some(info) = pctx.files.iter().find(|f| f.rel_path == update.file.0) else {
                dropped += 1; // not in the field (yet) — the workspace grammar's job later
                continue;
            };
            files += 1;
            match self.style_file(ctx, info, &update.style, Some(update.version)) {
                FileStyle::Ok { colored: c, unstyled: u } => {
                    colored += c;
                    unstyled += u;
                }
                FileStyle::VersionMismatch => dropped += 1,
                FileStyle::Failed => {}
            }
        }
        format!(
            "seam: {} file(s), {} glyphs colored, {} left default, {} dropped (version/file) — {} update(s)",
            files,
            colored,
            unstyled,
            dropped,
            updates.len()
        )
    }

    /// Style one file: re-derive its records + fold leaders (ONE rederive
    /// serves both the version hash and the walk), check the version if the
    /// caller has one, then write per-glyph colors — `Verb::RecolorGlyph`'s
    /// 4 B partial-write mechanism, iterated with a merge pointer
    /// (O(runs + records); both walks are byte-ascending). Blank records take
    /// no slot, exactly as `ensure_pick_cache` skips them.
    fn style_file(
        &self,
        ctx: &GpuContext,
        info: &PickFileInfo,
        runs: &[crate::seam::StyleRun],
        expected: Option<crate::seam::BufferVersion>,
    ) -> FileStyle {
        let Some(pctx) = &self.pick else {
            return FileStyle::Failed;
        };
        // Bytes' provenance: envelope-owned content first (the bytes the seam
        // folded), disk second (Stage G's original semantics). One code path
        // either way — only the read differs.
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
                    log::warn!("seam/style: failed to read {}", info.rel_path);
                    return FileStyle::Failed;
                }
            }
        };
        if let Some(expected) = expected {
            // The seam's law (seam::joins is this comparison over a whole
            // envelope): equality or drop. The folded version here is the
            // content hash of the re-derived bytes — the file-driven
            // provider convention (seam::content_hash_version).
            if crate::seam::content_hash_version(&bytes) != expected {
                log::warn!(
                    "seam/style: version mismatch on {} — update dropped, not translated",
                    info.rel_path
                );
                return FileStyle::VersionMismatch;
            }
        }
        // The engine re-run, on the THREAD-CACHED rederiver (repo::
        // rederive_cached — one Engine + trie per thread, outliving scenes:
        // the live loop rebuilds a scene per edit, so a scene-lifetime cache
        // would never amortize). The FFI resets the arena per load ("reuse
        // the arena across loads", ffi.mojo). This is the P1-live
        // measurement's named fix: uncached, this step paid ~120 ms FIXED
        // (Engine::new + trie parse) per file per apply.
        let records = match crate::repo::rederive_cached(&pctx.trie, &bytes, &info.item) {
            Ok(records) => records,
            Err(_) => {
                log::warn!("seam/style: engine re-run failed on {}", info.rel_path);
                return FileStyle::Failed;
            }
        };
        let (leaders, _, _, _) =
            crate::text::fold_leaders(&bytes, info.item.wrap_width, info.item.wrap_mode);
        if leaders.len() != records.len() {
            log::warn!(
                "seam/style: leader/record count mismatch on {} — file skipped",
                info.rel_path
            );
            return FileStyle::Failed;
        }
        // P2a-4 (the FOURTH live-found defect, 2026-09-27): the style walk
        // consumes the COMPACTED stream, not the raw one. Instances are
        // rebuilt from record POSITIONS here — walking raw records repainted
        // every kept glyph at its UNFOLDED y after the loader had collapsed
        // the column (bodies empty, not collapsed: the compaction dropped,
        // the restyle un-shifted). Compaction mirrors the loader's exactly
        // (same folds, same line resolution), so slots, positions and colors
        // come from one truth; kept_ix joins the compacted stream back to
        // the leader bytes for the run lookup.
        let fold_set = pctx.folds.get(&info.rel_path).filter(|f| !f.is_empty());
        let (records, byte_of): (Vec<crate::layout::GlyphRecord>, Vec<usize>) = match fold_set {
            Some(folds) => {
                let starts = crate::repo::line_starts_of(&bytes);
                let lines: Vec<u32> = leaders
                    .iter()
                    .map(|l| crate::repo::line_of_byte(l.0, &starts))
                    .collect();
                let compacted = crate::repo::compact_folds(&records, folds, &lines);
                let bytes_at: Vec<usize> =
                    compacted.kept_ix.iter().map(|&ix| leaders[ix as usize].0).collect();
                (compacted.records, bytes_at)
            }
            None => (records, leaders.iter().map(|l| l.0).collect()),
        };
        // The walk, COALESCED — the RecolorLine lesson: one queue.write_buffer
        // per glyph is ~15 µs of validation each, which is where the uncached
        // style plane's ~130 ms per file actually went (the cached engine was
        // necessary but not sufficient). Full 48 B instances are rebuilt per
        // CONTIGUOUS slot run — few writes per file. A record no run covers
        // is written back at the default color: a re-style after an edit must
        // not leave the previous version's colors on the glyphs between runs.
        let mut batches: Vec<(u32, Vec<RenderSlot>)> = Vec::new();
        let push = |slot: u32, inst: RenderSlot, batches: &mut Vec<(u32, Vec<RenderSlot>)>| {
            match batches.last_mut() {
                Some((start, insts))
                    if *start + insts.len() as u32 == slot
                        && *start / self.chunk_cap == slot / self.chunk_cap =>
                {
                    insts.push(inst);
                }
                _ => batches.push((slot, vec![inst])),
            }
        };
        let mut run_ix = 0usize;
        let mut slot = info.slot_base;
        let mut colored = 0usize;
        let mut unstyled = 0usize;
        for (i, r) in records.iter().enumerate() {
            if r.glyph_id() == 0 {
                continue; // blank: no instance slot, exactly as staging skips
            }
            let byte = byte_of[i];
            while run_ix < runs.len() && runs[run_ix].range.end <= byte {
                run_ix += 1;
            }
            // runs[run_ix] is the first run ending past `byte`; it covers
            // the record iff it also starts at/before it.
            let (packed, hit) = match runs.get(run_ix).filter(|run| byte >= run.range.start) {
                Some(run) => (
                    u32::from(run.rgb[0])
                        | u32::from(run.rgb[1]) << 8
                        | u32::from(run.rgb[2]) << 16
                        | 0xFF00_0000,
                    true,
                ),
                None => (crate::layout::DEFAULT_COLOR_PACKED, false),
            };
            if hit {
                colored += 1;
            } else {
                unstyled += 1;
            }
            let (mut pos, mut advance, mut height) =
                ([r.x(), r.y(), r.z()], r.advance(), r.height());
            // Preserve earlier nudge/scale-glyph edits on this slot.
            if let Some((p, a, h)) = self.geom_overrides.get(&slot) {
                pos = *p;
                advance = *a;
                height = *h;
            }
            push(
                slot,
                RenderSlot {
                    pos,
                    glyph_id: r.glyph_id(),
                    color: packed,
                    group_id: info.group_id,
                    advance,
                    height,
                },
                &mut batches,
            );
            slot += 1;
        }
        for (start, insts) in &batches {
            let chunk = (*start / self.chunk_cap) as usize;
            let local = (*start % self.chunk_cap) as u64;
            let off = self.chunk_off(chunk, local * 32);
            ctx.queue
                .write_buffer(self.chunk_buf(chunk), off, bytemuck::cast_slice(insts));
        }
        FileStyle::Ok { colored, unstyled }
    }
}

fn parse_highlight_sidecar(
    path: &std::path::Path,
) -> Result<std::collections::HashMap<String, Vec<crate::seam::StyleRun>>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut map: std::collections::HashMap<String, Vec<crate::seam::StyleRun>> =
        std::collections::HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches(['\r']);
        if line.is_empty() {
            continue;
        }
        let bad = |what: &str| format!("{}:{}: {what}", path.display(), n + 1);
        let mut parts = line.split('\t');
        let (rel, start, end, rgb) = (
            parts.next().ok_or_else(|| bad("missing rel_path"))?,
            parts.next().ok_or_else(|| bad("missing start"))?,
            parts.next().ok_or_else(|| bad("missing end"))?,
            parts.next().ok_or_else(|| bad("missing color"))?,
        );
        if parts.next().is_some() {
            return Err(bad("extra field"));
        }
        let start: usize = start.trim().parse().map_err(|_| bad("start not a number"))?;
        let end: usize = end.trim().parse().map_err(|_| bad("end not a number"))?;
        let hex = rgb.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("color not rrggbb hex"));
        }
        let rgb = [
            u8::from_str_radix(&hex[0..2], 16).map_err(|_| bad("color not rrggbb hex"))?,
            u8::from_str_radix(&hex[2..4], 16).map_err(|_| bad("color not rrggbb hex"))?,
            u8::from_str_radix(&hex[4..6], 16).map_err(|_| bad("color not rrggbb hex"))?,
        ];
        map.entry(rel.to_string()).or_default().push(crate::seam::StyleRun {
            range: start..end,
            rgb,
        });
    }
    Ok(map)
}

