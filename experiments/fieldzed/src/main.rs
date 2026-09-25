//! fieldzed — P1c: the one-binary proof.
//!
//! Zed's language stack and the glyph field LINKED in one process — no
//! sidecar file, no IPC. The provider thread owns a headless gpui App
//! (`TestAppContext` for this rung; the production shape is remote_server's
//! HeadlessProject pattern — see the seam doc's runtime section), lays each
//! `.rs` file in the target directory out as a REAL `language::Buffer` with
//! the real Rust grammar and One Dark, and ships one
//! `seam::SurfaceUpdate` per file over a channel: version =
//! `seam::content_hash_version` (the file-driven convention — static
//! content's identity IS its version), style runs over that version's byte
//! space, content riding ONCE (`Opened`) per the corpus rule.
//!
//! The main thread creates the GPU context, builds the repo scene
//! (the scene folds the directory from disk — the join catches any
//! divergence), applies the envelopes — the renderer re-derives each file's
//! bytes and DROPS any update whose version doesn't match, never translates
//! — and renders offscreen.
//!
//! Usage: fieldzed <dir> <out.png> [eye_x eye_y eye_z yaw_deg pitch_deg]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use anyhow::Context as _;
use gpui::AppContext as _;
use gpui::TestAppContext;

use glyph3d_native::seam::{
    content_hash_version, BufferVersion, ContentDelta, FileKey, StyleRun, SurfaceUpdate,
};
use glyph3d_native::glyph_scene::CameraMode;
use glyph3d_native::{offscreen, Op, SceneChoice};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir: PathBuf = args
        .next()
        .map(PathBuf::from)
        .context("usage: fieldzed <dir> <out.png> [eye_x eye_y eye_z yaw_deg pitch_deg]")?;
    let out: PathBuf = args.next().map(PathBuf::from).context("missing <out.png>")?;
    let mut pose = [30.0f32, -120.0, 60.0, 0.0, -12.0];
    let given: Vec<f32> = args.filter_map(|a| a.parse().ok()).collect();
    if given.len() == 5 {
        pose.copy_from_slice(&given);
    }

    // ── provider thread: Zed's stack, headless ────────────────────────────
    let (tx, rx) = mpsc::channel::<SurfaceUpdate>();
    let provider_dir = dir.clone();
    let provider = std::thread::spawn(move || provide(provider_dir, tx));

    // ── render thread: the field ──────────────────────────────────────────
    let ctx = pollster::block_on(glyph3d_native::gpu::init(None));
    let choice = SceneChoice::Repo {
        dir: dir.clone(),
        strategy: glyph3d_native::layout_mojo::Strategy::Direct,
        verify: false,
        focus: None,
        wrap_mode: glyph3d_native::fold::WrapMode::Back,
        z_wrap_spacing: 0.15,
        emoji_sheet: glyph3d_native::default_emoji_sheet(),
    };
    let scene = glyph3d_native::build_scene(
        &ctx,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        &choice,
        CameraMode::Front { zoom: 1.0 },
        true,
    );

    let mut updates = Vec::new();
    while let Ok(update) = rx.recv() {
        println!(
            "provider: {} — {} style runs, version {:016x}{}",
            update.file.0,
            update.style.len(),
            update.version.0,
            if update.complete { "" } else { " (partial)" }
        );
        updates.push(update);
    }
    provider
        .join()
        .map_err(|_| anyhow::anyhow!("provider thread panicked"))?;

    // Negative self-test, same spirit as GLYPH_K4_SELFTEST: one update with
    // a WRONG version (must be dropped by the join, not translated) and one
    // for a file the field doesn't hold (the workspace grammar's problem
    // later). The audit line must report both as dropped while the real
    // updates still color every glyph.
    if let Some(first) = updates.first().cloned() {
        updates.push(seam_bogus_version(first));
    }
    updates.push(SurfaceUpdate {
        file: FileKey("not-in-the-field.rs".into()),
        version: BufferVersion(1),
        content: ContentDelta::Tombstone,
        style: Vec::new(),
        structure: Vec::new(),
        decorations: Vec::new(),
        complete: true,
    });

    if let Some(line) = scene.apply_surface_updates(&ctx, &updates) {
        println!("{line}");
    }

    let ops = [Op::CamPose(
        [pose[0], pose[1], pose[2]],
        pose[3].to_radians(),
        pose[4].to_radians(),
    )];
    offscreen::run_scene(&ctx, scene, wgpu::TextureFormat::Rgba8UnormSrgb, &out, 2, &ops);
    Ok(())
}

/// A deliberately-stale copy of a real update: same file, same runs, WRONG
/// version. The seam's law must drop it — applying it would paint glyphs
/// with runs computed against different bytes.
fn seam_bogus_version(mut u: SurfaceUpdate) -> SurfaceUpdate {
    u.version = BufferVersion(u.version.0.wrapping_add(1));
    u
}

/// The provider: one headless App, real buffers, One Dark runs as envelopes.
fn provide(dir: PathBuf, tx: mpsc::Sender<SurfaceUpdate>) {
    let cx = TestAppContext::single();

    // One Dark from Zed's built-in defaults (ThemeRegistry::new inserts
    // them) — the same SyntaxTheme the editor resolves HighlightIds against.
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
    // Theme wiring BEFORE parsing: set_theme builds the grammar's highlight
    // map, which chunk HighlightIds are indexes into.
    rust.set_theme(&syntax);

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();

    for path in files {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("provider: skip {}: {e}", path.display());
                continue;
            }
        };
        let name = path
            .file_name()
            .expect("file_name")
            .to_string_lossy()
            .into_owned();
        // Buffers are UTF-8 strings; byte offsets in runs are buffer bytes.
        // A non-UTF-8 file is skipped, not lossy-converted (a lossy convert
        // would shift every offset — the version join exists to catch exactly
        // that class, but there is nothing to render faithfully anyway).
        let Ok(text) = String::from_utf8(bytes.clone()) else {
            eprintln!("provider: skip {name}: not UTF-8");
            continue;
        };
        let version: BufferVersion = content_hash_version(&bytes);

        let buffer = cx.update(|app| {
            app.new(|cx| {
                let mut buffer = language::Buffer::local(text, cx);
                buffer.set_language(Some(rust.clone()), cx);
                buffer
            })
        });
        cx.executor().run_until_parked();

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
                // Unstyled runs are plain text; the editor gets this color
                // from UI styles, not the syntax theme — One Dark's editor
                // foreground as the stand-in (same convention as S2/S3).
                .unwrap_or([0xab, 0xb2, 0xbf]);
            let (start, end) = (off, off + chunk.text.len());
            off = end;
            match style.last_mut() {
                Some(last) if last.range.end == start && last.rgb == rgb => last.range.end = end,
                _ => style.push(StyleRun { range: start..end, rgb }),
            }
        }

        tx.send(SurfaceUpdate {
            file: FileKey(name),
            version,
            content: ContentDelta::Opened(bytes),
            style,
            structure: Vec::new(),
            decorations: Vec::new(),
            complete: true,
        })
        .expect("render thread gone");
    }
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
