use std::path::{Path, PathBuf};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use super::ops::{build_ops, parse_verb, Op};
use super::parsers::*;
use crate::glyph_scene::Verb;


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
    /// Load an agent transcript session (JSONL from Claude Code or Antigravity) into an Agent Carrel
    #[arg(long, value_name = "PATH")]
    pub agent_session: Option<PathBuf>,
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
    /// Syntax color mode on repo load: `syntax` (eager CPU lexer during load)
    /// or `flat` (fast geometric load with extension-based LOD tint, awaiting external colorization)
    #[arg(long, value_name = "MODE", default_value = "syntax", value_parser = ["syntax", "flat"])]
    pub color_mode: String,
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
    /// Path to launch configuration file (default: looks for launch_config.toml if present)
    #[arg(long, value_name = "PATH")]
    pub launch_config: Option<PathBuf>,
    /// Render file background cards behind glyph fields
    #[arg(long)]
    pub file_backgrounds: bool,
    /// Disable Greeking subpixel glyphs into stable horizontal ink bars
    #[arg(long)]
    pub no_greeking: bool,
    /// Force gradual blend for Greeking (default is pure fast bypass)
    #[arg(long)]
    pub greek_smooth: bool,
    /// Enable pure/hard Greeking bypass (active by default)
    #[arg(long)]
    pub greek_pure: bool,
    /// On-screen glyph height in px/em where Greeking begins (default: 10.0)
    #[arg(long, value_name = "F")]
    pub greek_onset_px: Option<f32>,
    /// File background card RGBA color (e.g. "0.10,0.10,0.13,0.85")
    #[arg(long, value_name = "R,G,B,A", value_parser = parse_rgba)]
    pub file_bg_color: Option<[f32; 4]>,
    /// Override the initial LOD minimum pixel threshold (default: 1.0)
    #[arg(long, value_name = "F")]
    pub lod_min_px: Option<f32>,
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


pub fn parse_cli_from(matches: clap::ArgMatches) -> Cli {
    let mut cli = Cli::from_arg_matches(&matches).expect("clap derive round-trip");
    cli.ops = build_ops(&matches, &cli.raw_ops);

    let config_path = if let Some(path) = &cli.launch_config {
        Some(path.clone())
    } else if !cfg!(test) && cli.screenshot.is_none() && Path::new("launch_config.toml").is_file() {
        Some(PathBuf::from("launch_config.toml"))
    } else if !cfg!(test) && cli.screenshot.is_none() && Path::new("../launch_config.toml").is_file() {
        Some(PathBuf::from("../launch_config.toml"))
    } else {
        None
    };

    if let Some(path) = config_path {
        match crate::launch_config::LaunchConfig::from_file(&path) {
            Ok(cfg) => {
                if matches.value_source("file_backgrounds")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(fb) = cfg.file_backgrounds {
                        cli.file_backgrounds = fb;
                    }
                }
                if matches.value_source("file_bg_color")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(col) = cfg.file_bg_color {
                        cli.file_bg_color = Some(col);
                    }
                }
                if matches.value_source("lod_min_px")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(lod) = cfg.lod_min_px {
                        cli.lod_min_px = Some(lod);
                    }
                }
                if matches.value_source("wrap_mode")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(wm) = cfg.wrap_mode {
                        cli.wrap_mode = wm;
                    }
                }
                if matches.value_source("z_wrap_spacing")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(zw) = cfg.z_wrap_spacing {
                        cli.z_wrap_spacing = zw;
                    }
                }
                if matches.value_source("cluster_mode")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(cm) = cfg.cluster_mode {
                        cli.cluster_mode = cm;
                    }
                }
                if matches.value_source("color_mode")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(cm) = cfg.color_mode {
                        cli.color_mode = cm;
                    }
                }
                if matches.value_source("no_cull")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(nc) = cfg.no_cull {
                        cli.no_cull = nc;
                    }
                }
                if matches.value_source("no_ui")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(nu) = cfg.no_ui {
                        cli.no_ui = nu;
                    }
                }
                if matches.value_source("no_greeking")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(greek) = cfg.greeking {
                        cli.no_greeking = !greek;
                    }
                }
                if matches.value_source("greek_pure")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(pure) = cfg.greek_pure {
                        cli.greek_pure = pure;
                    }
                }
                if matches.value_source("greek_smooth")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(smooth) = cfg.greek_smooth {
                        cli.greek_smooth = smooth;
                    }
                }
                if matches.value_source("greek_onset_px")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(onset) = cfg.greek_onset_px {
                        cli.greek_onset_px = Some(onset);
                    }
                }
                if matches.value_source("load_repo")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(lr) = cfg.load_repo {
                        cli.load_repo = Some(lr);
                    }
                }
                if matches.value_source("agent_session")
                    != Some(clap::parser::ValueSource::CommandLine)
                {
                    if let Some(session) = cfg.agent_session {
                        cli.agent_session = Some(session);
                    }
                }
            }
            Err(e) => {
                log::warn!("{e}");
            }
        }
    }

    if let Some(session) = &cli.agent_session {
        cli.agent_session = Some(crate::launch_config::expand_home(session));
    }

    cli
}


pub fn parse_cli() -> Cli {
    parse_cli_from(Cli::command().get_matches())
}

pub fn default_text_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")
}

