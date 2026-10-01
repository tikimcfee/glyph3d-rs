//! CubeCL bring-up smoke — dev-only (`--cubecl-smoke`), the note-16 phase 0.
//!
//! Three proofs on the live app device, measured not theorized:
//!   1. SHARE — a CubeCL runtime registers around OUR wgpu
//!      instance/adapter/device/queue (one Metal device, no second owner —
//!      the Lever A question the whole spike hinges on).
//!   2. BUFFER — a CubeCL-allocated buffer is a real `wgpu::Buffer`: read
//!      back through CubeCL's channel AND through our own queue (a
//!      copy_buffer_to_buffer + map on our side), proving compute and
//!      render order on one device without a host copy.
//!   3. CONTRACTION — the WGSL pipeline's unconditional InstCombinePass
//!      (single-use `a*b+c` → `fma`) vs the CPU's two-round f32 arithmetic
//!      on the `k_apply` position shape. If the bits never move, the
//!      contraction hazard in note 16 §4 shrinks further; if they do, we
//!      learn the size and the values that move it.
//!
//! Not wired into the battery: it touches a GPU and asserts live.

use cubecl::prelude::*;
use cubecl::wgpu::{AutoCompiler, AutoGraphicsApi, GraphicsApi, WgpuServer, WgpuSetup};

use crate::gpu::GpuContext;

/// Probe A's fill: `out[i] = i`, checked byte-exactly on the way back.
#[cube(launch_unchecked)]
fn fill_index(out: &mut [u32]) {
    if ABSOLUTE_POS < out.len() {
        out[ABSOLUTE_POS] = ABSOLUTE_POS as u32;
    }
}

/// Probe B: the `k_apply` lane shape — two single-use muls feeding an add,
/// InstCombinePass's exact prey.
#[cube(launch_unchecked)]
fn probe_mul_add(a: &[f32], b: &[f32], c: &[f32], d: &[f32], out: &mut [f32]) {
    let i = ABSOLUTE_POS;
    if i < a.len() {
        out[i] = a[i] * b[i] + c[i] * d[i];
    }
}

pub fn run(ctx: &GpuContext) -> ! {
    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    println!("cubecl-smoke: shared device registered (client: {})", client.name());

    // ── Probe A: kernel -> cubecl buffer -> raw wgpu::Buffer -> our queue ──
    let n = 4096usize;
    let out = client.empty(n * size_of::<u32>());
    unsafe {
        fill_index::launch_unchecked(
            &client,
            CubeCount::Static((n / 256) as u32, 1, 1),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(out.clone(), n),
        )
    }
    // CubeCL's own readback first — it is also the batch flush.
    let bytes = client.read_one(out.clone()).expect("read_one failed");
    let vals: &[u32] = bytemuck::cast_slice(&bytes);
    let bad = vals.iter().take(n).enumerate().filter(|(i, v)| **v != *i as u32).count();
    println!("cubecl-smoke A1: fill_index via cubecl read_one — {bad} bad lanes of {n}");

    // The raw buffer, driven by OUR queue into OUR staging buffer.
    let resource = client
        .get_resource::<WgpuServer<AutoCompiler>>(out.clone())
        .expect("get_resource failed");
    let wres = resource.resource();
    let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cubecl-smoke staging"),
        size: wres.size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("cubecl-smoke copy"),
    });
    encoder.copy_buffer_to_buffer(&wres.buffer, wres.offset, &staging, 0, wres.size);
    ctx.queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    ctx.device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed during smoke readback");
    rx.recv()
        .expect("map_async callback dropped")
        .expect("staging map failed");
    let mapped = slice.get_mapped_range().expect("staging mapped range");
    let our_vals: &[u32] = bytemuck::cast_slice(&mapped);
    let our_bad = our_vals.iter().take(n).enumerate().filter(|(i, v)| **v != *i as u32).count();
    drop(mapped);
    staging.unmap();
    println!("cubecl-smoke A2: same buffer via OUR queue copy+map — {our_bad} bad lanes of {n}");

    // ── Probe B: contraction on the k_apply shape ──
    let m = 65536usize;
    let mut a = Vec::with_capacity(m);
    let mut b = Vec::with_capacity(m);
    let mut c = Vec::with_capacity(m);
    let mut d = Vec::with_capacity(m);
    for i in 0..m {
        // Deterministic spread over magnitudes so the product's rounding varies.
        a.push(1.0 + (i % 1024) as f32 * 0.0009765625);
        b.push(1.0 + (i % 97) as f32 * 1e-6);
        c.push((i as f32 - 32768.0) * 0.75);
        d.push(1.0 + (i % 13) as f32 * 1e-7);
    }
    let (ha, hb, hc, hd) = (
        client.create_from_slice(bytemuck::cast_slice(&a)),
        client.create_from_slice(bytemuck::cast_slice(&b)),
        client.create_from_slice(bytemuck::cast_slice(&c)),
        client.create_from_slice(bytemuck::cast_slice(&d)),
    );
    let hout = client.empty(m * size_of::<f32>());
    unsafe {
        probe_mul_add::launch_unchecked(
            &client,
            CubeCount::Static((m / 256) as u32, 1, 1),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(ha, m),
            BufferArg::from_raw_parts(hb, m),
            BufferArg::from_raw_parts(hc, m),
            BufferArg::from_raw_parts(hd, m),
            BufferArg::from_raw_parts(hout.clone(), m),
        )
    }
    let bytes = client.read_one(hout).expect("read_one failed");
    let gpu: &[f32] = bytemuck::cast_slice(&bytes);
    // The CPU reference: Rust f32 arithmetic, which never contracts to fma.
    let mut moved = 0usize;
    let mut max_ulp = 0i64;
    let mut first: Option<(usize, f32, f32)> = None;
    for i in 0..m {
        let cpu = a[i] * b[i] + c[i] * d[i];
        if gpu[i].to_bits() != cpu.to_bits() {
            moved += 1;
            let ulp = (gpu[i].to_bits() as i64 - cpu.to_bits() as i64).abs();
            if ulp > max_ulp {
                max_ulp = ulp;
            }
            if first.is_none() {
                first = Some((i, cpu, gpu[i]));
            }
        }
    }
    if let Some((i, cpu, gpu_v)) = first {
        println!(
            "cubecl-smoke B: {moved}/{m} lanes moved vs two-round CPU (max {max_ulp} ulp); \
             first at lane {i}: cpu {cpu:.9e} gpu {gpu_v:.9e}"
        );
    } else {
        println!("cubecl-smoke B: 0/{m} lanes moved vs two-round CPU — contraction did not fire on this shape");
    }

    let failed = bad + our_bad;
    if failed > 0 {
        eprintln!("cubecl-smoke FAIL: {failed} bad fill lanes — the interop itself is broken");
        std::process::exit(1);
    }
    println!("cubecl-smoke PASS: shared device + buffer interop (probe B above is the contraction measurement)");
    std::process::exit(0);
}
