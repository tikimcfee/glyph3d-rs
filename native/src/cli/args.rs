use std::path::{Path, PathBuf};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use super::ops::{build_ops, parse_verb, Op};
use super::parsers::*;
use crate::glyph_scene::Verb;

/// Windowed presentation mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum PresentMode {
    #[default]
    Fifo,
    Mailbox,
    Immediate,
}

impl From<PresentMode> for wgpu::PresentMode {
    fn from(m: PresentMode) -> Self {
        match m {
            PresentMode::Fifo => wgpu::PresentMode::Fifo,
            PresentMode::Mailbox => wgpu::PresentMode::Mailbox,
            PresentMode::Immediate => wgpu::PresentMode::Immediate,
        }
    }
}

impl std::fmt::Display for PresentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresentMode::Fifo => write!(f, "fifo"),
            PresentMode::Mailbox => write!(f, "mailbox"),
            PresentMode::Immediate => write!(f, "immediate"),
        }
    }
}

impl std::str::FromStr for PresentMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fifo" => Ok(PresentMode::Fifo),
            "mailbox" => Ok(PresentMode::Mailbox),
            "immediate" => Ok(PresentMode::Immediate),
            other => Err(format!("unknown present mode {other:?}")),
        }
    }
}

/// The Visible field's debug tint (`--debug-tint`): colour each glyph by the
/// LOD tier its line landed in, or by its item's cull state. A diagnostic
/// lane in the Params uniform; the Instanced and Derived shaders never read
/// it, so it moves no pixel there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum DebugTint {
    #[default]
    Off,
    /// Colour by LOD tier (glyph / wash / backdrop).
    Lod,
    /// Colour by cull state (visible / culled by frustum / culled by LOD).
    Cull,
}

impl DebugTint {
    /// The uniform's encoding (`FramePrepare::debug_tint`).
    pub fn as_u32(self) -> u32 {
        match self {
            DebugTint::Off => 0,
            DebugTint::Lod => 1,
            DebugTint::Cull => 2,
        }
    }
}

/// Long-form help tail: the mode summary + verb reference + windowed keys from
/// the hand-rolled parser's --help (nothing user-facing was dropped).
const AFTER_LONG_HELP: &str = "\
MODES:
  no args                the terminal launcher, on a terminal (--launcher
                         anywhere); otherwise the windowed text field
                         (default file: this crate's main.rs)
  --demo                 windowed: Stage A quad-field demo
  --screenshot PATH      offscreen: render N frames, write PNG, print timing,
                         exit 0. Deterministic (fixed virtual clock).

VERBS (--verb \"V [ARGS]\", repeatable; applies to the most recent pick):
  recolor-glyph [rrggbb] | recolor-line [rrggbb]
  nudge-glyph dx dy [dz] | scale-glyph f
  move-group dx dy dz | scale-group s
  tint-group rrggbb | tint-cycle | hide-group | show-group | toggle-hidden
  set-glyph-background rrggbb[aa] | set-glyph-transform tx ty tz [s] | reset-glyph-group
  (--field-mode visible keys the glyph verbs by item:byte; nudge is x-only
  and scale-glyph is not representable there, as in derived)

WINDOWED MODE:
  fly camera — WASD move, E|R up, Q|F down, RIGHT-drag look, scroll = speed,
  Esc releases | interact: LEFT click = pick glyph, h highlight line,
  g grab file (mouse drags, scroll scales), t tint, x hide |
  F1 toggle Debug panel, F2 save screenshot (out/windowed-shot-*.png;
  GLYPH_POSE_PRINT=1 also prints the frame's --cam-pose), F8 toggle the field HUD |
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
    /// Frames to render before exiting (offscreen and windowed)
    #[arg(long, value_name = "N")]
    pub frames: Option<u32>,
    /// Stage A quad-field demo instead of the text field
    #[arg(long)]
    pub demo: bool,
    /// Text file to stage (UTF-8); default: this crate's main.rs
    #[arg(long, value_name = "PATH")]
    pub render_file: Option<PathBuf>,
    /// Load an agent transcript session (JSONL from Claude Code or Antigravity) into an Agent Carrel
    #[arg(long, value_name = "PATH")]
    pub agent_session: Option<PathBuf>,
    /// Sliding window limit for Agent Carrel turn/beat deck (default 20)
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub deck_window_limit: usize,
    /// Sliding window time scroll offset backward for Agent Carrel deck (default 0)
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub deck_scroll_offset: usize,
    /// Sliding window limit for Agent Carrel workdesk file revisions (default 20)
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub desk_revision_limit: usize,
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
    /// Stage E1: render engine records through the Slug renderer
    #[arg(long, value_name = "PATH")]
    pub engine_render: Option<PathBuf>,
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
    /// HyperLayout against the oracle-backed fold: lay each input out with the
    /// production engine and with `fold.rs` (held to the JS oracle by
    /// `--fixture-fold`), diff records, instances and placements bit-exact,
    /// then exit. Inputs: `.pipe.bin` fixtures (their bytes and item params),
    /// directories (walked as `--load-repo` walks them, in `--cluster-mode`),
    /// or single text files.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub hyper_oracle_check: Vec<PathBuf>,
    /// Build the visible-set line table (Pass 1's per-line index and the
    /// long-line segment seeds, `layout_hyper/line_table.rs`) over the given
    /// inputs and print what it holds — lines, seeds, bytes — then exit. The
    /// same inputs `--hyper-oracle-check` takes.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub line_table_stats: Vec<PathBuf>,
    /// Stage E2: load a whole repository as a field of code pages
    #[arg(long, value_name = "DIR")]
    pub load_repo: Option<PathBuf>,
    /// Which layout engine a repo load uses. `hyper` (the DEFAULT) is the
    /// parallel CPU HyperLayout, writing render instances straight into the
    /// arena with its inputs prefetched in the background. `direct`,
    /// `batch` and `naive` run the same HyperLayout without the prefetch;
    /// under `--repo-verify`, `direct` records no 32 B wire records
    /// (placements and instances are diffed, records are not) while `batch`
    /// and `naive` do.
    #[arg(long, value_name = "MODE", default_value = "hyper")]
    pub repo_engine: crate::repo::Strategy,
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
    #[arg(long, value_name = "MODE", default_value = "back")]
    pub wrap_mode: crate::fold::WrapMode,
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
    #[arg(long, value_name = "MODE", default_value = "cluster")]
    pub cluster_mode: crate::fold::ClusterMode,
    /// Spatial arrangement mode for the repository files across the canvas:
    /// `shelf` (default: height-classed shelves across the whole repo) or
    /// `carrel` (hierarchical directory-based neighborhood carrels).
    #[arg(long, value_name = "MODE", default_value = "shelf")]
    pub layout_mode: crate::repo::RepoLayoutMode,
    /// Syntax color mode on repo load: `flat` (default: fast geometric load with extension-based LOD tint, awaiting external colorization)
    /// or `syntax` (eager CPU lexer during load)
    #[arg(long, value_name = "MODE", default_value = "flat")]
    pub color_mode: crate::repo::ColorMode,
    /// Stage E2: frame the first file whose path contains SUBSTR
    #[arg(long, value_name = "SUBSTR")]
    pub focus_file: Option<String>,
    /// Stage E2: walk + engine + stage + stats, then exit (no GPU)
    #[arg(long)]
    pub repo_scan_only: bool,
    /// Stage F: disable the cull/LOD pass (legacy per-chunk draws; debug/A-B)
    #[arg(long)]
    pub no_cull: bool,
    /// Glyph field render mode: `instanced` (default: one full 32 B placement
    /// record per glyph, read as-is by the vertex stage), `derived` (compact
    /// 20 B record per glyph with vertex-stage Y/Z derivation from line
    /// tables), or `visible` (EXPERIMENTAL, behind this flag: no slot per
    /// glyph — the source bytes and a line table are resident and the lines
    /// in view are laid out per frame on the GPU; `out/VISIBLE-MODE.md`).
    /// Switchable live from the Debug panel's selector (the scene rebuilds);
    /// a load past the Derived slot lanes falls back to `instanced` and says so.
    #[arg(long, value_name = "MODE", default_value = "instanced")]
    pub field_mode: glyph_field::GlyphFieldMode,
    /// Visible field only: tint glyphs by `lod` tier or `cull` state (`off`
    /// by default). The other modes ignore it; the F8 HUD names the active one.
    #[arg(long, value_name = "MODE", default_value = "off")]
    pub debug_tint: DebugTint,
    /// Ground/sky environment behind the glyph scene: `off` or `ground`
    /// (default: `[environment] mode` in config, itself `off`). Windowed:
    /// the B key toggles it live.
    #[arg(long, value_name = "MODE")]
    pub environment: Option<crate::config::EnvironmentMode>,
    /// Explicit ground plane height (world y) for `--environment ground`;
    /// default: the scene's lowest point minus `[environment] ground_gap`.
    #[arg(long, value_name = "Y", allow_negative_numbers = true)]
    pub ground_y: Option<f32>,
    /// Stage K: windowed without the egui UI overlay (exact pre-K behavior)
    #[arg(long)]
    pub no_ui: bool,
    /// Path to launch configuration file (default: looks for launch_config.toml if present)
    #[arg(long, value_name = "PATH")]
    pub launch_config: Option<PathBuf>,
    /// Open the terminal launcher (also what no arguments at all opens, on a
    /// terminal): pick a scene and its options, Enter starts the renderer on
    /// them and the menu returns when the window closes. With
    /// --launch-config, the launcher starts from that file.
    #[arg(long)]
    pub launcher: bool,
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
    /// "Text detail": px per text row at or above which glyphs render with
    /// full curve detail; below it they fuzz progressively (greeking).
    /// Default: the config's lod.text_detail_px (10.0). Was --greek-onset-px, still accepted
    #[arg(long, alias = "greek-onset-px", value_name = "F")]
    pub text_detail_px: Option<f32>,
    /// File background card RGBA color (e.g. "0.10,0.10,0.13,0.85")
    #[arg(long, value_name = "R,G,B,A", value_parser = parse_rgba)]
    pub file_bg_color: Option<[f32; 4]>,
    /// "Show glyphs": px per text row at or above which a file is drawn as
    /// glyphs; below it, a backdrop rectangle (visible mode: a line wash).
    /// Default: the config's lod.show_glyphs_px (1.0). Was --lod-min-px, still accepted
    #[arg(long, alias = "lod-min-px", value_name = "F")]
    pub show_glyphs_px: Option<f32>,
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
    /// Windowed only: how frames reach the display. `fifo` (the default) is
    /// vsync, so the FPS line reads the monitor's refresh; `mailbox` and
    /// `immediate` uncap it where the surface supports them (else fifo, and
    /// the log says so). Offscreen renders never present and ignore this.
    #[arg(long, value_name = "MODE", default_value = "fifo")]
    pub present_mode: PresentMode,
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

/// Whether this run looks for a personal `launch_config.toml` on its own.
/// Only an interactive run does. Screenshot runs never do, and neither does
/// any run that checks or measures (`--repo-verify`, `--repo-scan-only`, the
/// `--fixture-*` and `--hyper-oracle-check` instruments, `--line-table-stats`,
/// `--gpu-key` / `--gpu-profile`, `--generate`): every gate launches the
/// binary as one of these, and a personal override (a color, a wrap mode, a
/// field mode) must not reach what a gate compares. Until 2026-10-10 only
/// `--screenshot` was excluded, and a launcher-saved `field_mode = "visible"`
/// in the checkout's `launch_config.toml` turned repo-verify-direct red
/// (`direct` lays out no instances in visible mode).
pub(crate) fn discovers_launch_config(cli: &Cli) -> bool {
    let checks = cli.repo_verify
        || cli.repo_scan_only
        || cli.gpu_key
        || cli.gpu_profile
        || cli.generate.is_some()
        || !cli.fixture_reference.is_empty()
        || !cli.fixture_trie.is_empty()
        || !cli.fixture_fold.is_empty()
        || !cli.fixture_scan.is_empty()
        || !cli.fixture_bake.is_empty()
        || !cli.hyper_oracle_check.is_empty()
        || !cli.line_table_stats.is_empty();
    cli.screenshot.is_none() && !checks
}

/// Build the `Cli` from clap matches, merging the launch config underneath the
/// command line. An unreadable or invalid launch config is an `Err` — an
/// explicit `--launch-config` that does not load is a mistake, not a default.
pub fn parse_cli_from(matches: clap::ArgMatches) -> Result<Cli, String> {
    let mut cli = Cli::from_arg_matches(&matches).expect("clap derive round-trip");

    let config_path = if let Some(path) = &cli.launch_config {
        Some(path.clone())
    } else if !cfg!(test) && discovers_launch_config(&cli) {
        ["launch_config.toml", "../launch_config.toml"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
    } else {
        None
    };

    if let Some(path) = config_path {
        let cfg = crate::launch_config::LaunchConfig::from_file(&path)?;
        if !cfg.settings.is_empty() {
            crate::config::install(cfg.settings.clone())
                .map_err(|e| format!("launch config '{}': {e}", path.display()))?;
        }
        merge_launch_config(&mut cli, &matches, cfg);
    }

    // After the launch config: building an op can read a setting (a recolor
    // verb's default colour), and config::install refuses once anything has.
    cli.ops = build_ops(&matches, &cli.raw_ops);

    if let Some(session) = &cli.agent_session {
        cli.agent_session = Some(crate::launch_config::expand_home(session));
    }

    Ok(cli)
}

/// The launch config under the command line: a key applies unless its flag
/// was typed. The scene keys (`demo`, `agent_session`, `load_repo`,
/// `render_file`) go as one: a scene typed on the command line is the scene.
/// Until C27 (2026-10-10) they merged one by one, so a file's `load_repo`
/// outranked a typed `--render-file` (the scene precedence is demo > session
/// > repo > file, `Cli::action`).
fn merge_launch_config(cli: &mut Cli, matches: &clap::ArgMatches, cfg: crate::launch_config::LaunchConfig) {
    let typed = |id: &str| matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine);
    // `under!(key)`: the file's value where the flag of the same name was not
    // typed; `Some` for a field the CLI holds as an Option.
    macro_rules! under {
        ($key:ident) => {
            if !typed(stringify!($key)) {
                if let Some(v) = cfg.$key {
                    cli.$key = v;
                }
            }
        };
        (Some $key:ident) => {
            if !typed(stringify!($key)) {
                if let Some(v) = cfg.$key {
                    cli.$key = Some(v);
                }
            }
        };
    }
    under!(file_backgrounds);
    under!(Some file_bg_color);
    under!(Some show_glyphs_px);
    under!(wrap_mode);
    under!(z_wrap_spacing);
    under!(cluster_mode);
    under!(color_mode);
    under!(no_cull);
    under!(no_ui);
    if !typed("no_greeking") {
        if let Some(greek) = cfg.greeking {
            cli.no_greeking = !greek;
        }
    }
    under!(greek_pure);
    under!(greek_smooth);
    under!(Some text_detail_px);
    under!(Some focus_file);
    under!(layout_mode);
    under!(repo_engine);
    under!(field_mode);
    under!(debug_tint);
    under!(present_mode);
    under!(Some frames);
    let scene_typed = ["demo", "agent_session", "load_repo", "render_file", "engine_render"]
        .into_iter()
        .any(typed);
    if !scene_typed {
        under!(demo);
        under!(Some agent_session);
        under!(Some load_repo);
        under!(Some render_file);
    }
}

pub fn parse_cli() -> Cli {
    parse_cli_from(Cli::command().get_matches()).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    })
}

pub fn default_text_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")
}

