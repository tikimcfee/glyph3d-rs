use std::path::Path;

use crate::fold::WrapMode;
use crate::gpu::GpuContext;

use super::repo::{ChainMode, InstanceInputs, SharedDevice, run_repo_chain};

pub fn repo_check(ctx: &GpuContext, dir: &Path) -> ! {
    use crate::layout::{LayoutGlyphs as _, VerifyLayout as _};
    let t_all = std::time::Instant::now();
    // The renderer's default shape: wrap BACK, cluster on, the tuned grid
    // pagination from RepoParams::default().
    let params = crate::repo::RepoParams {
        wrap_mode: WrapMode::Back,
        cluster_mode: crate::fold::ClusterMode::Cluster,
        ..Default::default()
    };
    let walk = crate::repo::walk_repo(dir);
    let file_params: Vec<crate::layout::ItemParams> = walk
        .files
        .iter()
        .map(|f| {
            let newlines = f.bytes.iter().filter(|&&b| b == b'\n').count();
            crate::repo::file_item_params(&params, f.bytes.len(), newlines)
        })
        .collect();
    let item_count = walk.files.len();
    let n: usize = walk.files.iter().map(|f| f.bytes.len()).sum();
    let mut bytes = Vec::with_capacity(n);
    let mut fis = Vec::with_capacity(item_count);
    let mut off = 0usize;
    for (i, f) in walk.files.iter().enumerate() {
        let p = &file_params[i];
        bytes.extend_from_slice(&f.bytes);
        fis.push(crate::fold::Item {
            byte_start: off as i64,
            byte_count: f.bytes.len() as i64,
            origin_x: p.origin_x,
            origin_y: p.origin_y,
            origin_z: p.origin_z,
            wrap_width: p.wrap_width as i64,
            wrap_mode: p.wrap_mode,
            cluster_mode: p.cluster_mode,
            z_step: p.z_step,
            line_height: p.line_height,
            has_page: p.has_page,
            page_rows: p.page_rows as i64,
            page_cols: p.page_cols as i64,
            scroll_rows: p.scroll_rows as i64,
            pages_wide: p.pages_wide as i64,
            page_gap_x: p.page_gap_x,
            band_stride_y: p.band_stride_y,
            depth_per_band: p.depth_per_band,
            depth_per_col: p.depth_per_col,
            page_line_height: p.page_line_height,
        });
        off += f.bytes.len();
    }

    // ── the chain side — THE load path, shared with CubeclLayout ────────
    // BOTH tails: the fence sees the product's instance/placement output
    // AND the record tier against the same dispatches. The paint tables are
    // the SAME colorize_leaders output the engine side paints with, so the
    // instance tier compares like against like. (Peak-memory note: Both
    // holds four streams at the big-corpus shape — records and instances,
    // engine and chain. The fork gate's standing fixture is small by
    // design; a manual big-corpus run that brushes the machine ceiling can
    // set GLYPH_REPO_CHECK_TAIL=records to drop the instance tier.)
    let colors: Vec<Vec<u32>> = walk
        .files
        .iter()
        .map(|f| crate::text::colorize_leaders(&f.bytes))
        .collect();
    let mut per_record_colors: Vec<u32> = Vec::new();
    let mut color_base = vec![0u32; item_count];
    let mut groups = Vec::with_capacity(item_count);
    for (index, c) in colors.iter().enumerate() {
        color_base[index] = per_record_colors.len() as u32;
        per_record_colors.extend_from_slice(c);
        groups.push(index as u32);
    }
    let inputs = InstanceInputs {
        per_record_colors,
        color_base,
        is_per_record: vec![1u32; item_count],
        flat_colors: vec![0u32; item_count],
        groups,
    };
    let mode = if std::env::var("GLYPH_REPO_CHECK_TAIL").as_deref() == Ok("records") {
        ChainMode::Records
    } else {
        ChainMode::Both
    };
    let device = SharedDevice::from_ctx(ctx);
    // The endpoint (note 23, E2b): the chain's slots live on device in the
    // renderer-bound buffer; the lane tier reads them back host-side and
    // compares field-wise against the engine's arena. There is no
    // chain-side arena to hand off — that machinery (the pack windows, the
    // hops, the mapped target) retired with the hop it served.
    let stream = run_repo_chain(
        Some(&device),
        &bytes,
        &fis,
        &inputs,
        mode,
    );
    let recs_all = stream.records;
    let total_records = stream.total_records;
    let c = stream.candidates;
    let chain_dt = stream.chain_dur;
    let readback_dt = stream.readback_dur;
    let phases = stream.phases;
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        println!("  dbg recs[0..16] = {:?}", &recs_all[..16]);
    }
    let recs: &[u32] = &recs_all;

    // ── the engine side, SECOND — records for the same items, run after the
    // GPU work so its host-side record stream never overlaps the chain's
    // dispatches (see the note above the leader scan). The arena and the
    // placements stay LIVE past the diff now: the instance and placement
    // tiers compare against them (the arena IS the engine's instance
    // output; the records tier only needs `engine_records` and runs after
    // the instance tiers have dropped the arena).
    let t_eng = std::time::Instant::now();
    let mut arena = crate::layout::GlyphArena::new();
    let mut backend = crate::layout_mojo::MojoLayout::new(crate::layout_mojo::Strategy::Batched);
    backend
        .load_trie_file(&crate::default_engine_trie())
        .expect("engine trie");
    let eng_items: Vec<crate::layout::LayoutItem<'_>> = walk
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| crate::layout::LayoutItem {
            bytes: &f.bytes,
            params: file_params[index],
            group_id: index as u32,
            paint: crate::layout::Paint::PerRecord(&colors[index]),
        })
        .collect();
    let (placements, engine_records) = backend
        .layout_items_recording(&eng_items, &mut arena)
        .expect("engine layout failed");
    let eng_dt = t_eng.elapsed();
    let engine_total: u32 = placements.iter().map(|p| p.record_count).sum();
    // The owning item of every record, from the ENGINE's own placement
    // counts — the fork census below needs each record's page geometry to
    // attribute a deviation to the arithmetic that produced it.
    let mut item_of: Vec<u32> = Vec::with_capacity(engine_total as usize);
    for (idx, p) in placements.iter().enumerate() {
        item_of.extend(std::iter::repeat_n(idx as u32, p.record_count as usize));
    }
    drop(eng_items);

    // Empty-corpus refusal, always on — the repo-verify-direct lesson:
    // a PASS over zero items compared nothing.
    if item_count == 0 || total_records == 0 {
        eprintln!(
            "cubecl-repo-check FAIL: refusing to verify an empty corpus ({item_count} items, {total_records} records)"
        );
        std::process::exit(1);
    }

    // ── the instance and placement tiers (rung 5b) ────────────────────────
    // The PRODUCT tail's own claims: the packed slots byte-equal against
    // the engine-batched arena (the same comparison repo-verify makes of
    // the host backends), and the placements bit-equal — which fences the
    // pack kernel's extent folds (order-free atomics, but the seeds and
    // the lane arithmetic must match compact_records_into exactly).
    let mut inst_bad = 0usize;
    let mut place_bad = 0usize;
    let mut tint_bad = 0usize;
    if mode != ChainMode::Records {
        // ── THE instance tier (note 23, E2b): the endpoint's 32 B slot
        // stream vs the engine's 48 B arena, FIELD-wise. The slot word map
        // drops row/col (records-fenced bit-exact below), flags and _pad:
        // slot [0..3] pos, [3] gi, [4] color, [5] group, [6] adv,
        // [7] height against engine [0..3], [3], [6], [7], [8], [9]. The
        // slots are the renderer-bound bytes — this is the fence on the
        // exact form the shader reads.
        let eng_words: &[u32] = bytemuck::cast_slice(arena.instances());
        if !stream.slots.is_empty() {
            const MAP: [usize; 8] = [0, 1, 2, 3, 6, 7, 8, 9];
            if stream.slots.len() * 3 != eng_words.len() * 2 {
                inst_bad += 1;
                println!(
                    "  LANE LENGTH MISMATCH: engine {} slots, scatter {} slots",
                    eng_words.len() / 12,
                    stream.slots.len() / 8
                );
            } else {
                for s in 0..stream.slots.len() / 8 {
                    for (f, &e) in MAP.iter().enumerate() {
                        let a = eng_words[s * 12 + e];
                        let b = stream.slots[s * 8 + f];
                        if a != b {
                            inst_bad += 1;
                            if inst_bad <= 4 {
                                println!(
                                    "  LANE MISMATCH slot {s} field {f}: scatter {b:#x} engine {a:#x}"
                                );
                            }
                        }
                    }
                }
            }
        }
        // ── the tint tier (E2b): the tint stream is seg_tint's ONLY input
        // on the endpoint path, and NO golden view can see it — the
        // cameras keep all five fixture files near enough that the far-LOD
        // backdrop substitution (the tint's only pixel reader) never fires
        // (proven the hard way: scatter-tint-lane-dropped stayed GREEN
        // under pixel-ab). So the fence is BYTE-level self-consistency
        // against the engine-fenced slot stream: tint[2s] == slot gi,
        // tint[2s+1] == slot color. Transitively engine-fenced, by the
        // tier above.
        if !stream.slots.is_empty() {
            let tint = stream.tint.as_slice();
            if tint.len() * 4 != stream.slots.len() {
                tint_bad += 1;
                println!(
                    "  TINT LENGTH MISMATCH: {} slots, {} tint words",
                    stream.slots.len() / 8,
                    tint.len() / 2
                );
            } else {
                for s in 0..stream.slots.len() / 8 {
                    let (tg, tc) = (tint[s * 2], tint[s * 2 + 1]);
                    let (sg, sc) = (stream.slots[s * 8 + 3], stream.slots[s * 8 + 4]);
                    if tg != sg || tc != sc {
                        tint_bad += 1;
                        if tint_bad <= 4 {
                            println!(
                                "  TINT MISMATCH slot {s}: stream ({tg:#x}, {tc:#x}) vs slot ({sg:#x}, {sc:#x})"
                            );
                        }
                    }
                }
            }
        }
        for (idx, (gp, cp)) in placements.iter().zip(stream.placements.iter()).enumerate() {
            if !gp.bit_eq(cp) {
                place_bad += 1;
                if place_bad <= 4 {
                    println!(
                        "  PLACEMENT MISMATCH item {}: engine (base {} cnt {} rec {} right {:e} bottom {:e}) chain (base {} cnt {} rec {} right {:e} bottom {:e})",
                        idx,
                        gp.slot_base, gp.slot_count, gp.record_count, gp.page.right, gp.page.bottom,
                        cp.slot_base, cp.slot_count, cp.record_count, cp.page.right, cp.page.bottom
                    );
                }
            }
        }
        drop(arena);
        drop(stream.placements);
    }

    // ── the diff ──────────────────────────────────────────────────────────
    // The FORK CENSUS: bit-deviations bucketed BY LANE and by the integer
    // context that produced them — X's page multiplier m (the paginate
    // stride product), Z's wrap segment (the base-Z product), Y's row
    // magnitude (the base-Y product), and X at m == 0 (the line_adv scan
    // tree, which paginate never touches). The one aggregate number that
    // stood here could not tell those classes apart, and the rung-4
    // arithmetic-fork decision turns on exactly this decomposition: each
    // class has a different fix, and one of them (the scan tree) is not
    // fixable in paginate at all.
    let mut bad = 0usize;
    let mut bit_devs = 0usize;
    let mut max_dev = 0.0f64;
    let mut lane_devs = [0usize; 5];
    let mut lane_far = [0usize; 5]; // deviations farther than 1 ulp
    let mut lane_max = [0.0f64; 5];
    let mut x_m = [0usize; 4]; // X deviations at m == 0, 1, 2, >= 3
    let mut z_seg = [0usize; 4]; // Z deviations at segment 0, 1, 2, >= 3
    let mut y_big_row = 0usize; // Y deviations at row > 2048
    let total = total_records as usize;
    // STRICT mode (the fork gate): the claim is BIT-exactness, not the
    // eps tier — any measure-word deviation fails — and the census's
    // m>=3 / seg>=3 buckets must be proven EXERCISED by this corpus, so
    // the gate cannot quietly hollow the way /tmp scratch corpora would.
    let strict = std::env::var_os("GLYPH_REPO_CHECK_STRICT").is_some();
    let mut m3_records = 0usize;
    let mut seg3_records = 0usize;
    let mut shown = 0usize;
    for (o, want) in engine_records.iter().take(total).enumerate() {
        let w = o * 8;
        let got_gi = recs[w + 5];
        let got_row = recs[w + 6];
        let got_col = recs[w + 7];
        let mut ok = got_gi == want.counts[0] && got_row == want.counts[1] && got_col == want.counts[2];
        // The record's paginate context, off the engine-side item params.
        // Computed lazily for the census, unconditionally for the strict
        // denominators (the exercise proof).
        let mut ctx: Option<(i64, i64)> = None;
        if strict && o < item_of.len() {
            let prm = &file_params[item_of[o] as usize];
            let rows_s = if prm.has_page { prm.page_rows as i64 } else { 0 };
            let scroll_s = if prm.has_page { prm.scroll_rows as i64 } else { 0 };
            let wide_s = prm.pages_wide.max(1) as i64;
            let screen_row_s = got_row as i64 - scroll_s;
            let y_page_s = if rows_s > 0 && screen_row_s >= rows_s {
                screen_row_s / rows_s
            } else {
                0
            };
            if y_page_s % wide_s >= 3 {
                m3_records += 1;
            }
            if prm.wrap_width > 0 && (got_col as i64 / prm.wrap_width as i64) >= 3 {
                seg3_records += 1;
            }
        }
        for k in 0..5 {
            let got = f32::from_bits(recs[w + k]);
            let wantm = want.measures[k];
            if got.to_bits() == wantm.to_bits() {
                continue;
            }
            bit_devs += 1;
            lane_devs[k] += 1;
            if shown < 4 && std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                println!(
                    "  dbg dev record {o} lane {k}: chain {:e} ({:#x}) engine {:e} ({:#x}) row {} col {}",
                    got,
                    recs[w + k],
                    wantm,
                    wantm.to_bits(),
                    got_row,
                    got_col
                );
                shown += 1;
            }
            let rel = (got as f64 - wantm as f64).abs() / (wantm as f64).abs().max(1.0);
            if rel > max_dev {
                max_dev = rel;
            }
            if rel > lane_max[k] {
                lane_max[k] = rel;
            }
            // Same-sign f32s order by bit pattern, so the bit distance IS
            // the ulp distance; a cross-sign pair lands absurdly far and
            // counts as far, which is the right verdict for a position.
            let bd = (recs[w + k] as i64)
                .wrapping_sub(wantm.to_bits() as i64)
                .abs();
            if bd > 1 {
                lane_far[k] += 1;
            }
            if k < 3 && ctx.is_none() && o < item_of.len() {
                let prm = &file_params[item_of[o] as usize];
                let rows = if prm.has_page { prm.page_rows as i64 } else { 0 };
                let scroll = if prm.has_page { prm.scroll_rows as i64 } else { 0 };
                let wide = prm.pages_wide.max(1) as i64;
                let screen_row = got_row as i64 - scroll;
                let y_page = if rows > 0 && screen_row >= rows {
                    screen_row / rows
                } else {
                    0
                };
                let wrap_segment = if prm.wrap_width > 0 {
                    got_col as i64 / prm.wrap_width as i64
                } else {
                    0
                };
                ctx = Some((y_page % wide, wrap_segment));
            }
            match k {
                0 => x_m[ctx.map(|(page_col, _)| page_col).unwrap_or(0).min(3) as usize] += 1,
                1 => {
                    if got_row > 2048 {
                        y_big_row += 1;
                    }
                }
                2 => z_seg[ctx.map(|(_, seg_idx)| seg_idx).unwrap_or(0).min(3) as usize] += 1,
                _ => {}
            }
            if rel > 1e-4 {
                ok = false;
            }
        }
        if !ok {
            if bad < 8 {
                println!(
                    "  MISMATCH record {o}: gi {} vs {} row {} vs {} col {} vs {} | x {:e} vs {:e} y {:e} vs {:e} z {:e} vs {:e} adv {:e} vs {:e} hgt {:e} vs {:e}",
                    got_gi, want.counts[0], got_row, want.counts[1], got_col, want.counts[2],
                    f32::from_bits(recs[w]), want.measures[0],
                    f32::from_bits(recs[w + 1]), want.measures[1],
                    f32::from_bits(recs[w + 2]), want.measures[2],
                    f32::from_bits(recs[w + 3]), want.measures[3],
                    f32::from_bits(recs[w + 4]), want.measures[4]
                );
            }
            bad += 1;
        }
    }
    let count_ok = engine_records.len() == total_records as usize
        && engine_total == total_records;
    println!(
        "cubecl-repo-check: {} ({} files, {} B, {} records, {} candidates) — engine {:?} | chain+readback {:?} (readback {:?}) | counts {} — {} record mismatches, {} measure bit-deviations, max {:.2e} (total {:?})",
        dir.display(),
        item_count,
        n,
        total,
        c,
        eng_dt,
        chain_dt,
        readback_dt,
        if count_ok { "MATCH" } else { "DIFFER" },
        bad,
        bit_devs,
        max_dev,
        t_all.elapsed()
    );
    println!(
        "  chain spans: prep {:?} | tables {:?} | init {:?} | pack+upload {:?} | dispatch {:?} (wall; cold-process JIT hides in dispatch)",
        phases.prep, phases.tables, phases.init, phases.upload, phases.dispatch
    );
    if inst_bad > 0 || place_bad > 0 || tint_bad > 0 {
        eprintln!(
            "cubecl-repo-check FAIL: {inst_bad} lane mismatch words, {place_bad} placement mismatches, {tint_bad} tint mismatches"
        );
        std::process::exit(1);
    }
    if bad > 0 || !count_ok || max_dev > 1e-4 {
        eprintln!(
            "cubecl-repo-check FAIL: {bad} record mismatches, counts {}, max deviation {max_dev:.2e}",
            if count_ok { "MATCH" } else { "DIFFER" }
        );
        std::process::exit(1);
    }
    if strict {
        if bit_devs > 0 {
            eprintln!(
                "cubecl-repo-check FAIL (strict): {bit_devs} measure bit-deviations — the fork gate claims bit-exactness"
            );
            std::process::exit(1);
        }
        if m3_records == 0 || seg3_records == 0 {
            eprintln!(
                "cubecl-repo-check FAIL (strict): census not exercised — {m3_records} records at m >= 3, {seg3_records} at segment >= 3; the corpus cannot see the fork classes it exists to fence"
            );
            std::process::exit(1);
        }
        println!(
            "strict: bit-exact across all lanes; {m3_records} records at m >= 3, {seg3_records} at segment >= 3 (exercised)"
        );
    }
    // The census line prints on PASS too — it is the instrument that prices
    // the rung-4 fork, and a zero-deviation run is its most important datum.
    println!(
        "cubecl-repo-check census: \
         X {} (max {:.2e}, {} >1ulp; m0 {} m1 {} m2 {} m3+ {}) | \
         Y {} (max {:.2e}, {} >1ulp, {} at row>2048) | \
         Z {} (max {:.2e}, {} >1ulp; seg0 {} seg1 {} seg2 {} seg3+ {}) | \
         adv {} | hgt {}",
        lane_devs[0],
        lane_max[0],
        lane_far[0],
        x_m[0],
        x_m[1],
        x_m[2],
        x_m[3],
        lane_devs[1],
        lane_max[1],
        lane_far[1],
        y_big_row,
        lane_devs[2],
        lane_max[2],
        lane_far[2],
        z_seg[0],
        z_seg[1],
        z_seg[2],
        z_seg[3],
        lane_devs[3],
        lane_devs[4],
    );
    println!(
        "cubecl-repo-check PASS: glyph_id/row/col exact, measures inside 1e-4 ({} of {} measure words carry a last-bit f32 deviation — the documented reassociation tier)",
        bit_devs,
        total * 5
    );
    if mode != ChainMode::Records {
        println!(
            "instance tier: {} slots field-equal vs the engine arena (the endpoint's 32 B form, note 23), {} placements bit-equal, tint stream consistent",
            stream.slots.len() / 8,
            item_count
        );
    }
    std::process::exit(0);
}
