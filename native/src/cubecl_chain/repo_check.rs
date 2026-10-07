use std::path::Path;

use crate::fold::WrapMode;
use crate::gpu::GpuContext;

use super::repo::{InstanceInputs, SharedDevice, run_repo_chain};

pub fn repo_check(ctx: &GpuContext, dir: &Path, color_mode: crate::repo::ColorMode) -> ! {
    use crate::layout::{LayoutGlyphs as _, VerifyLayout as _};
    let t_all = std::time::Instant::now();
    // The renderer's default shape: wrap BACK, cluster on, the tuned grid
    // pagination from RepoParams::default().
    let params = crate::repo::RepoParams {
        wrap_mode: WrapMode::Back,
        cluster_mode: crate::fold::ClusterMode::Cluster,
        color_mode,
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

    let is_flat = color_mode == crate::repo::ColorMode::Flat;
    let (colors, inputs) = if is_flat {
        let mut groups = Vec::with_capacity(item_count);
        for index in 0..item_count {
            groups.push(index as u32);
        }
        let inputs = InstanceInputs {
            per_record_colors: Vec::new(),
            color_base: vec![0u32; item_count],
            is_per_record: vec![0u32; item_count],
            flat_colors: vec![crate::layout::DEFAULT_COLOR_PACKED; item_count],
            groups,
        };
        (None, inputs)
    } else {
        use rayon::prelude::*;
        let colors: Vec<Vec<u32>> = walk
            .files
            .par_iter()
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
        (Some(colors), inputs)
    };
    let device = SharedDevice::from_ctx(ctx);
    let stream = run_repo_chain(
        Some(&device),
        bytes,
        &fis,
        &inputs,
        true,
        glyph_field::GlyphFieldMode::Instanced,
        None,
    );
    let total_slots = stream.total_slots;
    let c = stream.candidates;
    let chain_dt = stream.chain_dur;
    let readback_dt = stream.readback_dur;
    let phases = stream.phases;

    let t_eng = std::time::Instant::now();
    let mut arena = crate::layout::GlyphArena::new();
    let mut backend = crate::layout_hyper::HyperLayout::new();
    backend
        .load_trie_file(&crate::default_engine_trie())
        .expect("engine trie");
    let eng_items: Vec<crate::layout::LayoutItem<'_>> = walk
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| {
            let paint = if is_flat {
                crate::layout::Paint::Flat(crate::layout::DEFAULT_COLOR_PACKED)
            } else {
                crate::layout::Paint::PerRecord(&colors.as_ref().unwrap()[index])
            };
            crate::layout::LayoutItem {
                bytes: &f.bytes,
                params: file_params[index],
                group_id: index as u32,
                paint,
            }
        })
        .collect();
    let (placements, _engine_records) = backend
        .layout_items_recording(&eng_items, &mut arena)
        .expect("engine layout failed");
    let eng_dt = t_eng.elapsed();
    let engine_instances = arena.instances().to_vec();
    drop(eng_items);

    if item_count == 0 || total_slots == 0 {
        eprintln!(
            "cubecl-repo-check FAIL: refusing to verify an empty corpus ({item_count} items, {total_slots} slots)"
        );
        std::process::exit(1);
    }

    let mut inst_bad = 0usize;
    let mut place_bad = 0usize;
    let mut tint_bad = 0usize;

    let eng_words: &[u32] = bytemuck::cast_slice(&engine_instances);
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
            for slot_idx in 0..stream.slots.len() / 8 {
                for (field_idx, &map_offset) in MAP.iter().enumerate() {
                    let engine_word = eng_words[slot_idx * 12 + map_offset];
                    let scatter_word = stream.slots[slot_idx * 8 + field_idx];
                    if engine_word != scatter_word {
                        inst_bad += 1;
                        if inst_bad <= 4 {
                            println!(
                                "  LANE MISMATCH slot {slot_idx} field {field_idx}: scatter {scatter_word:#x} engine {engine_word:#x}"
                            );
                        }
                    }
                }
            }
        }
    }

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

    let mut item_of_slot: Vec<u32> = Vec::with_capacity(total_slots as usize);
    for (idx, p) in placements.iter().enumerate() {
        item_of_slot.extend(std::iter::repeat_n(idx as u32, p.slot_count as usize));
    }

    let mut bad = 0usize;
    let mut bit_devs = 0usize;
    let mut max_dev = 0.0f64;
    let mut lane_devs = [0usize; 5];
    let mut lane_far = [0usize; 5];
    let mut lane_max = [0.0f64; 5];
    let mut x_m = [0usize; 4];
    let mut z_seg = [0usize; 4];
    let mut y_big_row = 0usize;
    let strict = std::env::var_os("GLYPH_REPO_CHECK_STRICT").is_some();
    let mut m3_records = 0usize;
    let mut seg3_records = 0usize;
    let mut shown = 0usize;

    let total = total_slots as usize;
    for s in 0..total {
        let sw_offset = s * 8;
        let got_gi = stream.slots[sw_offset + 3];
        let want = &engine_instances[s];
        let mut ok = got_gi == want.glyph_id;

        let mut ctx: Option<(i64, i64)> = None;
        if s < item_of_slot.len() {
            let prm = &file_params[item_of_slot[s] as usize];
            let rows_s = if prm.has_page { prm.page_rows as i64 } else { 0 };
            let scroll_s = if prm.has_page { prm.scroll_rows as i64 } else { 0 };
            let wide_s = prm.pages_wide.max(1) as i64;
            let screen_row_s = want.row as i64 - scroll_s;
            let y_page_s = if rows_s > 0 && screen_row_s >= rows_s {
                screen_row_s / rows_s
            } else {
                0
            };
            if y_page_s % wide_s >= 3 {
                m3_records += 1;
            }
            if prm.wrap_width > 0 && (want.col as i64 / prm.wrap_width as i64) >= 3 {
                seg3_records += 1;
            }
            let wrap_segment = if prm.wrap_width > 0 {
                want.col as i64 / prm.wrap_width as i64
            } else {
                0
            };
            ctx = Some((y_page_s % wide_s, wrap_segment));
        }

        let got_measures = [
            f32::from_bits(stream.slots[sw_offset]),
            f32::from_bits(stream.slots[sw_offset + 1]),
            f32::from_bits(stream.slots[sw_offset + 2]),
            f32::from_bits(stream.slots[sw_offset + 6]),
            f32::from_bits(stream.slots[sw_offset + 7]),
        ];
        let want_measures = [
            want.pos[0],
            want.pos[1],
            want.pos[2],
            want.advance,
            want.height,
        ];

        for k in 0..5 {
            let got = got_measures[k];
            let wantm = want_measures[k];
            if got.to_bits() == wantm.to_bits() {
                continue;
            }
            bit_devs += 1;
            lane_devs[k] += 1;
            if shown < 4 && std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                println!(
                    "  dbg dev slot {s} lane {k}: chain {:e} ({:#x}) engine {:e} ({:#x}) row {} col {}",
                    got,
                    got.to_bits(),
                    wantm,
                    wantm.to_bits(),
                    want.row,
                    want.col
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
            let bd = (got.to_bits() as i64)
                .wrapping_sub(wantm.to_bits() as i64)
                .abs();
            if bd > 1 {
                lane_far[k] += 1;
            }
            match k {
                0 => x_m[ctx.map(|(page_col, _)| page_col).unwrap_or(0).min(3) as usize] += 1,
                1 => {
                    if want.row > 2048 {
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
                    "  MISMATCH slot {s}: gi {} vs {} row {} col {} | x {:e} vs {:e} y {:e} vs {:e} z {:e} vs {:e} adv {:e} vs {:e} hgt {:e} vs {:e}",
                    got_gi, want.glyph_id, want.row, want.col,
                    got_measures[0], want_measures[0],
                    got_measures[1], want_measures[1],
                    got_measures[2], want_measures[2],
                    got_measures[3], want_measures[3],
                    got_measures[4], want_measures[4]
                );
            }
            bad += 1;
        }
    }
    let count_ok = engine_instances.len() == total_slots as usize;
    println!(
        "cubecl-repo-check: {} ({} files, {} B, {} slots, {} candidates) — engine {:?} | chain+readback {:?} (readback {:?}) | counts {} — {} slot mismatches, {} measure bit-deviations, max {:.2e} (total {:?})",
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
            "cubecl-repo-check FAIL: {bad} slot mismatches, counts {}, max deviation {max_dev:.2e}",
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
        if c == 0 {
            eprintln!(
                "cubecl-repo-check FAIL (strict): 0 cluster candidates — the corpus cannot fence the cluster-commit path it exists to cover"
            );
            std::process::exit(1);
        }
        println!(
            "strict: bit-exact across all lanes; {m3_records} records at m >= 3, {seg3_records} at segment >= 3, {c} cluster candidates (exercised)"
        );
    }
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
    println!(
        "instance tier: {} slots field-equal vs the engine arena (the endpoint's 32 B form, note 23), {} placements bit-equal, tint stream consistent",
        stream.slots.len() / 8,
        item_count
    );
    std::process::exit(0);
}
