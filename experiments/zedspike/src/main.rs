//! zedspike S1/S2 — Zed's language stack, headless, no window.
//!
//! The premise this proves: everything below Zed's `editor`/`workspace`/`ui`
//! layers runs without a window, and the consumption seam is pure data —
//! `language::BufferSnapshot::chunks(range, LanguageAwareStyling)` yielding
//! text runs carrying tree-sitter `HighlightId`s (theme-space indexes, built
//! per language by `Language::set_theme` + `build_highlight_map`). That
//! iterator is what a 3D text backend would swallow instead of GPUI's
//! `ShapedLine::paint`.
//!
//! S1 prints the styled runs; S2 additionally writes an HTML rendering
//! colored by One Dark's real syntax theme — the eyeball-verifiable artifact.
//! `--html PATH` selects the output file (default: out.html next to cwd).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use gpui::AppContext as _;
use gpui::TestAppContext;

/// The Zed checkout, via the per-machine `experiments/zed` symlink.
const ZED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../zed");

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let target: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{ZED}/crates/language/src/buffer.rs")));
    let html_out: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("out.html"));
    // S3: the rel path the renderer's repo scene knows this file by (the
    // sidecar is keyed by it). Defaults to the file name.
    let rel_path: String = args
        .next()
        .unwrap_or_else(|| {
            target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into())
        });
    let text = std::fs::read_to_string(&target)
        .with_context(|| format!("reading {}", target.display()))?;
    let bytes = text.len();
    let line_count = text.lines().count();

    // Headless gpui: test-support's App, no event loop, no window, no renderer.
    let cx = TestAppContext::single();

    // One Dark via ThemeRegistry::new — it inserts zed's built-in default
    // families (the real SyntaxTheme the editor resolves HighlightIds
    // against). No assets needed; the defaults are compiled in.
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
        .context("One Dark not in the default theme families")?;
    let syntax: Arc<syntax_theme::SyntaxTheme> = one_dark.syntax().clone();

    let config = grammars::load_config("rust");
    let queries = grammars::load_queries("rust");
    let rust = Arc::new(
        language::Language::new(config, Some(tree_sitter_rust::LANGUAGE.into()))
            .with_queries(queries)
            .context("loading rust language queries")?,
    );
    // Wire the theme in BEFORE parsing: set_theme builds the grammar's
    // highlight map, which is what chunk HighlightIds are indexes into.
    rust.set_theme(&syntax);

    let buffer = cx.update(|app| {
        app.new(|cx| {
            let mut buffer = language::Buffer::local(text, cx);
            buffer.set_language(Some(rust), cx);
            buffer
        })
    });

    // Tree-sitter parses on the background executor; park it so the first
    // snapshot is fully parsed (the editor does this reactively — we are a
    // batch consumer).
    cx.executor().run_until_parked();

    let snapshot = cx.read(|app| buffer.read(app).snapshot());
    let styling = language::LanguageAwareStyling {
        tree_sitter: true,
        diagnostics: false,
    };

    // The seam, consumed: styled runs, resolved to One Dark styles. One pass
    // builds BOTH artifacts — the HTML rendering and the S3 sidecar
    // (rel_path, byte-range, rrggbb) the glyph3d-native `--highlight` op eats.
    let mut runs = 0usize;
    let mut off = 0usize;
    let mut sidecar: Vec<(usize, usize, String)> = Vec::new();
    let mut html = String::with_capacity(bytes * 2);
    html.push_str("<!doctype html><meta charset=\"utf-8\"><style>body{background:#282c34;margin:2em;}pre{font:12px/1.5 ui-monospace,Menlo,monospace;white-space:pre-wrap;}</style><pre>");
    for chunk in snapshot.chunks(0..snapshot.len(), styling) {
        let name = chunk
            .syntax_highlight_id
            .and_then(|id| syntax.get_capture_name(id))
            .unwrap_or("plain");
        let style = chunk.syntax_highlight_id.and_then(|id| syntax.get(id).copied());
        let id = format!("{name}");
        let preview: String = chunk.text.chars().take(48).collect();
        let preview = preview.replace('\n', "\\n");
        println!("{id:>18} | {preview}");
        runs += 1;

        let (color, weight, style_attr, run_hex) = match style {
            Some(s) => {
                let hex = s.color.map(hsla_hex).unwrap_or_else(|| "abb2bf".into());
                let css = s.color.map(hsla_css).unwrap_or_else(|| "#abb2bf".into());
                (
                    css,
                    s.font_weight.map(|w| w.0).unwrap_or(400.0),
                    match s.font_style {
                        Some(gpui::FontStyle::Italic) => " font-style:italic;",
                        _ => "",
                    },
                    hex,
                )
            }
            // Unstyled chunks are plain text; the editor gets this color from
            // UI styles, not the syntax theme — hardcoded fallback for the
            // spike (One Dark's editor foreground).
            None => ("#abb2bf".into(), 400.0, "", "abb2bf".into()),
        };
        let escaped = html_escape(&chunk.text);
        html.push_str(&format!(
            "<span style=\"color:{color};font-weight:{weight:.0};{style_attr}\">{escaped}</span>"
        ));

        // Sidecar run; adjacent same-color runs merge.
        let (start, end) = (off, off + chunk.text.len());
        off = end;
        match sidecar.last_mut() {
            Some(last) if last.1 == start && last.2 == run_hex => last.1 = end,
            _ => sidecar.push((start, end, run_hex)),
        }
    }
    html.push_str("</pre>\n");
    std::fs::write(&html_out, html).with_context(|| format!("writing {}", html_out.display()))?;
    let sidecar_out = html_out.with_extension("sidecar");
    let body = sidecar
        .iter()
        .map(|(s, e, hex)| format!("{rel_path}\t{s}\t{e}\t{hex}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&sidecar_out, format!("{body}\n"))
        .with_context(|| format!("writing {}", sidecar_out.display()))?;

    println!(
        "\n{:?}: {} bytes, {} lines, {} styled runs — headless, no window.\nHTML: {}\nSidecar: {} ({} runs)",
        target, bytes, line_count, runs,
        html_out.display(),
        sidecar_out.display(),
        sidecar.len(),
    );
    Ok(())
}

/// HSLA (0..1 components) to `rrggbb` — the sidecar's color form. Classic
/// chroma/hue-position conversion; alpha is ignored (instances are opaque).
fn hsla_hex(c: gpui::Hsla) -> String {
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
    format!("{:02x}{:02x}{:02x}", byte(r1 + m), byte(g1 + m), byte(b1 + m))
}

fn hsla_css(c: gpui::Hsla) -> String {
    format!(
        "hsl({:.0} {:.0}% {:.0}% / {:.2})",
        c.h * 360.0,
        c.s * 100.0,
        c.l * 100.0,
        c.a
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

