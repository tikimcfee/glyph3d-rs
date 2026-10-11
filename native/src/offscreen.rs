//! Offscreen mode: no window, no surface. Render N frames to a texture,
//! read the last one back, write a PNG, print timing, exit 0.
//!
//! This mode is the verification oracle for every future stage — keep it
//! deterministic and dependency-light.

use std::path::Path;

use crate::glyph_scene::CameraMode;
use crate::gpu::GpuContext;
use crate::scene;
use crate::{
    build_scene_with_options, Op, SceneChoice, SceneCullOptions, OFFSCREEN_HEIGHT, OFFSCREEN_WIDTH,
};

#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &GpuContext,
    choice: &SceneChoice,
    path: &Path,
    frames: u32,
    zoom: f32,
    cull_opts: SceneCullOptions,
    ops: &[Op],
) {
    let shader_composite = std::env::var_os("GLYPH_L3_SHADER_COMPOSITE").is_some();
    let format = if shader_composite {
        wgpu::TextureFormat::Bgra8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8UnormSrgb
    };
    let scene =
        build_scene_with_options(ctx, format, choice, CameraMode::Front { zoom }, cull_opts);
    run_scene(ctx, scene, format, path, frames, ops);
}

/// Render an ALREADY-BUILT scene offscreen and write the PNG — the P1c entry
/// (`experiments/fieldzed` builds its scene, applies seam envelopes, then
/// lands here). Same machinery `run` uses: L3 composite hook, scripted ops,
/// deterministic 1/60 s virtual clock, padded-row readback.
pub fn run_scene(
    ctx: &GpuContext,
    mut scene: Box<dyn scene::SceneLike>,
    format: wgpu::TextureFormat,
    path: &Path,
    frames: u32,
    ops: &[Op],
) {
    let device = &ctx.device;

    // Stage L (L3) dev-only verification hook: GLYPH_L3_SHADER_COMPOSITE=1
    // makes the offscreen target Bgra8UnormSrgb, forcing the WINDOWED
    // composite path (composite.wgsl shader instead of the format-matched
    // copy) under the deterministic oracle driver — the live-display-free
    // proof of the shader path. The readback below swizzles BGRA→RGBA so the
    // PNG compares directly against the Rgba baselines. Default (unset)
    // behavior is byte-identical to before; documented in AGENTS.md.
    let shader_composite = std::env::var_os("GLYPH_L3_SHADER_COMPOSITE").is_some();
    // sRGB target so the PNG bytes are display-ready sRGB values straight
    // out of readback (no manual gamma pass needed).
    debug_assert_eq!(
        shader_composite,
        matches!(format, wgpu::TextureFormat::Bgra8UnormSrgb),
        "run_scene: caller must pass the format the L3 hook agrees with"
    );
    let size = wgpu::Extent3d {
        width: OFFSCREEN_WIDTH,
        height: OFFSCREEN_HEIGHT,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        // Stage L (L3): + COPY_DST — the pooled-target copy composite writes
        // INTO this texture. (Its absence was caught by gate 8's byte
        // compare: without COPY_DST the copy is a validation error and the
        // frame never lands.)
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let color_view = texture.create_view(&Default::default());
    let depth_view = scene::create_depth(device, wgpu::TextureFormat::Depth32Float, size.width, size.height);

    // Stage G: scripted picks + verbs, applied in CLI order before the first
    // frame. Deterministic: the Front camera + fixed viewport make --pick-px
    // reproducible, and --pick-row/--pick-col don't involve a ray at all.
    // --cam-pose switches to a scripted Fly pose (oblique-pick repro).
    scene.set_viewport(size.width, size.height);
    for op in ops {
        let line = match op {
            Op::Pick(p) => scene.apply_pick(ctx, p),
            Op::Verb(v) => scene.apply_verb(ctx, v),
            Op::CamPose(eye, yaw, pitch) => {
                scene.set_cam_pose(*eye, *yaw, *pitch);
                None
            }
            // S3 spike: Zed-sidecar highlight — one op, applied like a verb.
            Op::Highlight(p) => scene.apply_highlight_sidecar(ctx, p),
        };
        match line {
            Some(line) => println!("{line}"),
            None => println!("op: scene does not support picking/manipulation"),
        }
    }

    // GLYPH_DRAG_SCRIPT=g|c:DX:DY[:EVENTS] (2026-10-10): a scripted grab
    // drag through the window's own entry points — the cursor parked at the
    // viewport centre, the grab key (g a file, c its zone; pick first), then
    // EVENTS (default 1) cursor moves per frame of (DX, DY) px between them,
    // applied as one drag per frame like a real mouse. With
    // GLYPH_DRAG_TIMING=1 each frame's drag prints a DRAGTIME line.
    let drag = std::env::var("GLYPH_DRAG_SCRIPT").ok().map(|spec| parse_drag_script(&spec));
    if let Some((key, ..)) = drag {
        scene.on_cursor(ctx, size.width as f32 * 0.5, size.height as f32 * 0.5);
        scene.on_key(ctx, key, true);
    }
    let mut drag_cursor = (size.width as f32 * 0.5, size.height as f32 * 0.5);

    // Readback buffer: copy_texture_to_buffer requires 256-byte-aligned rows.
    let unpadded_bpr = size.width * 4;
    let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded_bpr * size.height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let frames = frames.max(1);
    log::info!(
        "offscreen: {}x{} {:?}, rendering {} frame(s) of {} instances",
        size.width,
        size.height,
        format,
        frames,
        scene.instance_count(),
    );

    // Stage H: per-frame drain keeps the profiler's pending-frame queue (cap
    // 3) from silently dropping newer frames; the post-loop drain below
    // collects whatever is still in flight.
    let mut profile_acc = crate::gpu::ProfileAccumulator::default();
    let t0 = std::time::Instant::now();
    for frame in 0..frames {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen frame"), // Stage L (O2)
        });
        // Fixed virtual clock step (1/60 s per frame) so screenshots are
        // deterministic regardless of how fast frames actually encode. The
        // scene's own animation steps on the same clock (a no-op for every
        // scene without one, which is every golden view).
        if let Some((_, dx, dy, events)) = drag {
            for _ in 0..events {
                drag_cursor = (drag_cursor.0 + dx / events as f32, drag_cursor.1 + dy / events as f32);
                scene.on_cursor(ctx, drag_cursor.0, drag_cursor.1);
            }
        }
        scene.animate(ctx, 1.0 / 60.0);
        scene.render(
            ctx,
            &mut encoder,
            &crate::scene::FrameTarget {
                color_view: &color_view,
                // Stage L (L3): the composite's copy path needs the texture
                // handle and the format.
                color_texture: &texture,
                color_format: format,
                depth_view: &depth_view,
                width: size.width,
                height: size.height,
            },
            frame as f32 / 60.0,
        );
        // Stage H: resolve profiler queries into their readback buffers before
        // submit (extra copy commands only — the render target is untouched).
        if let Some(p) = &ctx.profiler {
            p.borrow_mut().resolve_queries(&mut encoder);
        }
        if frame == frames - 1 {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_bpr),
                        rows_per_image: Some(size.height),
                    },
                },
                size,
            );
        }
        ctx.queue.submit([encoder.finish()]);
        if let Some(p) = &ctx.profiler {
            if let Err(e) = p.borrow_mut().end_frame() {
                log::warn!("profiler end_frame: {e}");
            }
            // Non-blocking pump: fold any GPU-completed frame into the means.
            let _ = device.poll(wgpu::PollType::Poll);
            let period = ctx.queue.get_timestamp_period();
            if let Some(results) = p.borrow_mut().process_finished_frame(period) {
                profile_acc.add_frame(&results);
            }
        }
    }
    let encode_submit = t0.elapsed();

    // Block until the copy lands in the readback buffer.
    let map_t0 = std::time::Instant::now();
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed during readback");
    rx.recv()
        .expect("map_async callback dropped")
        .expect("buffer map failed");
    let map_time = map_t0.elapsed();

    // Strip row padding and write the PNG.
    let data = slice.get_mapped_range().expect("readback range not mapped");
    let mut pixels = Vec::with_capacity((unpadded_bpr * size.height) as usize);
    for row in 0..size.height {
        let start = (row * padded_bpr) as usize;
        let row_bytes = &data[start..start + unpadded_bpr as usize];
        if shader_composite {
            // The hook's target is BGRA — swizzle back to RGBA so the PNG
            // compares directly against the Rgba baselines.
            for px in row_bytes.as_chunks::<4>().0 {
                pixels.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        } else {
            pixels.extend_from_slice(row_bytes);
        }
    }
    drop(data);
    readback.unmap();

    // Stage G debug: GLYPH_G_DUMP reads slot bytes back after the frames, to
    // verify a verb's upload landed. `<slot>[,<len>]` is a stored slot;
    // `<item>:<byte>[,<len>]` (M3) is the Visible field's key — the glyph is
    // located in the transient buffer of the LAST prepared frame, which is
    // why this runs after the loop and the readback wait (before 2026-10-10
    // it ran before the first frame; a stored slot reads the same either
    // way). The PNG is untouched: a print, not a draw.
    if let Some(spec) = std::env::var_os("GLYPH_G_DUMP") {
        match parse_dump_spec(&spec.to_string_lossy()) {
            Ok(DumpSpec::Slot { slot, len }) => {
                let mut buf = vec![0u32; (len / 4).max(12)];
                scene.debug_dump_instances(ctx, slot, &mut buf);
                println!("GLYPH_G_DUMP slot {slot}: {buf:08x?}");
            }
            Ok(DumpSpec::ItemByte { item, byte, len }) => match scene.debug_locate(ctx, item, byte) {
                Some(slot) => {
                    let mut buf = vec![0u32; (len / 4).max(5)];
                    scene.debug_dump_instances(ctx, u64::from(slot), &mut buf);
                    println!("GLYPH_G_DUMP item {item} byte {byte} -> transient slot {slot}: {buf:08x?}");
                }
                None => println!(
                    "GLYPH_G_DUMP item {item} byte {byte}: not laid out in the last frame \
                     (culled or washed, or the field is not Visible — a stored field is addressed by slot)"
                ),
            },
            Err(e) => panic!("GLYPH_G_DUMP: {e}"),
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create output directory");
    }
    image::save_buffer(
        path,
        &pixels,
        size.width,
        size.height,
        image::ColorType::Rgba8,
    )
    .expect("failed to write PNG");

    let total = t0.elapsed();
    // Stage H: drain every profiler frame the GPU has completed (the readback
    // poll above already waited for all submissions, so the query maps are
    // done too) and print mean per-pass times.
    if let Some(p) = &ctx.profiler {
        let period = ctx.queue.get_timestamp_period();
        loop {
            let results = p.borrow_mut().process_finished_frame(period);
            match results {
                Some(results) => profile_acc.add_frame(&results),
                None => break,
            }
        }
        let cpu = crate::gpu::take_cpu_scope_summary(ctx);
        println!(
            "profile: {} frame(s) measured | GPU: {} | CPU: {}",
            profile_acc.frames_measured,
            if profile_acc.scopes.is_empty() { "—".to_string() } else { profile_acc.summary() },
            if cpu.is_empty() { "—".to_string() } else { cpu },
        );
    }
    // encode+submit for N frames, plus one map/wait; per-frame render time is
    // the honest number for throughput comparisons. The map wait blocks until
    // ALL submitted frames have GPU-completed (the copy is enqueued last), so
    // encode_submit+map_time is the GPU-completed time for the whole run.
    let gpu_completed = encode_submit + map_time;
    println!(
        "offscreen: {} frame(s) x {} instances | submit+render {:.2?} ({:.2?}/frame) | readback wait {:.2?} | total {:.2?}",
        frames,
        scene.instance_count(),
        encode_submit,
        encode_submit / frames,
        map_time,
        total,
    );
    if frames > 1 {
        println!(
            "offscreen: steady-state ≈ {:.1} fps ({} frames GPU-completed in {:.2?})",
            frames as f64 / gpu_completed.as_secs_f64(),
            frames,
            gpu_completed,
        );
    }
    println!("wrote {}", path.display());
}

/// What `GLYPH_G_DUMP` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpSpec {
    /// `<slot>[,<len>]`: a stored field's slot, `len` bytes (default 96).
    Slot { slot: u64, len: usize },
    /// `<item>:<byte>[,<len>]`: the Visible field's key, located in the last
    /// prepared frame's transient buffer; `len` bytes (default one 20 B slot).
    ItemByte { item: u32, byte: u32, len: usize },
}

/// Parse `GLYPH_G_DUMP`'s value.
pub fn parse_dump_spec(spec: &str) -> Result<DumpSpec, String> {
    let (head, len) = match spec.split_once(',') {
        Some((h, l)) => (h.trim(), Some(l.trim().parse::<usize>().map_err(|_| format!("bad length in {spec:?}"))?)),
        None => (spec.trim(), None),
    };
    match head.split_once(':') {
        Some((item, byte)) => {
            let item = item.trim().parse::<u32>().map_err(|_| format!("bad item in {spec:?} (want item:byte)"))?;
            let byte = byte.trim().parse::<u32>().map_err(|_| format!("bad byte in {spec:?} (want item:byte)"))?;
            Ok(DumpSpec::ItemByte { item, byte, len: len.unwrap_or(20) })
        }
        None => {
            let slot = head.parse::<u64>().map_err(|_| format!("bad slot in {spec:?} (want slot[,len] or item:byte[,len])"))?;
            Ok(DumpSpec::Slot { slot, len: len.unwrap_or(96) })
        }
    }
}

/// `GLYPH_DRAG_SCRIPT`'s `g|c:DX:DY[:EVENTS]`.
fn parse_drag_script(spec: &str) -> (winit::keyboard::KeyCode, f32, f32, u32) {
    let parts: Vec<&str> = spec.split(':').collect();
    let key = match parts.first().copied() {
        Some("g") => winit::keyboard::KeyCode::KeyG,
        Some("c") => winit::keyboard::KeyCode::KeyC,
        other => panic!("GLYPH_DRAG_SCRIPT: {other:?} is not g|c (g|c:DX:DY[:EVENTS])"),
    };
    let num = |i: usize, d: f32| parts.get(i).map_or(d, |v| v.parse().expect("GLYPH_DRAG_SCRIPT: DX, DY and EVENTS are numbers"));
    (key, num(1, 0.0), num(2, 4.0), num(3, 1.0).max(1.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::{parse_dump_spec, DumpSpec};

    /// Both forms, both defaults, and that a malformed value is refused
    /// rather than read as slot 0.
    #[test]
    fn g_dump_spec_parses_slots_and_item_bytes() {
        assert_eq!(parse_dump_spec("42"), Ok(DumpSpec::Slot { slot: 42, len: 96 }));
        assert_eq!(parse_dump_spec("42,32"), Ok(DumpSpec::Slot { slot: 42, len: 32 }));
        assert_eq!(parse_dump_spec("3:17"), Ok(DumpSpec::ItemByte { item: 3, byte: 17, len: 20 }));
        assert_eq!(parse_dump_spec(" 3:17 , 40"), Ok(DumpSpec::ItemByte { item: 3, byte: 17, len: 40 }));
        assert!(parse_dump_spec("").is_err());
        assert!(parse_dump_spec("3:").is_err());
        assert!(parse_dump_spec(":17").is_err());
        assert!(parse_dump_spec("3:17,x").is_err());
        assert!(parse_dump_spec("slot").is_err());
    }
}
