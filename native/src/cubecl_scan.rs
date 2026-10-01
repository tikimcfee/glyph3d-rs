//! CubeCL scan bring-up — dev-only (`--cubecl-scan-check`), the note-16 phase 1.
//!
//! The first REAL kernel of the spike: `leaf_of` + the monoid `combine` +
//! `p_store` as one CubeCL `chunk_reduce` (thread per 64-byte chunk), with
//! the chunk partials diffed BIT-EXACT against the in-process CPU reference
//! — `scan.rs`'s `scan_leaf_value`/`scan_combine`, the same monoid already
//! proven against the JS oracle. No Mojo in the loop: the validation chain
//! here is fixtures -> scan.rs (Rust, CPU) -> this kernel (Rust, GPU).
//!
//! Scope, deliberately: single-item fixtures, decode on CPU (the bench's
//! "mode 0" shape). A HISTORICAL instrument: the phase-1 first kernel,
//! kept for its bit-exact chunk-partial witness — the full chain
//! (multi-item, spine, device decode, cluster) lives in `cubecl_chain/`.
//! Not wired into the battery: it touches a GPU.

use std::path::Path;

use cubecl::prelude::*;
use cubecl::wgpu::{AutoGraphicsApi, GraphicsApi, WgpuSetup};

use crate::fold::{run_pipeline, WrapMode};
use crate::gpu::GpuContext;
use crate::scan::{DEFAULT_CHUNK_SIZE, scan_combine, scan_identity, scan_leaf_value};

// The schema's partial-lane layout (glyph-identity.json, hash-pinned).
const PARTIAL_COUNT_STRIDE: usize = 8;
const P_RESET: usize = 0;
const P_NL: usize = 1;
const P_GLYPHS: usize = 2;
const P_ROWS: usize = 3;
const P_HEAD_LEN: usize = 4;
const P_TAIL_LEN: usize = 5;
const P_WRAP: usize = 6;
const P_MODE: usize = 7;
const SM_STRIDE: usize = 2;
const SM_ADVANCE: usize = 0;

const F_LEADER: u32 = 1;
const F_NEWLINE: u32 = 4;
const WRAP_BACK: i32 = 1;

/// Thread per chunk: the serial combine of one-byte leaves over the chunk,
/// partial lanes out. A line-for-line transcription of `leaf_of` +
/// `scan_combine` (leaf-specialized exactly like the Mojo device twin's —
/// chunk reduce only ever combines LEAVES, so b.rows is always 0).
#[cube(launch_unchecked)]
fn chunk_reduce(
    fl: &[u32],
    sm: &[f32],
    pc: &mut [u32],
    pm: &mut [f32],
    wrap: i32,
    mode: i32,
    #[comptime] chunk: usize,
) {
    let c = ABSOLUTE_POS;
    let n = fl.len();
    let lo = c * chunk;
    if lo < n {
        let hi = if lo + chunk < n { lo + chunk } else { n };
        // The identity accumulator.
        let mut a_reset = 0i32;
        let mut a_nl = 0i32;
        let mut a_glyphs = 0i32;
        let mut a_rows = 0i32;
        let mut a_head = 0i32;
        let mut a_tail = 0i32;
        let mut a_wrap = 0i32;
        let mut a_mode = 0i32;
        let mut a_adv = 0.0f32;
        for i in lo..hi {
            // leaf_of: reset/wrap/mode always; the rest only for leaders.
            let f = fl[i];
            let mut l_nl = 0i32;
            let mut l_glyphs = 0i32;
            let mut l_head = 0i32;
            let mut l_tail = 0i32;
            let mut l_adv = 0.0f32;
            if (f & F_LEADER) != 0 {
                l_glyphs = 1;
                if (f & F_NEWLINE) != 0 {
                    l_nl = 1;
                } else {
                    l_head = 1;
                    l_tail = 1;
                    l_adv = sm[i * SM_STRIDE + SM_ADVANCE];
                }
            }
            // combine(acc, leaf) — transcribed line-for-line from scan.rs.
            if i == 0 {
                // b.reset absorbs: combine(a, b) == b.
                a_reset = 1;
                a_nl = l_nl;
                a_glyphs = l_glyphs;
                a_rows = 0;
                a_head = l_head;
                a_tail = l_tail;
                a_adv = l_adv;
                a_wrap = wrap;
                a_mode = mode;
            } else {
                a_wrap = wrap;
                a_mode = mode;
                if l_nl == 0 {
                    a_tail += l_tail;
                    a_adv += l_adv; // f32 per add — the oracle's chain
                    if a_nl == 0 {
                        a_head = a_tail;
                    }
                } else {
                    if a_nl == 0 {
                        a_head += l_head;
                        a_rows = 0; // a leaf's rows
                    } else {
                        // The junction line, closed by this newline.
                        let len = a_tail + l_head;
                        if mode != WRAP_BACK && wrap > 0 && len > 0 {
                            a_rows += (len - 1) / wrap + 1;
                        } else {
                            a_rows += 1;
                        }
                    }
                    a_tail = l_tail;
                    a_adv = l_adv;
                }
                a_nl += l_nl;
                a_glyphs += l_glyphs;
            }
        }
        // p_store, the schema's lane order.
        let o = c * PARTIAL_COUNT_STRIDE;
        pc[o + P_RESET] = a_reset as u32;
        pc[o + P_NL] = a_nl as u32;
        pc[o + P_GLYPHS] = a_glyphs as u32;
        pc[o + P_ROWS] = a_rows as u32;
        pc[o + P_HEAD_LEN] = a_head as u32;
        pc[o + P_TAIL_LEN] = a_tail as u32;
        pc[o + P_WRAP] = a_wrap as u32;
        pc[o + P_MODE] = a_mode as u32;
        pm[c] = a_adv;
    }
}

pub fn run(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-scan-check: {e}");
        std::process::exit(1);
    });
    assert_eq!(
        fx.items.len(),
        1,
        "cubecl-scan-check is single-item in phase 1 ({} has {})",
        fx.name,
        fx.items.len()
    );
    let item = &fx.items[0];
    let wrap = item.wrap_width;
    let wrap_i32 = i32::try_from(wrap).expect("wrap width fits i32");
    let mode_i32 = match item.wrap_mode {
        WrapMode::Down => 0,
        WrapMode::Back => 1,
    };

    // Decode on CPU — the same static lanes the bench's mode 0 uploads.
    let r = run_pipeline(&fx.bytes, &fx.trie, &fx.items);
    let n = fx.bytes.len();
    let chunk = DEFAULT_CHUNK_SIZE;
    let n_chunks = n.div_ceil(chunk);

    // The reference partials: scan.rs's own monoid, per chunk.
    let mut expected = Vec::with_capacity(n_chunks);
    for c in 0..n_chunks {
        let lo = c * chunk;
        let hi = (lo + chunk).min(n);
        let mut acc = scan_identity();
        for i in lo..hi {
            let flags = r.slots.flags(i);
            let leaf = scan_leaf_value(
                flags & F_NEWLINE != 0,
                r.slots.advance(i),
                flags & F_LEADER != 0,
                wrap,
                i == 0,
                item.wrap_mode,
            );
            scan_combine(&mut acc, &leaf);
        }
        expected.push(acc);
    }

    // The device side.
    let mut fl = Vec::with_capacity(n);
    let mut sm = Vec::with_capacity(n * SM_STRIDE);
    for i in 0..n {
        fl.push(r.slots.flags(i));
        sm.push(r.slots.advance(i));
        sm.push(r.slots.height(i));
    }
    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let h_fl = client.create_from_slice(bytemuck::cast_slice(&fl));
    let h_sm = client.create_from_slice(bytemuck::cast_slice(&sm));
    let h_pc = client.empty(n_chunks * PARTIAL_COUNT_STRIDE * size_of::<u32>());
    let h_pm = client.empty(n_chunks * size_of::<f32>());
    let t0 = std::time::Instant::now();
    unsafe {
        chunk_reduce::launch_unchecked(
            &client,
            CubeCount::Static(n_chunks.div_ceil(256) as u32, 1, 1),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_fl, n),
            BufferArg::from_raw_parts(h_sm, n * SM_STRIDE),
            BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
            wrap_i32,
            mode_i32,
            chunk,
        )
    }
    let pc_bytes = client.read_one(h_pc).expect("read_one pc");
    let pm_bytes = client.read_one(h_pm).expect("read_one pm");
    let dt = t0.elapsed();
    let pc: &[u32] = bytemuck::cast_slice(&pc_bytes);
    let pm: &[f32] = bytemuck::cast_slice(&pm_bytes);

    // The diff, lane by lane.
    let mut bad = 0usize;
    for c in 0..n_chunks {
        let e = &expected[c];
        let o = c * PARTIAL_COUNT_STRIDE;
        let lanes = [
            (e.reset, pc[o + P_RESET] as i64, "reset"),
            (e.newlines, pc[o + P_NL] as i64, "nl"),
            (e.glyphs, pc[o + P_GLYPHS] as i64, "glyphs"),
            (e.rows, pc[o + P_ROWS] as i64, "rows"),
            (e.head_len, pc[o + P_HEAD_LEN] as i64, "head_len"),
            (e.tail_len, pc[o + P_TAIL_LEN] as i64, "tail_len"),
            (e.wrap, pc[o + P_WRAP] as i64, "wrap"),
            (
                match e.mode {
                    WrapMode::Down => 0,
                    WrapMode::Back => 1,
                },
                pc[o + P_MODE] as i64,
                "mode",
            ),
        ];
        for (want, got, name) in lanes {
            if want != got {
                if bad < 8 {
                    println!("  MISMATCH chunk {c} {name}: cpu {want} gpu {got}");
                }
                bad += 1;
            }
        }
        if e.tail_advance.to_bits() != pm[c].to_bits() {
            if bad < 8 {
                println!(
                    "  MISMATCH chunk {c} tail_advance: cpu {:e} gpu {:e}",
                    e.tail_advance, pm[c]
                );
            }
            bad += 1;
        }
    }

    println!(
        "cubecl-scan-check: {} ({} B, {} chunks @ {}) — {} lane mismatches; \
         kernel+readback {:?} (smoke timing only)",
        fx.name,
        n,
        n_chunks,
        chunk,
        bad,
        dt
    );
    if bad > 0 {
        eprintln!("cubecl-scan-check FAIL: {bad} lane mismatches vs scan.rs");
        std::process::exit(1);
    }
    println!("cubecl-scan-check PASS: chunk partials bit-exact vs scan.rs");
    std::process::exit(0);
}
