//! The command line: the clap `Cli` struct, the string→enum value parsers,
//! the op-stream assembly (`Op`, `RawOps`, verb/pick parsing) and the Stage H
//! parity tests. Extracted from `main.rs` in the 2026-09 code-shape refactor
//! — a pure move; `pub` stands in for the crate-root visibility these
//! items had.

use std::path::{Path, PathBuf};

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};

use crate::fold;
use crate::glyph_scene::{PickCommand, Verb};

/// `--wrap-mode` -> the layout parameter. clap's `value_parser` has already
/// refused anything that is not one of the two spellings, so an unknown value
/// here is a bug in this function rather than in the caller's command line —
/// which is why it panics instead of falling back to the default. A silent
/// fallback would render mode A while the operator believed they asked for B.
pub fn parse_wrap_mode(s: &str) -> fold::WrapMode {
    match s {
        "down" => fold::WrapMode::Down,
        "back" => fold::WrapMode::Back,
        other => panic!("--wrap-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--z-wrap-spacing` validation, run by clap at parse time. NaN must be
/// refused HERE, not left to `ItemParams::validate` at the layout seam:
/// clap's error names the flag and the value; the seam's panic would name
/// neither. Negative means the staircase steps FORWARD through the page
/// plane, which no baseline has ever rendered; refuse it rather than gate
/// nothing on a geometry nobody has looked at. 0 is legitimate — the
/// documented flat layout (`RepoParams::z_wrap_spacing`).
pub fn parse_z_wrap_spacing(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("--z-wrap-spacing: {s:?} is not a number"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!(
            "--z-wrap-spacing: {v} is out of domain (need a finite value >= 0)"
        ));
    }
    Ok(v)
}

/// Panics on an unknown strategy for the same reason `parse_wrap_mode` does:
/// clap has already refused anything else, so reaching here means the parser
/// and this match disagree, and silently loading with the wrong strategy would
/// make a verification run compare something other than what was asked for.
pub fn parse_strategy(s: &str) -> crate::repo::Strategy {
    use crate::repo::Strategy;
    match s {
        "naive" => Strategy::PerItem,
        "batch" => Strategy::Batched,
        "direct" => Strategy::Direct,
        "cubecl" => {
            #[cfg(feature = "cubecl")]
            {
                Strategy::Cubecl
            }
            #[cfg(not(feature = "cubecl"))]
            {
                eprintln!("error: --repo-engine cubecl was not compiled into this binary (rebuild with `cargo run --features cubecl`)");
                std::process::exit(1);
            }
        }
        "hyper" => Strategy::Hyper,
        other => panic!("--repo-engine: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--cluster-mode` -> the layout parameter. Same shape as `parse_wrap_mode`:
/// clap has already refused anything that is not one of the two spellings, so
/// an unknown value here is a bug in this function rather than in the caller's
/// command line — a silent fallback would render mode A while the operator
/// believed they asked for B.
pub fn parse_cluster_mode(s: &str) -> fold::ClusterMode {
    match s {
        "leader" => fold::ClusterMode::Leader,
        "cluster" => fold::ClusterMode::Cluster,
        other => panic!("--cluster-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--layout-mode` -> the layout parameter. Same shape as `parse_wrap_mode`:
pub fn parse_layout_mode(s: &str) -> crate::repo::RepoLayoutMode {
    match s {
        "shelf" => crate::repo::RepoLayoutMode::Shelf,
        "carrel" => crate::repo::RepoLayoutMode::Carrel,
        other => panic!("--layout-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}


/// Long-form help tail: the mode summary + verb reference + windowed keys from
/// the hand-rolled parser's --help (nothing user-facing was dropped).
const AFTER_LONG_HELP: &str = "\
MODES:
  no args                windowed text field (default file: this crate's main.rs)
  --demo                 windowed: Stage A quad-field demo
  --screenshot PATH      offscreen: render N frames, write PNG, print timing,
                         exit 0. Deterministic (fixed virtual clock).

VERBS (--verb \"V [ARGS]\", repeatable; applies to the most recent pick):
  recolor-glyph [rrggbb] | recolor-line [rrggbb]
  nudge-glyph dx dy [dz] | scale-glyph f
  move-group dx dy dz | scale-group s
  tint-group rrggbb | tint-cycle | hide-group | show-group | toggle-hidden

WINDOWED MODE:
  fly camera — WASD move, E|R up, Q|F down, RIGHT-drag look, scroll = speed,
  Esc releases | interact: LEFT click = pick glyph, h highlight line,
  g grab file (mouse drags, scroll scales), t tint, x hide |
  F1 toggle Debug panel, F2 save screenshot (out/windowed-shot-*.png) |
  --screenshot-frame N --screenshot-out PATH: scripted capture, keeps running";

/// Stage H: clap-derive CLI. Semantics (flags, defaults, op-stream order) are
/// preserved from the hand-rolled parser it replaced — parity is pinned by the
/// tests at the bottom of this file. `args_override_self` keeps the old
/// last-wins behavior for repeated scalar flags.
#[derive(Parser)]
#[command(
    name = "glyph3d-native",
    about = "native GPU port of the glyph3d-js code-visualization system (dev/verification CLI)",
    after_long_help = AFTER_LONG_HELP,
    args_override_self = true,
)]
pub struct Cli {
    /// Offscreen: render to PATH (PNG), print timing, exit 0
    #[arg(long, value_name = "PATH")]
    pub screenshot: Option<PathBuf>,
    /// Frames to render offscreen
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub frames: u32,
    /// Stage A quad-field demo instead of the text field
    #[arg(long)]
    pub demo: bool,
    /// Text file to stage (UTF-8); default: this crate's main.rs
    #[arg(long, value_name = "PATH")]
    pub render_file: Option<PathBuf>,
    /// Tile the file N times (stress)
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub copies: u32,
    /// Camera magnification for offscreen
    #[arg(long, value_name = "F", default_value_t = 1.0, allow_negative_numbers = true)]
    pub zoom: f32,
    /// The colour-emoji sheet every glyph scene loads (default:
    /// assets/atlas/emoji-sheet.bin, baked from the vendored Noto Color Emoji).
    /// Point it at another G3ES file to swap the sheet without a rebuild.
    #[arg(long, value_name = "PATH")]
    pub emoji_sheet: Option<PathBuf>,
    /// Trie for engine modes (default: assets/atlas/engine-trie.bin — the real
    /// atlas mapping; pass a .pipe.bin fixture for the toy one)
    #[arg(long, value_name = "PATH")]
    pub engine_trie: Option<PathBuf>,
    /// Stage E1: render engine records through the Slug renderer
    #[arg(long, value_name = "PATH")]
    pub engine_render: Option<PathBuf>,
    /// Fixture parity (reference port): print the canonical parse manifest for each
    /// .pipe.bin fixture and exit. tools/check-fixture-parity.sh diffs these
    /// lines against the ones engine/fixture_manifest.mojo emits from the Mojo
    /// loader — two independent parsers agreeing on checksums over their PARSED
    /// values, not on the file's bytes.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub fixture_manifest: Vec<PathBuf>,
    /// Fixture parity: lay each .pipe.bin with the CPU reference fold and diff
    /// BIT-EXACT against the oracle's own expected lanes, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub fixture_reference: Vec<PathBuf>,
    /// Trie rebuild: rebuild each .pipe.bin's trie from its own bytes with the
    /// ported GlyphTrie and diff against the trie the oracle stored, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub fixture_trie: Vec<PathBuf>,
    /// Full fold: run the ported serial fold over each .pipe.bin and compare
    /// EVERY lane of EVERY byte plus boxes and the batch union, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub fixture_fold: Vec<PathBuf>,
    /// Scan form: run the ported scan form over each .pipe.bin at a SWEEP of
    /// chunk/group/shard tunings and compare under the tiered contract, then
    /// exit. Invariance across the tunings is associativity in situ.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub fixture_scan: Vec<PathBuf>,
    /// Bake: replay each .bake.bin through the ported bake and diff the
    /// record AND every seed-protocol query bit-exact, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub fixture_bake: Vec<PathBuf>,
    /// Stage E2: load a whole repository as a field of code pages
    #[arg(long, value_name = "DIR")]
    pub load_repo: Option<PathBuf>,
    /// Which FFI strategy the Mojo backend uses. `direct` is the DEFAULT: the
    /// engine writes render instances straight into the arena, materializing no
    /// wire record on either side of the FFI — one pass where `naive` and
    /// `batch` take three, and it folds in chunks so lane memory follows the
    /// chunk rather than the corpus. The record strategies remain because they
    /// are the verification form: `VerifyLayout` needs a wire stream, and
    /// `--repo-verify` diffs whichever pair you name.
    #[arg(long, value_name = "MODE", default_value = "hyper", value_parser = ["naive", "batch", "direct", "cubecl", "hyper"])]
    pub repo_engine: String,
    /// Diff the chosen strategy against a counterpart, bit-exact over the
    /// whole repo: placements and instances always, wire records when both
    /// paths have them (`direct` has none, and the PASS line says so).
    #[arg(long)]
    pub repo_verify: bool,
    /// How a wrap is spent on a repo load: `back` (the default, and what the
    /// `repo-wide`/`repo-zoom` byte-equal screenshot baselines are taken
    /// under; `repo-down` covers the other) keeps the row and steps the
    /// segment back in depth instead — one row per source line however long
    /// it is. `down` advances the visual row per wrap.
    #[arg(long, value_name = "MODE", default_value = "back", value_parser = ["down", "back"])]
    pub wrap_mode: String,
    /// The wrap staircase's pitch: z step per intra-line wrap segment, as a
    /// multiple of the em cell height (`RepoParams::z_wrap_spacing` — the
    /// web's `zWrapSpacing`). 0 restores the flat layout exactly
    /// (repo.rs). The default is what the repo screenshot baselines are
    /// taken under — same standing as --wrap-mode.
    // allow_negative_numbers so a negative REACHES the parser and is refused
    // with the flag named — otherwise clap eats "-0.1" as an unknown flag.
    #[arg(long, value_name = "F", default_value_t = 0.15, allow_negative_numbers = true, value_parser = parse_z_wrap_spacing)]
    pub z_wrap_spacing: f64,
    /// Whether the sequence pass resolves codepoint clusters to single glyphs
    /// on a repo load (and on --render-file): `cluster` (the default, since
    /// 2026-09-22) resolves the font's sequences (ZWJ families, RI flags, skin
    /// tones, keycaps) to single slots with trailing leaders zeroed; `leader`
    /// is one glyph per UTF-8 leader. The emoji baselines pin `leader`
    /// explicitly.
    #[arg(long, value_name = "MODE", default_value = "cluster", value_parser = ["leader", "cluster"])]
    pub cluster_mode: String,
    /// Spatial arrangement mode for the repository files across the canvas:
    /// `shelf` (default: height-classed shelves across the whole repo) or
    /// `carrel` (hierarchical directory-based neighborhood carrels).
    #[arg(long, value_name = "MODE", default_value = "shelf", value_parser = ["shelf", "carrel"])]
    pub layout_mode: String,
    /// Stage E2: frame the first file whose path contains SUBSTR
    #[arg(long, value_name = "SUBSTR")]
    pub focus_file: Option<String>,
    /// Stage E2: walk + engine + stage + stats, then exit (no GPU)
    #[arg(long)]
    pub repo_scan_only: bool,
    /// Stage F: disable the cull/LOD pass (legacy per-chunk draws; debug/A-B)
    #[arg(long)]
    pub no_cull: bool,
    /// Stage K: windowed without the egui UI overlay (exact pre-K behavior)
    #[arg(long)]
    pub no_ui: bool,
    /// Stage K (K6): windowed only — capture the frame after N frames have
    /// rendered (requires --screenshot-out; the app KEEPS RUNNING afterward —
    /// unlike --screenshot it never exits)
    #[arg(long, value_name = "N", requires = "screenshot_out", conflicts_with = "screenshot")]
    pub screenshot_frame: Option<u64>,
    /// Stage K (K6): windowed only — PNG path for --screenshot-frame
    #[arg(long, value_name = "PATH", requires = "screenshot_frame", conflicts_with = "screenshot")]
    pub screenshot_out: Option<PathBuf>,
    /// Generate shell completions for SHELL and exit
    #[arg(long, value_name = "SHELL")]
    pub generate: Option<clap_complete::Shell>,
    /// Print the golden-set key for the adapter wgpu picks (`backend-vendor`,
    /// e.g. `vulkan-nvidia`) and exit. The build tool asks this to choose
    /// which baseline directory the pixel gate compares against.
    #[arg(long)]
    pub gpu_key: bool,
    /// Print the full hardware profile the renderer resolved and exit — the
    /// provenance record committed beside a golden set as ADAPTER.txt.
    #[arg(long)]
    pub gpu_profile: bool,
    /// Dev-only CubeCL bring-up smoke (note 16, phase 0): share the device,
    /// prove buffer interop both directions, measure float contraction on the
    /// k_apply shape, print the verdicts, exit. Not wired into the battery.
    #[arg(long)]
    pub cubecl_smoke: bool,
    /// Dev-only CubeCL scan check (note 16, phase 1): the chunk_reduce kernel
    /// over one fixture, chunk partials diffed bit-exact vs scan.rs, exit.
    #[arg(long, value_name = "PATH")]
    pub cubecl_scan_check: Option<PathBuf>,
    /// Dev-only CubeCL chain check (note 16, phase 2): the full scan skeleton
    /// over one fixture — counts + line_advance bit-exact vs scan.rs,
    /// positions deviation-reported, exit.
    #[arg(long, value_name = "PATH")]
    pub cubecl_chain_check: Option<PathBuf>,
    /// Dev-only CubeCL chain bench (note 16, phase 2): the scan skeleton over
    /// a raw file as one item, dispatch/readback timing, exit.
    #[arg(long, value_name = "PATH")]
    pub cubecl_chain_bench: Option<PathBuf>,
    /// Dev-only CubeCL decode check (phase 3a): the device decode over one
    /// fixture — packed flags + advance diffed bit-exact vs fold::decode_all.
    #[arg(long, value_name = "PATH")]
    pub cubecl_decode_check: Option<PathBuf>,
    /// Dev-only CubeCL cluster check (phase 3b): decode + cluster on device
    /// vs decode_all + resolve_clusters — flags + advance bit-exact, exit.
    #[arg(long, value_name = "PATH")]
    pub cubecl_cluster_check: Option<PathBuf>,
    /// Dev-only CubeCL repo parity driver (phase 4 rung 3): the full chain
    /// over a real repository, records diffed tier-aware against the
    /// engine's batched output — the fence the load-path flip rides on.
    #[arg(long, value_name = "DIR")]
    pub cubecl_repo_check: Option<PathBuf>,
    /// Windowed only: how frames reach the display. `fifo` (the default) is
    /// vsync, so the FPS line reads the monitor's refresh; `mailbox` and
    /// `immediate` uncap it where the surface supports them (else fifo, and
    /// the log says so). Offscreen renders never present and ignore this.
    #[arg(long, value_name = "MODE", default_value = "fifo", value_parser = ["fifo", "mailbox", "immediate"])]
    pub present_mode: String,
    /// Stage G op-stream flags, captured per-flag by clap and re-interleaved
    /// into `ops` by build_ops().
    #[command(flatten)]
    pub raw_ops: RawOps,
    /// Stage G: the interleaved pick/verb script, in CLI order. Verbs apply
    /// to the most recent pick.
    #[arg(skip)]
    pub ops: Vec<Op>,
}

/// The op-stream flags exactly as clap captures them (per-flag vectors).
/// `build_ops` restores the true CLI interleaving via occurrence indices.
#[derive(clap::Args, Default)]
pub struct RawOps {
    /// Stage G: pick the first file whose path contains SUBSTR
    #[arg(long, value_name = "SUBSTR", action = ArgAction::Append)]
    pub pick_file: Vec<String>,
    /// With --pick-file: deterministic glyph pick (folded row)
    #[arg(long, value_name = "N", action = ArgAction::Append)]
    pub pick_row: Vec<u32>,
    /// With --pick-file: deterministic glyph pick (folded col)
    #[arg(long, value_name = "M", action = ArgAction::Append)]
    pub pick_col: Vec<u32>,
    /// Ray pick through physical pixel (X,Y) of the viewport
    #[arg(long, value_names = ["X", "Y"], num_args = 2, action = ArgAction::Append, allow_negative_numbers = true)]
    pub pick_px: Vec<f32>,
    /// Scripted Fly-camera pose: eye + yaw/pitch in DEGREES (interleaves with
    /// picks/verbs like --pick-px)
    #[arg(long, value_names = ["X", "Y", "Z", "YAW", "PITCH"], num_args = 5, action = ArgAction::Append, allow_negative_numbers = true)]
    pub cam_pose: Vec<f32>,
    /// Manipulation verb on the most recent pick (repeatable — see VERBS below)
    #[arg(long, value_name = "V [ARGS]", action = ArgAction::Append, value_parser = parse_verb)]
    pub verb: Vec<Verb>,
    /// S3 spike (`experiments/zedspike`): apply a Zed-pipeline highlight
    /// sidecar — `rel_path<TAB>start<TAB>end<TAB>rrggbb` runs, byte offsets
    /// into each file — to per-glyph instance colors, interleaved with
    /// picks/verbs before the first frame. Repo scenes; without the flag,
    /// rendering is byte-identical.
    #[arg(long, value_name = "PATH", action = ArgAction::Append)]
    pub highlight: Vec<PathBuf>,
}

/// Stage G: one scripted operation (picks and verbs interleave in CLI order).
pub enum Op {
    Pick(PickCommand),
    Verb(Verb),
    /// Scripted Fly-camera pose: eye + yaw/pitch (RADIANS) — repro of
    /// oblique windowed camera states for --pick-px.
    CamPose([f32; 3], f32, f32),
    /// S3 spike: apply a Zed-sidecar highlight to instance colors (offscreen,
    /// before the first frame — same op-stream slot as picks/verbs).
    Highlight(PathBuf),
}

/// --pick-row/--pick-col upgrade the most recent --pick-file pick into a
/// deterministic RowCol pick (defaults: row 0 / col 0 for the unset half).
fn set_pick_row_col(ops: &mut Vec<Op>, row: Option<u32>, col: Option<u32>) {
    match ops.last_mut() {
        Some(Op::Pick(PickCommand::File(f))) => {
            let f = f.clone();
            ops.pop();
            ops.push(Op::Pick(PickCommand::RowCol {
                file: f,
                row: row.unwrap_or(0),
                col: col.unwrap_or(0),
            }));
        }
        Some(Op::Pick(PickCommand::RowCol { row: r, col: c, .. })) => {
            if let Some(row) = row {
                *r = row;
            }
            if let Some(col) = col {
                *c = col;
            }
        }
        _ => panic!("--pick-row/--pick-col must follow --pick-file"),
    }
}

/// Parse a `--verb` string into a Verb (clap `value_parser`). Forms:
///   recolor-glyph `[rrggbb]`      recolor-line `[rrggbb]`
///   nudge-glyph dx dy `[dz]`      scale-glyph f
///   move-group dx dy dz         scale-group s
///   tint-group rrggbb           tint-cycle
///   hide-group | show-group | toggle-hidden
pub fn parse_verb(s: &str) -> Result<Verb, String> {
    let t: Vec<&str> = s.split_whitespace().collect();
    let usage = format!(
        "unknown/malformed --verb {s:?} — expected recolor-glyph|recolor-line|\
         nudge-glyph|scale-glyph|move-group|scale-group|tint-group|tint-cycle|\
         hide-group|show-group|toggle-hidden"
    );
    let f = |i: usize| -> Result<f32, String> {
        t.get(i)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("--verb {s:?}: bad/missing float at position {i}"))
    };
    let hex = |i: usize| -> Result<[u8; 3], String> {
        let h = t
            .get(i)
            .ok_or_else(|| format!("--verb {s:?}: missing rrggbb at position {i}"))?
            .trim_start_matches('#');
        let v = u32::from_str_radix(h, 16)
            .map_err(|_| format!("--verb {s:?}: bad hex color {h:?}"))?;
        Ok([((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8])
    };
    Ok(match t.first().copied().unwrap_or("") {
        "recolor-glyph" => Verb::RecolorGlyph(if t.len() > 1 { hex(1)? } else { [255, 80, 80] }),
        "recolor-line" => Verb::RecolorLine(if t.len() > 1 { hex(1)? } else { [255, 213, 79] }),
        "nudge-glyph" => Verb::NudgeGlyph([f(1)?, f(2)?, if t.len() > 3 { f(3)? } else { 0.0 }]),
        "scale-glyph" => Verb::ScaleGlyph(f(1)?),
        "move-group" => Verb::MoveGroup([f(1)?, f(2)?, f(3)?]),
        "scale-group" => Verb::ScaleGroup(f(1)?),
        "tint-group" => {
            let [r, g, b] = hex(1)?;
            Verb::TintGroup([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
        }
        "tint-cycle" => Verb::TintCycle,
        "hide-group" => Verb::SetHidden(true),
        "show-group" => Verb::SetHidden(false),
        "toggle-hidden" => Verb::ToggleHidden,
        _ => return Err(usage),
    })
}

/// Re-interleave the op stream in true CLI order. clap stores each flag's
/// values separately; `indices_of` yields one argv index PER VALUE (verified
/// against clap_builder 4.6.6: `push_arg_values` pushes an index per value),
/// so for multi-value flags (--pick-px, --cam-pose) both values and indices
/// are chunked by the flag's arity and zipped occurrence-by-occurrence.
fn build_ops(matches: &clap::ArgMatches, raw: &RawOps) -> Vec<Op> {
    enum Keyed {
        PickFile(String),
        Row(u32),
        Col(u32),
        Px(f32, f32),
        CamPose([f32; 3], f32, f32),
        Verb(Verb),
        Highlight(PathBuf),
    }
    let indices = |id: &str| -> Vec<usize> {
        matches.indices_of(id).map(Iterator::collect).unwrap_or_default()
    };
    let mut keyed: Vec<(usize, Keyed)> = Vec::new();
    for (i, f) in indices("pick_file").into_iter().zip(raw.pick_file.iter()) {
        keyed.push((i, Keyed::PickFile(f.clone())));
    }
    for (i, r) in indices("pick_row").into_iter().zip(raw.pick_row.iter()) {
        keyed.push((i, Keyed::Row(*r)));
    }
    for (i, c) in indices("pick_col").into_iter().zip(raw.pick_col.iter()) {
        keyed.push((i, Keyed::Col(*c)));
    }
    for (ic, xy) in indices("pick_px").chunks(2).zip(raw.pick_px.as_chunks::<2>().0) {
        keyed.push((ic[0], Keyed::Px(xy[0], xy[1])));
    }
    for (ic, v) in indices("cam_pose").chunks(5).zip(raw.cam_pose.as_chunks::<5>().0) {
        // Degrees on the CLI, radians in the op stream (unchanged semantics).
        keyed.push((
            ic[0],
            Keyed::CamPose([v[0], v[1], v[2]], v[3].to_radians(), v[4].to_radians()),
        ));
    }
    for (i, v) in indices("verb").into_iter().zip(raw.verb.iter()) {
        keyed.push((i, Keyed::Verb(v.clone())));
    }
    for (i, p) in indices("highlight").into_iter().zip(raw.highlight.iter()) {
        keyed.push((i, Keyed::Highlight(p.clone())));
    }
    keyed.sort_by_key(|(i, _)| *i);

    let mut ops = Vec::new();
    for (_, k) in keyed {
        match k {
            Keyed::PickFile(f) => ops.push(Op::Pick(PickCommand::File(f))),
            Keyed::Row(r) => set_pick_row_col(&mut ops, Some(r), None),
            Keyed::Col(c) => set_pick_row_col(&mut ops, None, Some(c)),
            Keyed::Px(x, y) => ops.push(Op::Pick(PickCommand::Pixel { x, y })),
            Keyed::CamPose(p, yaw, pitch) => ops.push(Op::CamPose(p, yaw, pitch)),
            Keyed::Verb(v) => ops.push(Op::Verb(v)),
            Keyed::Highlight(p) => ops.push(Op::Highlight(p)),
        }
    }
    ops
}

/// Parse argv (including `argv[0]`) into a Cli, reconstructing the op stream.
fn parse_cli_from(matches: clap::ArgMatches) -> Cli {
    let mut cli = Cli::from_arg_matches(&matches).expect("clap derive round-trip");
    cli.ops = build_ops(&matches, &cli.raw_ops);
    cli
}

pub fn parse_cli() -> Cli {
    parse_cli_from(Cli::command().get_matches())
}

/// clap has already refused anything outside the three spellings.
pub fn parse_present_mode(s: &str) -> wgpu::PresentMode {
    match s {
        "mailbox" => wgpu::PresentMode::Mailbox,
        "immediate" => wgpu::PresentMode::Immediate,
        _ => wgpu::PresentMode::Fifo,
    }
}

pub fn default_text_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")
}

// ── Stage H: CLI parity tests ────────────────────────────────────────────────
// Pin the clap migration against the hand-rolled parser's semantics: same
// flags, same defaults, same op-stream interleaving, same pick-row/col upgrade
// rules (including the error path), same last-wins scalar repeats.
#[cfg(test)]
mod cli_tests {
    use super::*;

    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let argv = std::iter::once("glyph3d-native").chain(args.iter().copied());
        let matches = Cli::command().try_get_matches_from(argv)?;
        Ok(parse_cli_from(matches))
    }

    fn parse(args: &[&str]) -> Cli {
        try_parse(args).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    #[test]
    fn defaults_match_old_parser() {
        let cli = parse(&[]);
        assert!(cli.screenshot.is_none());
        assert_eq!(cli.frames, 1);
        assert!(!cli.demo);
        assert!(cli.render_file.is_none());
        assert_eq!(cli.copies, 1);
        assert_eq!(cli.zoom, 1.0);
        assert!(cli.engine_trie.is_none());
        assert!(cli.emoji_sheet.is_none());
        assert!(cli.engine_render.is_none());
        assert!(cli.load_repo.is_none());
        // The DEFAULT is `hyper` (pure-Rust parallel direct engine).
        assert_eq!(cli.repo_engine, "hyper");
        // THE DEFAULT THE SCREENSHOT BASELINES DEPEND ON. A change here moves
        // repo-wide.png and repo-zoom.png, so it is pinned in the CLI layer too
        // and not only in RepoParams::default.
        assert_eq!(cli.wrap_mode, "back");
        assert_eq!(parse_wrap_mode(&cli.wrap_mode), fold::WrapMode::Back);
        // ...and the non-default is still reachable and still spelled the same.
        assert_eq!(parse_wrap_mode("down"), fold::WrapMode::Down);
        // Same standing as wrap_mode: the wrap staircase's pitch moves the
        // same baselines (repo-wide renders the staircase), so the CLI layer
        // pins the default too, not only RepoParams::default (repo.rs).
        assert_eq!(cli.z_wrap_spacing, 0.15);
        // The sequence pass: cluster is the default (since 2026-09-22); leader
        // is the reachable other spelling, and the emoji view pins it by hand.
        assert_eq!(cli.cluster_mode, "cluster");
        assert_eq!(parse_cluster_mode(&cli.cluster_mode), fold::ClusterMode::Cluster);
        assert_eq!(parse_cluster_mode("leader"), fold::ClusterMode::Leader);
        assert_eq!(cli.layout_mode, "shelf");
        assert_eq!(parse_layout_mode(&cli.layout_mode), crate::repo::RepoLayoutMode::Shelf);
        assert_eq!(parse_layout_mode("carrel"), crate::repo::RepoLayoutMode::Carrel);
        assert!(!cli.repo_verify);
        assert!(cli.focus_file.is_none());
        assert!(!cli.repo_scan_only);
        assert!(!cli.no_cull);
        assert!(!cli.no_ui);
        assert!(cli.screenshot_frame.is_none());
        assert!(cli.screenshot_out.is_none());
        assert!(cli.ops.is_empty());
        assert!(!cli.gpu_key);
        assert!(!cli.gpu_profile);
        // Fifo is the default because it is what every FPS figure before
        // 2026-09-07 was measured under; changing it would make old numbers
        // incomparable without saying so.
        assert_eq!(cli.present_mode, "fifo");
        assert_eq!(parse_present_mode(&cli.present_mode), wgpu::PresentMode::Fifo);
    }

    #[test]
    fn scalar_flags_parse() {
        let cli = parse(&[
            "--screenshot", "out.png", "--frames", "2", "--demo", "--copies", "3", "--zoom",
            "2.5", "--no-cull", "--no-ui", "--load-repo", "fixtures/g-pick-repo",
            "--repo-engine", "batch", "--repo-verify", "--focus-file", "alpha",
            "--wrap-mode", "back", "--z-wrap-spacing", "0.6", "--cluster-mode", "cluster",
            "--layout-mode", "carrel",
            "--render-file", "src/main.rs", "--engine-trie", "t.bin",
            "--engine-render", "c.rs",
            "--present-mode", "mailbox", "--gpu-key", "--gpu-profile",
            "--emoji-sheet", "sheets/other.bin",
        ]);
        assert_eq!(cli.emoji_sheet, Some(PathBuf::from("sheets/other.bin")));
        assert_eq!(cli.present_mode, "mailbox");
        assert_eq!(parse_present_mode(&cli.present_mode), wgpu::PresentMode::Mailbox);
        assert!(cli.gpu_key);
        assert!(cli.gpu_profile);
        assert_eq!(cli.screenshot, Some(PathBuf::from("out.png")));
        assert_eq!(cli.frames, 2);
        assert!(cli.demo);
        assert_eq!(cli.copies, 3);
        assert_eq!(cli.zoom, 2.5);
        assert!(cli.no_cull);
        assert!(cli.no_ui);
        assert_eq!(cli.load_repo, Some(PathBuf::from("fixtures/g-pick-repo")));
        assert_eq!(cli.repo_engine, "batch");
        assert_eq!(cli.wrap_mode, "back");
        assert_eq!(parse_wrap_mode(&cli.wrap_mode), fold::WrapMode::Back);
        assert_eq!(cli.z_wrap_spacing, 0.6);
        assert_eq!(cli.cluster_mode, "cluster");
        assert_eq!(parse_cluster_mode(&cli.cluster_mode), fold::ClusterMode::Cluster);
        assert_eq!(cli.layout_mode, "carrel");
        assert_eq!(parse_layout_mode(&cli.layout_mode), crate::repo::RepoLayoutMode::Carrel);
        assert!(cli.repo_verify);
        assert_eq!(cli.focus_file.as_deref(), Some("alpha"));
        assert_eq!(cli.render_file, Some(PathBuf::from("src/main.rs")));
        assert_eq!(cli.engine_trie, Some(PathBuf::from("t.bin")));
        assert_eq!(cli.engine_render, Some(PathBuf::from("c.rs")));
    }

    /// An unknown mode is REFUSED at the boundary, not folded to the default.
    /// A silent fallback would lay out mode A while the operator asked for B,
    /// and nothing downstream could tell them apart from a correct mode-A run.
    #[test]
    fn an_unknown_wrap_mode_is_refused() {
        let text = match try_parse(&["--wrap-mode", "sideways"]) {
            Ok(_) => panic!("clap must refuse an unknown wrap mode"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("sideways"), "the error must name the bad value: {text}");
    }

    /// Same rule as the wrap mode's: an unknown cluster mode is REFUSED at the
    /// boundary, not folded to the default — a silent fallback would render
    /// mode A while the operator believed they asked for B.
    #[test]
    fn an_unknown_cluster_mode_is_refused() {
        let text = match try_parse(&["--cluster-mode", "grapheme"]) {
            Ok(_) => panic!("clap must refuse an unknown cluster mode"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("grapheme"), "the error must name the bad value: {text}");
    }

    /// Same rule: an unknown layout mode is REFUSED at the boundary.
    #[test]
    fn an_unknown_layout_mode_is_refused() {
        let text = match try_parse(&["--layout-mode", "grid"]) {
            Ok(_) => panic!("clap must refuse an unknown layout mode"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("grid"), "the error must name the bad value: {text}");
    }

    /// Out-of-domain spacing is REFUSED at the boundary with the flag and the
    /// value named — not passed down to ItemParams::validate, whose panic
    /// would say "z_step" and nothing about the command line. NaN is the
    /// dangerous one (two NaN layouts compare bit-equal — see
    /// ItemParams::validate); negative is a forward staircase no baseline
    /// has ever rendered.
    #[test]
    fn an_out_of_domain_z_wrap_spacing_is_refused() {
        for bad in ["-0.1", "NaN", "inf", "abc"] {
            let text = match try_parse(&["--z-wrap-spacing", bad]) {
                Ok(_) => panic!("clap must refuse --z-wrap-spacing {bad}"),
                Err(e) => e.to_string(),
            };
            assert!(text.contains("--z-wrap-spacing"), "error must name the flag: {text}");
        }
        // The boundary itself is in domain: 0 is the documented flat layout.
        let cli = parse(&["--z-wrap-spacing", "0"]);
        assert_eq!(cli.z_wrap_spacing, 0.0);
    }

    #[test]
    fn repeated_scalar_flag_is_last_wins() {
        // The hand-rolled parser silently took the last occurrence.
        let cli = parse(&["--zoom", "2", "--zoom", "3"]);
        assert_eq!(cli.zoom, 3.0);
    }

    #[test]
    fn op_stream_preserves_cli_order() {
        let cli = parse(&[
            "--pick-file", "alpha", "--pick-row", "4", "--pick-col", "4",
            "--verb", "recolor-line ff0000",
            "--cam-pose", "1", "2", "3", "30", "-10",
            "--pick-px", "100", "200",
            "--verb", "tint-cycle",
            "--pick-px", "300", "400",
            "--pick-file", "beta",
            "--verb", "hide-group",
        ]);
        assert_eq!(cli.ops.len(), 8);
        match &cli.ops[0] {
            Op::Pick(PickCommand::RowCol { file, row, col }) => {
                assert_eq!(file, "alpha");
                assert_eq!(*row, 4);
                assert_eq!(*col, 4);
            }
            _ => panic!("op[0] should be the upgraded RowCol pick"),
        }
        assert!(matches!(cli.ops[1], Op::Verb(Verb::RecolorLine([255, 0, 0]))));
        match &cli.ops[2] {
            Op::CamPose(p, yaw, pitch) => {
                assert_eq!(*p, [1.0, 2.0, 3.0]);
                assert!((yaw - 30f32.to_radians()).abs() < 1e-6);
                assert!((pitch - (-10f32).to_radians()).abs() < 1e-6);
            }
            _ => panic!("op[2] should be CamPose"),
        }
        assert!(matches!(
            cli.ops[3],
            Op::Pick(PickCommand::Pixel { x: 100.0, y: 200.0 })
        ));
        assert!(matches!(cli.ops[4], Op::Verb(Verb::TintCycle)));
        assert!(matches!(
            cli.ops[5],
            Op::Pick(PickCommand::Pixel { x: 300.0, y: 400.0 })
        ));
        assert!(matches!(&cli.ops[6], Op::Pick(PickCommand::File(f)) if f == "beta"));
        assert!(matches!(&cli.ops[7], Op::Verb(Verb::SetHidden(true))));
    }

    #[test]
    fn pick_file_beta_is_in_stream() {
        let cli = parse(&["--pick-px", "1", "2", "--pick-file", "beta", "--pick-row", "7"]);
        assert_eq!(cli.ops.len(), 2);
        assert!(matches!(
            cli.ops[0],
            Op::Pick(PickCommand::Pixel { x: 1.0, y: 2.0 })
        ));
        match &cli.ops[1] {
            Op::Pick(PickCommand::RowCol { file, row, col }) => {
                assert_eq!(file, "beta");
                assert_eq!(*row, 7);
                assert_eq!(*col, 0);
            }
            _ => panic!("op[1] should be RowCol beta 7 0"),
        }
    }

    #[test]
    fn pick_row_col_upgrade_semantics() {
        // Repeated upgrades mutate the same pick (row/col defaults 0).
        let cli = parse(&["--pick-file", "a", "--pick-row", "1", "--pick-row", "2"]);
        assert_eq!(cli.ops.len(), 1);
        match &cli.ops[0] {
            Op::Pick(PickCommand::RowCol { file, row, col }) => {
                assert_eq!(file, "a");
                assert_eq!(*row, 2);
                assert_eq!(*col, 0);
            }
            _ => panic!("expected RowCol"),
        }
        let cli = parse(&["--pick-file", "a", "--pick-col", "5", "--pick-row", "3"]);
        match &cli.ops[0] {
            Op::Pick(PickCommand::RowCol { row, col, .. }) => {
                assert_eq!(*row, 3);
                assert_eq!(*col, 5);
            }
            _ => panic!("expected RowCol"),
        }
        // No row/col: plain File pick survives.
        let cli = parse(&["--pick-file", "a"]);
        assert!(matches!(&cli.ops[0], Op::Pick(PickCommand::File(f)) if f == "a"));
    }

    #[test]
    #[should_panic(expected = "--pick-row/--pick-col must follow --pick-file")]
    fn pick_row_without_pick_file_panics() {
        parse(&["--pick-row", "1"]);
    }

    #[test]
    #[should_panic(expected = "--pick-row/--pick-col must follow --pick-file")]
    fn pick_row_after_verb_panics() {
        parse(&["--pick-file", "a", "--verb", "tint-cycle", "--pick-row", "1"]);
    }

    #[test]
    fn unknown_flag_errors() {
        let err = try_parse(&["--bogus"]).err().expect("--bogus must fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn help_is_display_help() {
        // -h/--help surface as DisplayHelp via try_get_matches_from; the real
        // binary exits 0 through clap's default error exit path.
        let err = Cli::command()
            .try_get_matches_from(["glyph3d-native", "--help"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn repo_engine_rejects_bad_value() {
        assert!(try_parse(&["--repo-engine", "turbo"]).is_err());
    }

    #[test]
    fn verb_error_messages_survive() {
        let err = parse_verb("frobnicate 1 2").err().expect("bad verb must fail");
        assert!(err.contains("unknown/malformed --verb"), "{err}");
        let err = parse_verb("nudge-glyph 1").err().expect("bad verb must fail");
        assert!(err.contains("bad/missing float at position 2"), "{err}");
        let err = parse_verb("tint-group zzzz").err().expect("bad verb must fail");
        assert!(err.contains("bad hex color"), "{err}");
        let err = try_parse(&["--verb", "frobnicate"]).err().expect("bad verb must fail");
        assert!(err.to_string().contains("unknown/malformed --verb"), "{err}");
    }

    #[test]
    fn verb_defaults_and_forms() {
        assert!(matches!(
            parse_verb("recolor-glyph").unwrap(),
            Verb::RecolorGlyph([255, 80, 80])
        ));
        assert!(matches!(
            parse_verb("recolor-line").unwrap(),
            Verb::RecolorLine([255, 213, 79])
        ));
        assert!(matches!(
            parse_verb("nudge-glyph 1 2").unwrap(),
            Verb::NudgeGlyph(v) if v == [1.0, 2.0, 0.0]
        ));
        assert!(matches!(
            parse_verb("move-group 1 2 3").unwrap(),
            Verb::MoveGroup(v) if v == [1.0, 2.0, 3.0]
        ));
        // Every channel is asserted, and the literal is chosen so that no
        // channel can be right by accident. `ff0080` could not do this job:
        // 0xFF survives `& -> |`, 0x00 survives `>> -> <<`, and blue was never
        // checked at all — seven distinct mutations of the hex decoder hid
        // behind one badly-chosen colour, all of them invisible to the full
        // twelve-gate battery. 0x12/0x34/0x56 are distinct, and none is 0 or
        // 255, so a wrong mask, a wrong shift or a wrong divisor all move a
        // value this test reads.
        let Verb::TintGroup(v) = parse_verb("tint-group 123456").unwrap() else {
            panic!("tint-group did not parse to TintGroup");
        };
        assert!((v[0] - 18.0 / 255.0).abs() < 1e-6, "red channel: {v:?}");
        assert!((v[1] - 52.0 / 255.0).abs() < 1e-6, "green channel: {v:?}");
        assert!((v[2] - 86.0 / 255.0).abs() < 1e-6, "blue channel: {v:?}");

        // The explicit-colour form, which is what makes the `t.len() > 1`
        // arity checks falsifiable: with only the bare verb tested, `>` and `<`
        // both fall through to the default and the check is unobservable.
        assert!(matches!(
            parse_verb("recolor-glyph aabbcc").unwrap(),
            Verb::RecolorGlyph([0xAA, 0xBB, 0xCC])
        ));
        assert!(matches!(
            parse_verb("recolor-line 0a141e").unwrap(),
            Verb::RecolorLine([0x0A, 0x14, 0x1E])
        ));
        assert!(matches!(parse_verb("show-group").unwrap(), Verb::SetHidden(false)));
        assert!(matches!(parse_verb("toggle-hidden").unwrap(), Verb::ToggleHidden));
        assert!(matches!(
            parse_verb("scale-glyph 2.5").unwrap(),
            Verb::ScaleGlyph(s) if s == 2.5
        ));
        assert!(matches!(
            parse_verb("scale-group 0.5").unwrap(),
            Verb::ScaleGroup(s) if s == 0.5
        ));
    }

    #[test]
    fn screenshot_frame_flags_parse() {
        // Stage K (K6): the windowed capture pair parses together.
        let cli = parse(&["--screenshot-frame", "90", "--screenshot-out", "/tmp/shot.png"]);
        assert_eq!(cli.screenshot_frame, Some(90));
        assert_eq!(cli.screenshot_out, Some(PathBuf::from("/tmp/shot.png")));
        // They require each other.
        assert!(try_parse(&["--screenshot-frame", "90"]).is_err());
        assert!(try_parse(&["--screenshot-out", "/tmp/shot.png"]).is_err());
        // Windowed-only: clap rejects combining them with offscreen --screenshot.
        assert!(try_parse(&[
            "--screenshot", "out.png", "--screenshot-frame", "90", "--screenshot-out", "/tmp/s.png",
        ])
        .is_err());
    }

    #[test]
    fn generate_flag_parses() {
        let cli = parse(&["--generate", "bash"]);
        assert_eq!(cli.generate, Some(clap_complete::Shell::Bash));
        assert!(try_parse(&["--generate", "tcsh"]).is_err());
    }

    #[test]
    fn negative_numbers_in_cam_pose_and_pick_px() {
        // The old parser took the next raw tokens unconditionally; clap needs
        // allow_negative_numbers to match that for oblique camera repros.
        let cli = parse(&["--cam-pose", "-1", "-2", "-3", "-180", "-45"]);
        match &cli.ops[0] {
            Op::CamPose(p, yaw, pitch) => {
                assert_eq!(*p, [-1.0, -2.0, -3.0]);
                assert!((yaw - (-180f32).to_radians()).abs() < 1e-6);
                assert!((pitch - (-45f32).to_radians()).abs() < 1e-6);
            }
            _ => panic!("expected CamPose"),
        }
    }
}
