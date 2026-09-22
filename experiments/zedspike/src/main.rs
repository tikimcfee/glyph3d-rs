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

const ZED: &str = "/Users/lugo/localdev/externalcompute/zed";

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

    // The seam, consumed: styled runs, resolved to One Dark styles.
    let mut runs = 0usize;
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

        let (color, weight, style_attr) = match style {
            Some(s) => (
                s.color.map(hsla_css).unwrap_or_else(|| "#abb2bf".into()),
                s.font_weight.map(|w| w.0).unwrap_or(400.0),
                match s.font_style {
                    Some(gpui::FontStyle::Italic) => " font-style:italic;",
                    _ => "",
                },
            ),
            // Unstyled chunks are plain text; the editor gets this color from
            // UI styles, not the syntax theme — hardcoded fallback for the
            // spike (One Dark's editor foreground).
            None => ("#abb2bf".into(), 400.0, ""),
        };
        let escaped = html_escape(&chunk.text);
        html.push_str(&format!(
            "<span style=\"color:{color};font-weight:{weight};{style_attr}\">{escaped}</span>"
        ));
    }
    html.push_str("</pre>\n");
    std::fs::write(&html_out, html).with_context(|| format!("writing {}", html_out.display()))?;

    println!(
        "\n{:?}: {} bytes, {} lines, {} styled runs — headless, no window.\nHTML: {}",
        target, bytes, line_count, runs,
        html_out.display()
    );
    Ok(())
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

