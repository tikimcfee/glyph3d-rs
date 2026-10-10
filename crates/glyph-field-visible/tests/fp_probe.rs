//! The device honours the layout kernel's error-free transforms AS THE
//! KERNEL WRITES THEM. WGSL lets an implementation reassociate and simplify
//! float arithmetic, and the RTX 5090's Vulkan compiler does: a textbook
//! TwoSum error `(a - (s - bb)) + (b - bb)` compiles to 0 (printed below,
//! not asserted — another device may keep it), which cost the x narrowing
//! its lo bits. Routing every add through `fma(a, one, b)` with a runtime
//! `one` keeps each add an IEEE add. This pins that form, on a tie case
//! where the error term decides the rounding, against the CPU.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

const SRC: &str = r#"
@group(0) @binding(0) var<storage, read> inp: array<f32>;
@group(0) @binding(1) var<storage, read_write> out: array<f32>;
fn two_sum_plain(a: f32, b: f32) -> vec2<f32> {
    let s = a + b;
    let bb = s - a;
    return vec2<f32>(s, (a - (s - bb)) + (b - bb));
}
fn add1(a: f32, b: f32, one: f32) -> f32 { return fma(a, one, b); }
fn two_sum(a: f32, b: f32, one: f32) -> vec2<f32> {
    let s = add1(a, b, one);
    let bb = add1(s, -a, one);
    let e = add1(add1(a, -add1(s, -bb, one), one), add1(b, -bb, one), one);
    return vec2<f32>(s, e);
}
fn narrow(a_hi: f32, a_lo: f32, b_hi: f32, b_lo: f32, one: f32) -> f32 {
    let st = two_sum(a_hi, b_hi, one);
    let r = add1(st.y, add1(a_lo, b_lo, one), one);
    return add1(st.x, r, one);
}
@compute @workgroup_size(1)
fn main() {
    let a = inp[0]; let b = inp[1]; let lo = inp[2]; let one = inp[3];
    let plain = two_sum_plain(a, b);
    out[0] = plain.y;
    let st = two_sum(a, b, one);
    out[1] = st.y;
    out[2] = narrow(a, 0.0, b, lo, one);
    let p = a * b;
    out[3] = fma(a, b, -p);
}
"#;

#[test]
fn the_device_honours_the_fma_routed_two_sum() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .expect("an adapter");
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("fp probe"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device");
    use wgpu::util::DeviceExt;
    // Two cells plus 3.5 lands exactly on an f32 tie; the origin's lo part
    // (2^-33) decides which way it rounds.
    let cell = glyph_field_visible::fu_to_world(1229, 2320);
    let (a, b, lo) = (2.0 * cell, 3.5f32, 2f32.powi(-33));
    let inp = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&[a, b, lo, 1.0f32]), usage: wgpu::BufferUsages::STORAGE });
    let out = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: None, source: wgpu::ShaderSource::Wgsl(SRC.into()) });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: None, layout: None, module: &module, entry_point: Some("main"), compilation_options: Default::default(), cache: None });
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: inp.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: out.as_entire_binding() }],
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(1, 1, 1);
    }
    enc.copy_buffer_to_buffer(&out, 0, &staging, 0, 16);
    queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| tx.send(r).expect("probe receiver"));
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
    rx.recv().expect("callback").expect("map");
    let data = staging.slice(..).get_mapped_range().expect("range");
    let v: [f32; 4] = bytemuck::pod_read_unaligned(&data[..16]);

    let (s, e) = glyph_field_visible::tables::two_sum(a, b);
    let cpu = glyph_field_visible::tables::narrow(a, 0.0, b, lo);
    let want = (a as f64 + b as f64 + lo as f64) as f32;
    eprintln!("tie case: s={s:e} CPU error={e:e}; GPU plain TwoSum error={:e}, fma-routed error={:e}; narrow CPU {:#x} GPU {:#x} f64 {:#x}", v[0], v[1], cpu.to_bits(), v[2].to_bits(), want.to_bits());
    assert_ne!(e, 0.0, "the case must be a tie (the error term decides)");
    assert_eq!(cpu.to_bits(), want.to_bits(), "the CPU twin rounds as f64 does");
    assert_eq!(v[1].to_bits(), e.to_bits(), "the fma-routed TwoSum error is the IEEE error");
    assert_eq!(v[2].to_bits(), cpu.to_bits(), "the fma-routed narrow rounds as the CPU does");
    assert_ne!(v[3], 0.0, "fma is fused: the product's error is recovered");
}
