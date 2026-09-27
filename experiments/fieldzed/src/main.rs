//! fieldzed — P1c → P1-live: the edit→reflow pipe, in one binary.
//!
//! Zed's language stack and the glyph field linked in one process. The
//! provider thread runs a headless gpui App over REAL `language::Buffer`s
//! (Rust grammar, One Dark); the main thread owns the content map, folds it
//! with the engine, styles it from envelopes, renders offscreen.
//!
//! LIVE RUNG: the provider performs a scripted edit sequence on one file's
//! buffer — insert at top, delete a middle block, append stubs — and after
//! each edit ships a `seam::SurfaceUpdate` carrying an `Edited` delta (the
//! corpus rule: bytes cross once, deltas thereafter) plus the re-derived
//! style runs at the NEW version. The main thread applies each delta to its
//! OWN copy with `ContentDelta::apply`, re-folds (Tier 0: whole-scene
//! rebuild — the measurement this rung exists to take), re-styles, and
//! renders a frame. The version join is then a REAL cross-check: provider
//! and renderer apply the same delta through two independent code paths
//! (Zed's buffer vs our splice), and the field only repaints if both landed
//! on identical bytes — the law verifying the pipeline, not just guarding it.
//!
//! Usage: fieldzed <dir> <out_prefix> [eye_x eye_y eye_z yaw_deg pitch_deg]
//! Writes <out_prefix>edit{0..N}.png — frame 0 is the pre-edit state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use gpui::AppContext as _;
use gpui::TestAppContext;

use glyph3d_native::seam::{
    content_hash_version, BufferVersion, ContentDelta, FileKey, StyleRun, SurfaceUpdate,
};
use glyph3d_native::glyph_scene::CameraMode;
use glyph3d_native::{atlas, offscreen, repo, Op, SceneChoice};

fn main() -> anyhow::Result<()> {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // `--live` — the windowed run: the provider paces a LOOPING edit script
    // (banner in, block deleted, stubs appended, revert; ~1.5 s apart) and
    // the windowed renderer rebuilds + restyles between frames. The window
    // is the poking surface: fly (WASD/right-drag), click to pick (the pick
    // lines stream to THIS stdout), F1 for the Debug panel.
    let live = argv.first().is_some_and(|a| a == "--live");
    if live {
        argv.remove(0);
    }
    let mut args = argv.into_iter();
    let dir: PathBuf = args
        .next()
        .map(PathBuf::from)
        .context("usage: fieldzed [--live] <dir> <out_prefix> [eye_x eye_y eye_z yaw_deg pitch_deg]")?;

    // ── provider thread: Zed's stack, headless, then the edit script ──────
    let (tx, rx) = mpsc::channel::<SurfaceUpdate>();
    let provider_dir = dir.clone();
    let provider = std::thread::spawn(move || provide(provider_dir, tx, live));

    let ctx = pollster::block_on(glyph3d_native::gpu::init(None));
    let trie = glyph3d_native::default_engine_trie();
    let atlas_load = Instant::now();
    let atlas = atlas::Atlas::load(&ctx, &glyph3d_native::default_emoji_sheet());
    let atlas_us = atlas_load.elapsed().as_micros();

    if live {
        return run_live(ctx, rx, dir, trie);
    }

    let out_prefix: String = args.next().context("missing <out_prefix>")?;
    let mut pose = [30.0f32, -120.0, 60.0, 0.0, -12.0];
    let given: Vec<f32> = args.filter_map(|a| a.parse().ok()).collect();
    if given.len() == 5 {
        pose.copy_from_slice(&given);
    }

    let mut updates = Vec::new();
    while let Ok(update) = rx.recv() {
        let kind = match &update.content {
            ContentDelta::Opened(b) => format!("opened {}B", b.len()),
            ContentDelta::Edited { range, text } => {
                format!("edit {}..{} +{}B", range.start, range.end, text.len())
            }
            ContentDelta::Tombstone => "tombstone".to_string(),
        };
        println!(
            "provider: {} — v{:016x} {} runs ({kind})",
            update.file.0,
            update.version.0,
            update.style.len(),
        );
        updates.push(update);
    }
    provider.join().map_err(|_| anyhow::anyhow!("provider thread panicked"))?;

    // ── the live loop: apply delta → re-fold → re-style → render ──────────
    let mut content: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let params = repo::RepoParams::default();
    let ops = [Op::CamPose(
        [pose[0], pose[1], pose[2]],
        pose[3].to_radians(),
        pose[4].to_radians(),
    )];

    for (frame, update) in updates.iter().enumerate() {
        let t_all = Instant::now();
        // 1. Content plane: apply the delta to OUR copy (the corpus rule).
        let entry = content
            .entry(update.file.0.clone())
            .or_insert_with(|| Arc::new(Vec::new()));
        match &update.content {
            ContentDelta::Opened(bytes) => *entry = Arc::new(bytes.clone()),
            ContentDelta::Edited { .. } => {
                // Sole owner in the map: make_mut splices in place, no copy.
                let bytes = Arc::make_mut(entry);
                update.content.apply(bytes);
            }
            ContentDelta::Tombstone => {
                content.remove(&update.file.0);
                continue;
            }
        }

        // 2. Re-fold (Tier 0: whole-scene rebuild — THE measurement).
        let t_build = Instant::now();
        let files = content
            .iter()
            .map(|(rel, bytes)| repo::RepoFile::in_memory(rel.clone(), bytes.as_ref().clone()))
            .collect::<Vec<_>>();
        // The walk must be deterministic (path order) — same rule as disk.
        let mut files = files;
        files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let walk = repo::WalkResult::from_files(files);
        let load = repo::load_items(
            walk,
            Duration::ZERO,
            Path::new("."),
            &trie,
            &params,
            glyph3d_native::layout_mojo::Strategy::Direct,
            false,
            None, // folds: the offscreen frames don't carry structure (yet)
        );
        let mut staged = load.into_staged(None, &atlas.slot_ink);
        // Envelope-owned content: re-derivation (and therefore the version
        // join) reads THE BYTES WE FOLDED, not disk.
        if let Some(pick) = &mut staged.pick {
            pick.content = Some(content.clone());
        }
        let scene = glyph3d_native::build_scene_from_staged(
            &ctx,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            &atlas,
            staged,
            CameraMode::Front { zoom: 1.0 },
            true,
        );
        let build_us = t_build.elapsed().as_micros();

        // 3. Style plane: the envelope, version-checked against those bytes.
        let t_style = Instant::now();
        let audit = scene
            .apply_surface_updates(&ctx, &[update.clone()])
            .unwrap_or_default();
        let style_us = t_style.elapsed().as_micros();

        // 4. Render.
        let out = format!("{out_prefix}edit{frame}.png");
        offscreen::run_scene(
            &ctx,
            scene,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            Path::new(&out),
            2,
            &ops,
        );
        println!(
            "frame {frame}: build {build_us}µs | style {style_us}µs | total {}µs | {audit}",
            t_all.elapsed().as_micros()
        );
    }
    println!("(atlas loaded once in {atlas_us}µs — reused across rebuilds)");
    Ok(())
}

/// The LIVE run: pre-drain the Opened envelopes (the provider sends them
/// immediately, then paces the script), hand the renderer a LiveSource, and
/// enter the windowed loop. The provider keeps editing forever; every
/// envelope rebuilds the scene in place with the camera held.
fn run_live(
    ctx: glyph3d_native::gpu::GpuContext,
    rx: mpsc::Receiver<SurfaceUpdate>,
    dir: PathBuf,
    trie: PathBuf,
) -> anyhow::Result<()> {
    let mut content: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let mut backlog: Vec<SurfaceUpdate> = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(update) => {
                println!(
                    "provider: {} — v{:016x} {} runs",
                    update.file.0,
                    update.version.0,
                    update.style.len()
                );
                // Openeds stage now; anything else (the script can start the
                // moment the Openeds are out — the release provider is fast)
                // is RETAINED for the first poll. Dropping it desyncs the
                // content map from the provider's mirror, and the version
                // join would refuse every edit after (the live-measured bug).
                if let ContentDelta::Opened(bytes) = &update.content {
                    content.insert(update.file.0.clone(), Arc::new(bytes.clone()));
                } else {
                    backlog.push(update);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("provider thread exited before opening any files")
            }
        }
    }
    if content.is_empty() {
        anyhow::bail!("no Opened envelopes arrived — nothing to render");
    }
    println!(
        "live: {} file(s) staged from envelopes — entering the windowed loop \
         (WASD fly, right-drag look, click picks, F1 Debug panel; edits arrive ~1.5 s apart)",
        content.len()
    );
    // The disk-bound choice exists for the z/cluster dials; live content
    // reclaims the scene on every envelope (see LiveSource's doc).
    let choice = SceneChoice::Repo {
        dir,
        strategy: glyph3d_native::layout_mojo::Strategy::Direct,
        verify: false,
        focus: None,
        wrap_mode: glyph3d_native::fold::WrapMode::Back,
        z_wrap_spacing: 0.15,
        cluster_mode: glyph3d_native::fold::ClusterMode::Cluster,
        emoji_sheet: glyph3d_native::default_emoji_sheet(),
    };
    let live = glyph3d_native::windowed::LiveSource {
        rx,
        content,
        params: repo::RepoParams::default(),
        trie,
        emoji_sheet: glyph3d_native::default_emoji_sheet(),
        // Loaded ONCE here — the measurement showed a per-reload atlas was
        // 96% of the rebuild hitch (185 ms of 197).
        atlas: atlas::Atlas::load(&ctx, &glyph3d_native::default_emoji_sheet()),
        last_style: HashMap::new(),
        backlog,
    };
    glyph3d_native::windowed::run(
        ctx,
        choice,
        true,
        &[],
        true,
        None,
        wgpu::PresentMode::AutoVsync,
        Some(live),
    );
    Ok(())
}

/// The provider: one headless App, real buffers, One Dark runs — then a
/// scripted edit sequence on `zedspike-main.rs`, one envelope per state.
/// The provider keeps its OWN byte mirror (a Vec<u8>) and applies each edit
/// to both the Zed buffer and the mirror; the mirror's hash is the version,
/// and the mirror's deltas are what the renderer applies. Two independent
/// applications of the same delta, joined by hash — that's the point.
/// `paced` (the --live run) loops the script forever with ~1.5 s sleeps and
/// a full-range revert between cycles; the offscreen run is one pass.
fn provide(dir: PathBuf, tx: mpsc::Sender<SurfaceUpdate>, paced: bool) {
    let cx = TestAppContext::single();

    struct NoAssets;
    impl gpui::AssetSource for NoAssets {
        fn load(&self, _: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
            Ok(None)
        }
        fn list(&self, _: &str) -> anyhow::Result<Vec<gpui::SharedString>> {
            Ok(Vec::new())
        }
    }
    let registry = theme::ThemeRegistry::new(Box::new(NoAssets));
    let one_dark = registry
        .get("One Dark")
        .expect("One Dark not in the default theme families");
    let syntax = one_dark.syntax().clone();

    let config = grammars::load_config("rust");
    let queries = grammars::load_queries("rust");
    let rust = Arc::new(
        language::Language::new(config, Some(tree_sitter_rust::LANGUAGE.into()))
            .with_queries(queries)
            .expect("loading rust language queries"),
    );
    rust.set_theme(&syntax);

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();

    const EDITED: &str = "zedspike-main.rs";
    let mut mirror: Option<Vec<u8>> = None;
    let mut buffer = None;

    for path in &files {
        let bytes = std::fs::read(path).expect("read file");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(text) = String::from_utf8(bytes.clone()) else {
            eprintln!("provider: skip {name}: not UTF-8");
            continue;
        };
        if name == EDITED {
            mirror = Some(bytes.clone());
            buffer = Some(open_buffer(&cx, text, &rust));
        } else {
            // Non-edited files: one Opened envelope, styled. Park BEFORE
            // styling — chunks read the parse, and an unparked buffer
            // styles as one gray run (the frame-0 bug this comment replaced).
            let b = open_buffer(&cx, text, &rust);
            park(&cx);
            let runs = style_runs(&cx, &b, &syntax);
            send(&tx, &name, content_hash_version(&bytes), ContentDelta::Opened(bytes), runs);
        }
    }
    let Some(mirror) = mirror.as_mut() else {
        eprintln!("provider: {EDITED} not found in {} — no live demo", dir.display());
        return;
    };
    let buffer = buffer.unwrap();

    // Initial state of the edited file.
    park(&cx);
    send(
        &tx,
        EDITED,
        content_hash_version(mirror),
        ContentDelta::Opened(mirror.clone()),
        style_runs(&cx, &buffer, &syntax),
    );

    // The script — three edits, each visible in the field's geometry:
    //  1. INSERT a 12-line banner at the top (the column grows);
    //  2. DELETE a ~30-line middle block (it shrinks);
    //  3. APPEND 16 stub lines at the end (it grows again).
    // `paced` loops the cycle forever (~1.5 s per step) with a full-range
    // revert at the end — the windowed demo's heartbeat.
    let original = mirror.clone();
    loop {
        let banner = banner().to_string();
        let script: Vec<(usize, usize, String)> = {
            let text = String::from_utf8_lossy(mirror).into_owned();
            let lines: Vec<&str> = text.lines().collect();
            let mut v = vec![(0usize, 0usize, banner.clone())];
            if lines.len() > 120 {
                // Delete lines 90..120 (byte range of those lines incl. newlines).
                let start = byte_of_line(&lines, 90);
                let end = byte_of_line(&lines, 120);
                v.push((start + banner.len(), end + banner.len(), String::new()));
            }
            v.push((
                mirror.len() + banner.len(),
                mirror.len() + banner.len() + 1,
                stubs().to_string(),
            ));
            v
        };

        for (i, (start, end, text)) in script.iter().enumerate() {
            // The script's offsets were computed against the PRE-BANNER bytes +
            // banner shift for edits 2/3 — apply to BOTH sides identically.
            let range = *start..*end;
            let replacement = text.clone();
            let repl_str = replacement.clone();
            cx.update(|app| {
                buffer.update(app, |b, cx| {
                    b.edit(
                        [(range.start.min(b.len())..range.end.min(b.len()), repl_str.as_str())],
                        None,
                        cx,
                    );
                });
            });
            let delta = ContentDelta::Edited {
                range,
                text: replacement.into_bytes(),
            };
            delta.apply(mirror); // provider-side application of the same delta
            park(&cx);

            let version = content_hash_version(mirror);
            let runs = style_runs(&cx, &buffer, &syntax);
            eprintln!("provider: edit {i} applied ({} bytes now)", mirror.len());
            send(&tx, EDITED, version, delta, runs);
            if paced {
                std::thread::sleep(Duration::from_millis(1500));
            }
        }

        if !paced {
            break;
        }
        // Revert to the original: one full-range replacement. The version
        // after it hashes the original bytes — a DIFFERENT version than any
        // before, so the join accepts it as the newest state, never a replay.
        let range = 0..mirror.len();
        let text = original.clone();
        let text_str = String::from_utf8(text.clone()).expect("original was UTF-8 on open");
        cx.update(|app| {
            buffer.update(app, |b, cx| {
                b.edit([(0..b.len(), text_str.as_str())], None, cx);
            });
        });
        let delta = ContentDelta::Edited {
            range,
            text,
        };
        delta.apply(mirror);
        park(&cx);
        let version = content_hash_version(mirror);
        let runs = style_runs(&cx, &buffer, &syntax);
        eprintln!("provider: reverted ({} bytes)", mirror.len());
        send(&tx, EDITED, version, delta, runs);
        std::thread::sleep(Duration::from_millis(1500));
    }
}

const BANNER: &str = "// ═══════════════════════════════════════════════════════════════\n\
      // LIVE REFLOW — this banner was inserted by the provider at\n\
      // frame 0's version; every offset below it shifted, and the\n\
      // field re-folded from the EDITED delta, not a re-read.\n\
      // ═══════════════════════════════════════════════════════════════\n\
      // 2\n// 3\n// 4\n// 5\n// 6\n// 7\n// 8\n";

fn banner() -> &'static str {
    BANNER
}

fn stubs() -> &'static str {
    "\n// ── appended stubs ──\nfn stub_a() {}\nfn stub_b() {}\nfn stub_c() {}\nfn stub_d() {}\nfn stub_e() {}\nfn stub_f() {}\nfn stub_g() {}\nfn stub_h() {}\nfn stub_i() {}\nfn stub_j() {}\nfn stub_k() {}\nfn stub_l() {}\n"
}

fn open_buffer(
    cx: &TestAppContext,
    text: String,
    rust: &Arc<language::Language>,
) -> gpui::Entity<language::Buffer> {
    let rust = rust.clone();
    cx.update(|app| {
        app.new(|cx| {
            let mut buffer = language::Buffer::local(text, cx);
            buffer.set_language(Some(rust), cx);
            buffer
        })
    })
}

fn park(cx: &TestAppContext) {
    cx.executor().run_until_parked();
}

fn style_runs(
    cx: &TestAppContext,
    buffer: &gpui::Entity<language::Buffer>,
    syntax: &Arc<syntax_theme::SyntaxTheme>,
) -> Vec<StyleRun> {
    let snapshot = cx.read(|app| buffer.read(app).snapshot());
    let styling = language::LanguageAwareStyling {
        tree_sitter: true,
        diagnostics: false,
    };
    let mut style: Vec<StyleRun> = Vec::new();
    let mut off = 0usize;
    for chunk in snapshot.chunks(0..snapshot.len(), styling) {
        let rgb = chunk
            .syntax_highlight_id
            .and_then(|id| syntax.get(id).copied())
            .and_then(|s| s.color)
            .map(hsla_rgb)
            .unwrap_or([0xab, 0xb2, 0xbf]);
        let (start, end) = (off, off + chunk.text.len());
        off = end;
        match style.last_mut() {
            Some(last) if last.range.end == start && last.rgb == rgb => last.range.end = end,
            _ => style.push(StyleRun { range: start..end, rgb }),
        }
    }
    style
}

fn send(
    tx: &mpsc::Sender<SurfaceUpdate>,
    file: &str,
    version: BufferVersion,
    content: ContentDelta,
    style: Vec<StyleRun>,
) {
    tx.send(SurfaceUpdate {
        file: FileKey(file.to_string()),
        version,
        content,
        style,
        structure: Vec::new(),
        decorations: Vec::new(),
        complete: true,
    })
    .expect("render thread gone");
}

/// Byte offset of the START of `line_ix` (0-based) in the joined-with-\n text.
fn byte_of_line(lines: &[&str], line_ix: usize) -> usize {
    lines
        .iter()
        .take(line_ix)
        .map(|l| l.len() + 1)
        .sum()
}

/// HSLA (0..1 components) → sRGB bytes — the run color form. Chroma/hue-
/// position conversion; alpha ignored (instances are opaque).
fn hsla_rgb(c: gpui::Hsla) -> [u8; 3] {
    let h = (c.h * 360.0).rem_euclid(360.0);
    let (s, l) = (c.s, c.l);
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = chroma * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = l - chroma / 2.0;
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(r1 + m), byte(g1 + m), byte(b1 + m)]
}
