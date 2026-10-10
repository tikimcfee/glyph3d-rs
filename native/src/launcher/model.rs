//! The launcher's state: a `LaunchConfig` (the file it loaded, edited by
//! the controls) over the CLI's own defaults, and the UI state around it.
//!
//! There is no second copy of an option here. A choice row lists its enum's
//! clap `ValueEnum` variants, spelled and described by the derive the flag
//! uses; a value the user has not touched shows the CLI default (a `Cli`
//! parsed from no arguments); and what launches is the config itself,
//! written by the struct that reads it (`LaunchConfig::to_toml_string`).

use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};

use crate::agent_transcript::discovery::{scan_session_dirs, DiscoveredSession};
use crate::cli::Cli;
use crate::launch_config::{value_name, LaunchConfig};

/// Repo presets the repo field cycles through when the config names none.
/// Both are repo-relative, so they exist in every checkout.
pub const DEFAULT_REPO_PRESETS: &[&str] = &[".", "native/fixtures/g-pick-repo"];

/// Most-recent transcripts the session field cycles through.
const SESSION_PICKS: usize = 20;

/// Which scene a launch opens — the config's scene keys, one at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Repo,
    File,
    Agent,
    Demo,
}

impl Target {
    pub const ALL: [Target; 4] = [Target::Repo, Target::File, Target::Agent, Target::Demo];

    pub fn label(self) -> &'static str {
        match self {
            Target::Repo => "Repo",
            Target::File => "File",
            Target::Agent => "Agent",
            Target::Demo => "Demo",
        }
    }
}

/// Which panel a control is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Target,
    Layout,
    Graphics,
}

/// Every control, in focus (and drawing) order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Target,
    RepoPath,
    FocusFile,
    FilePath,
    SessionPath,
    LayoutMode,
    WrapMode,
    ZWrapSpacing,
    RepoEngine,
    ColorMode,
    ClusterMode,
    FieldMode,
    DebugTint,
    TextDetail,
    ShowGlyphs,
    Greeking,
    FileBackgrounds,
    Cull,
    UiOverlay,
    PresentMode,
}

impl Control {
    pub const ALL: &'static [Control] = &[
        Control::Target,
        Control::RepoPath,
        Control::FocusFile,
        Control::FilePath,
        Control::SessionPath,
        Control::LayoutMode,
        Control::WrapMode,
        Control::ZWrapSpacing,
        Control::RepoEngine,
        Control::ColorMode,
        Control::ClusterMode,
        Control::FieldMode,
        Control::DebugTint,
        Control::TextDetail,
        Control::ShowGlyphs,
        Control::Greeking,
        Control::FileBackgrounds,
        Control::Cull,
        Control::UiOverlay,
        Control::PresentMode,
    ];

    pub fn section(self) -> Section {
        use Control::*;
        match self {
            Target | RepoPath | FocusFile | FilePath | SessionPath => Section::Target,
            LayoutMode | WrapMode | ZWrapSpacing | RepoEngine | ColorMode | ClusterMode => Section::Layout,
            FieldMode | DebugTint | TextDetail | ShowGlyphs | Greeking | FileBackgrounds | Cull | UiOverlay
            | PresentMode => Section::Graphics,
        }
    }

    pub fn label(self) -> &'static str {
        use Control::*;
        match self {
            Target => "Target",
            RepoPath => "Repo",
            FocusFile => "Focus",
            FilePath => "File",
            SessionPath => "Session",
            LayoutMode => "Layout",
            WrapMode => "Wrap",
            ZWrapSpacing => "Z spacing",
            RepoEngine => "Engine",
            ColorMode => "Color",
            ClusterMode => "Cluster",
            FieldMode => "Field mode",
            DebugTint => "Debug tint",
            TextDetail => "Text detail",
            ShowGlyphs => "Show glyphs",
            Greeking => "Greeking",
            FileBackgrounds => "File cards",
            Cull => "Culling",
            UiOverlay => "Egui overlay",
            PresentMode => "Present",
        }
    }

    pub fn is_text_input(self) -> bool {
        matches!(self, Control::RepoPath | Control::FocusFile | Control::FilePath | Control::SessionPath)
    }
}

/// One choice of a choice row: its CLI name, its help, whether it is the
/// current value.
#[derive(Clone, Debug, PartialEq)]
pub struct Opt {
    pub name: String,
    pub help: Option<String>,
    pub selected: bool,
}

/// What a control shows.
#[derive(Clone, Debug, PartialEq)]
pub enum View {
    Choice(Vec<Opt>),
    Toggle(bool),
    /// A number, or `None` when it is the renderer's config default (shown
    /// with that default).
    Dial { value: Option<f64>, default: f64, unit: &'static str },
    Text { value: String, placeholder: &'static str },
}

fn options<T: ValueEnum + PartialEq>(current: &T) -> Vec<Opt> {
    T::value_variants()
        .iter()
        .map(|v| Opt {
            name: value_name(v),
            help: v.to_possible_value().and_then(|p| p.get_help().map(|h| h.to_string())),
            selected: v == current,
        })
        .collect()
}

fn step_enum<T: ValueEnum + PartialEq + Clone>(current: &T, dir: i32) -> T {
    let all = T::value_variants();
    let i = all.iter().position(|v| v == current).unwrap_or(0) as i32;
    all[(i + dir).rem_euclid(all.len() as i32) as usize].clone()
}

/// The launcher.
pub struct Launcher {
    /// The options as loaded and edited. Scene keys live in the fields below
    /// until a launch writes the chosen one back.
    pub cfg: LaunchConfig,
    /// The CLI with no arguments: every default a control shows.
    defaults: Cli,
    /// `[lod]` as the renderer would resolve it under `cfg`'s sections.
    lod_defaults: (f64, f64),
    pub target: Target,
    pub repo_path: String,
    pub file_path: String,
    pub session_path: String,
    pub focus: Control,
    pub status: String,
    /// The file the options came from, if any.
    pub config_source: Option<PathBuf>,
    /// A config that failed to load: shown, and launching is refused until
    /// it loads (`r`), because the launch would silently drop it.
    pub config_error: Option<String>,
    /// Newest first; scanned on first use, and again on reload.
    sessions: Option<Vec<DiscoveredSession>>,
    /// The file the last launch wrote, kept so a launch can be repeated.
    pub last_launch: Option<PathBuf>,
}

impl Launcher {
    /// Built-in defaults only: reads no file and scans no directory (tests).
    pub fn defaults() -> Self {
        let defaults = Cli::try_parse_from(["glyph3d-native"]).expect("the CLI parses with no arguments");
        let mut l = Self {
            cfg: LaunchConfig::default(),
            defaults,
            lod_defaults: (0.0, 0.0),
            target: Target::Repo,
            repo_path: ".".to_string(),
            file_path: crate::cli::default_text_file().display().to_string(),
            session_path: String::new(),
            focus: Control::Target,
            status: "Ready to launch".to_string(),
            config_source: None,
            config_error: None,
            sessions: Some(Vec::new()),
            last_launch: None,
        };
        l.resolve_lod_defaults();
        l
    }

    /// The launcher as a user opens it: the given config file, else
    /// `launch_config.toml` where the renderer looks for it.
    pub fn open(explicit: Option<PathBuf>) -> Self {
        let mut l = Self::defaults();
        l.sessions = None;
        l.load(explicit);
        l
    }

    fn resolve_lod_defaults(&mut self) {
        let lod = crate::config::Settings::with_overrides(self.cfg.settings.clone())
            .map(|s| s.lod)
            .unwrap_or_else(|_| crate::config::Settings::with_overrides(toml::Table::new()).expect("defaults load").lod);
        self.lod_defaults = (lod.text_detail_px as f64, lod.show_glyphs_px as f64);
    }

    /// (Re)load the config. The scene keys become the target and its path.
    pub fn load(&mut self, explicit: Option<PathBuf>) {
        let path = explicit.or_else(|| {
            [Path::new("launch_config.toml"), Path::new("../launch_config.toml")]
                .into_iter()
                .find(|p| p.is_file())
                .map(Path::to_path_buf)
        });
        self.sessions = None;
        let Some(path) = path else {
            self.config_source = None;
            self.config_error = None;
            self.status = "No launch_config.toml: CLI defaults".to_string();
            return;
        };
        match LaunchConfig::from_file(&path) {
            Ok(cfg) => {
                self.adopt(cfg);
                self.config_error = None;
                self.status = format!("Loaded {}", path.display());
            }
            Err(e) => {
                self.config_error = Some(e);
                self.status = "Config did not load: fix it and press [R]".to_string();
            }
        }
        self.config_source = Some(path);
    }

    /// Take a loaded config as the launcher's state.
    pub fn adopt(&mut self, mut cfg: LaunchConfig) {
        self.target = if cfg.demo == Some(true) {
            Target::Demo
        } else if cfg.agent_session.is_some() {
            Target::Agent
        } else if cfg.load_repo.is_some() {
            Target::Repo
        } else if cfg.render_file.is_some() {
            Target::File
        } else {
            Target::Repo
        };
        if let Some(p) = cfg.load_repo.take() {
            self.repo_path = p.display().to_string();
        }
        if let Some(p) = cfg.render_file.take() {
            self.file_path = p.display().to_string();
        }
        if let Some(p) = cfg.agent_session.take() {
            self.session_path = crate::launch_config::expand_home(&p).display().to_string();
        }
        cfg.demo = None;
        self.cfg = cfg;
        self.resolve_lod_defaults();
    }

    pub fn repo_presets(&self) -> Vec<String> {
        self.cfg
            .repo_presets
            .clone()
            .unwrap_or_else(|| DEFAULT_REPO_PRESETS.iter().map(|p| p.to_string()).collect())
    }

    pub fn is_applicable(&self, c: Control) -> bool {
        match c {
            Control::RepoPath | Control::FocusFile | Control::LayoutMode | Control::RepoEngine => {
                self.target == Target::Repo
            }
            Control::FilePath => self.target == Target::File,
            Control::SessionPath => self.target == Target::Agent,
            Control::ZWrapSpacing | Control::WrapMode | Control::ColorMode | Control::ClusterMode => {
                self.target != Target::Demo
            }
            _ => true,
        }
    }

    pub fn focus_next(&mut self, dir: i32) {
        let all = Control::ALL;
        let mut i = all.iter().position(|&c| c == self.focus).unwrap_or(0) as i32;
        loop {
            i = (i + dir).rem_euclid(all.len() as i32);
            if self.is_applicable(all[i as usize]) {
                self.focus = all[i as usize];
                return;
            }
        }
    }

    /// What `c` shows now.
    pub fn view(&self, c: Control) -> View {
        let d = &self.defaults;
        let cfg = &self.cfg;
        match c {
            Control::Target => View::Choice(
                Target::ALL
                    .iter()
                    .map(|t| Opt { name: t.label().to_string(), help: None, selected: *t == self.target })
                    .collect(),
            ),
            Control::RepoPath => View::Text { value: self.repo_path.clone(), placeholder: "." },
            Control::FocusFile => View::Text {
                value: cfg.focus_file.clone().unwrap_or_default(),
                placeholder: "(all files)",
            },
            Control::FilePath => View::Text { value: self.file_path.clone(), placeholder: "(this crate's main.rs)" },
            Control::SessionPath => View::Text { value: self.session_path.clone(), placeholder: "(◄/► picks a recent one)" },
            Control::LayoutMode => View::Choice(options(&cfg.layout_mode.unwrap_or(d.layout_mode))),
            Control::WrapMode => View::Choice(options(&cfg.wrap_mode.unwrap_or(d.wrap_mode))),
            Control::ZWrapSpacing => View::Dial {
                value: Some(cfg.z_wrap_spacing.unwrap_or(d.z_wrap_spacing)),
                default: d.z_wrap_spacing,
                unit: "x em",
            },
            Control::RepoEngine => View::Choice(options(&cfg.repo_engine.unwrap_or(d.repo_engine))),
            Control::ColorMode => View::Choice(options(&cfg.color_mode.unwrap_or(d.color_mode))),
            Control::ClusterMode => View::Choice(options(&cfg.cluster_mode.unwrap_or(d.cluster_mode))),
            Control::FieldMode => View::Choice(options(&cfg.field_mode.unwrap_or(d.field_mode))),
            Control::DebugTint => View::Choice(options(&cfg.debug_tint.unwrap_or(d.debug_tint))),
            Control::TextDetail => View::Dial {
                value: cfg.text_detail_px.map(f64::from),
                default: self.lod_defaults.0,
                unit: "px/row",
            },
            Control::ShowGlyphs => View::Dial {
                value: cfg.show_glyphs_px.map(f64::from),
                default: self.lod_defaults.1,
                unit: "px/row",
            },
            Control::Greeking => View::Toggle(cfg.greeking.unwrap_or(!d.no_greeking)),
            Control::FileBackgrounds => View::Toggle(cfg.file_backgrounds.unwrap_or(d.file_backgrounds)),
            Control::Cull => View::Toggle(!cfg.no_cull.unwrap_or(d.no_cull)),
            Control::UiOverlay => View::Toggle(!cfg.no_ui.unwrap_or(d.no_ui)),
            Control::PresentMode => View::Choice(options(&cfg.present_mode.unwrap_or(d.present_mode))),
        }
    }

    /// ◄ (-1) / ► (+1) on the focused control: the previous/next choice, a
    /// dial step, a preset or a recent session, a toggle flipped.
    pub fn step(&mut self, dir: i32) {
        let d = &self.defaults;
        let cfg = &mut self.cfg;
        match self.focus {
            Control::Target => {
                let i = Target::ALL.iter().position(|t| *t == self.target).unwrap_or(0) as i32;
                self.target = Target::ALL[(i + dir).rem_euclid(4) as usize];
            }
            Control::RepoPath => {
                let presets = self.repo_presets();
                if presets.is_empty() {
                    return;
                }
                let n = presets.len() as i32;
                let i = match presets.iter().position(|p| *p == self.repo_path) {
                    Some(i) => (i as i32 + dir).rem_euclid(n),
                    None if dir > 0 => 0,
                    None => n - 1,
                };
                self.repo_path = presets[i as usize].clone();
            }
            Control::SessionPath => self.step_session(dir),
            Control::FocusFile | Control::FilePath => {}
            Control::LayoutMode => cfg.layout_mode = Some(step_enum(&cfg.layout_mode.unwrap_or(d.layout_mode), dir)),
            Control::WrapMode => cfg.wrap_mode = Some(step_enum(&cfg.wrap_mode.unwrap_or(d.wrap_mode), dir)),
            Control::RepoEngine => cfg.repo_engine = Some(step_enum(&cfg.repo_engine.unwrap_or(d.repo_engine), dir)),
            Control::ColorMode => cfg.color_mode = Some(step_enum(&cfg.color_mode.unwrap_or(d.color_mode), dir)),
            Control::ClusterMode => cfg.cluster_mode = Some(step_enum(&cfg.cluster_mode.unwrap_or(d.cluster_mode), dir)),
            Control::FieldMode => cfg.field_mode = Some(step_enum(&cfg.field_mode.unwrap_or(d.field_mode), dir)),
            Control::DebugTint => cfg.debug_tint = Some(step_enum(&cfg.debug_tint.unwrap_or(d.debug_tint), dir)),
            Control::PresentMode => {
                cfg.present_mode = Some(step_enum(&cfg.present_mode.unwrap_or(d.present_mode), dir))
            }
            Control::ZWrapSpacing => {
                let v = cfg.z_wrap_spacing.unwrap_or(d.z_wrap_spacing);
                cfg.z_wrap_spacing = Some(dial(v, dir, 0.05, 0.0, 2.0));
            }
            Control::TextDetail => {
                let v = cfg.text_detail_px.map(f64::from).unwrap_or(self.lod_defaults.0);
                cfg.text_detail_px = Some(dial(v, dir, 1.0, 1.0, 64.0) as f32);
            }
            Control::ShowGlyphs => {
                let v = cfg.show_glyphs_px.map(f64::from).unwrap_or(self.lod_defaults.1);
                cfg.show_glyphs_px = Some(dial(v, dir, 0.25, 0.0, 32.0) as f32);
            }
            Control::Greeking | Control::FileBackgrounds | Control::Cull | Control::UiOverlay => self.toggle(),
        }
    }

    /// Space: flip a toggle, or step a choice forward.
    pub fn toggle(&mut self) {
        let d = &self.defaults;
        let cfg = &mut self.cfg;
        match self.focus {
            Control::Greeking => cfg.greeking = Some(!cfg.greeking.unwrap_or(!d.no_greeking)),
            Control::FileBackgrounds => cfg.file_backgrounds = Some(!cfg.file_backgrounds.unwrap_or(d.file_backgrounds)),
            Control::Cull => cfg.no_cull = Some(!cfg.no_cull.unwrap_or(d.no_cull)),
            Control::UiOverlay => cfg.no_ui = Some(!cfg.no_ui.unwrap_or(d.no_ui)),
            c if c.is_text_input() => {}
            _ => self.step(1),
        }
    }

    /// The focused text field, for typing into.
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Control::RepoPath => Some(&mut self.repo_path),
            Control::FilePath => Some(&mut self.file_path),
            Control::SessionPath => Some(&mut self.session_path),
            Control::FocusFile => Some(self.cfg.focus_file.get_or_insert_with(String::new)),
            _ => None,
        }
    }

    /// The recent sessions, scanned once (the scan reads each transcript's
    /// head for its title).
    pub fn sessions(&mut self) -> &[DiscoveredSession] {
        if self.sessions.is_none() {
            let mut found = scan_session_dirs(&self.cfg.session_dirs());
            found.truncate(SESSION_PICKS);
            self.sessions = Some(found);
        }
        self.sessions.as_deref().unwrap_or_default()
    }

    /// The selected session's title and project, when it is a scanned one.
    pub fn session_title(&self) -> Option<String> {
        let s = self.sessions.as_ref()?.iter().find(|s| s.path.display().to_string() == self.session_path)?;
        Some(match &s.project_name {
            Some(p) => format!("{} — {}", p, s.title),
            None => s.title.clone(),
        })
    }

    fn step_session(&mut self, dir: i32) {
        let current = self.session_path.clone();
        let paths: Vec<String> = self.sessions().iter().map(|s| s.path.display().to_string()).collect();
        if paths.is_empty() {
            let dirs = self.cfg.session_dirs();
            self.status = if dirs.is_empty() {
                glyph_session_dirs::describe(&dirs)
            } else {
                format!("No transcripts in: {}", glyph_session_dirs::describe(&dirs))
            };
            return;
        }
        let n = paths.len() as i32;
        let i = match paths.iter().position(|p| *p == current) {
            Some(i) => (i as i32 + dir).rem_euclid(n),
            None if dir > 0 => 0,
            None => n - 1,
        };
        self.session_path = paths[i as usize].clone();
    }

    pub fn session_dirs_report(&self) -> String {
        glyph_session_dirs::describe(&self.cfg.session_dirs())
    }

    /// The config a launch writes: the options as edited, plus the chosen
    /// scene's one key, its path made absolute against `cwd` (`run` resolves
    /// paths against where you typed it; the file outlives that).
    pub fn launch_config(&self, cwd: &Path) -> LaunchConfig {
        let abs = |s: &str, fallback: &str| -> PathBuf {
            let s = if s.trim().is_empty() { fallback } else { s.trim() };
            let p = crate::launch_config::expand_home(Path::new(s));
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        };
        let mut cfg = self.cfg.clone();
        if cfg.focus_file.as_deref().is_some_and(|f| f.trim().is_empty()) {
            cfg.focus_file = None;
        }
        match self.target {
            Target::Repo => cfg.load_repo = Some(abs(&self.repo_path, ".")),
            Target::File => cfg.render_file = Some(abs(&self.file_path, &crate::cli::default_text_file().display().to_string())),
            Target::Agent => cfg.agent_session = Some(abs(&self.session_path, "transcript.jsonl")),
            Target::Demo => cfg.demo = Some(true),
        }
        if self.target != Target::Repo {
            cfg.focus_file = None;
        }
        cfg
    }
}

fn dial(v: f64, dir: i32, step: f64, lo: f64, hi: f64) -> f64 {
    let stepped = ((v / step).round() + dir as f64) * step;
    // Two decimals: 0.15 + 0.05 is 0.2, not 0.20000000000000001.
    ((stepped.clamp(lo, hi)) * 100.0).round() / 100.0
}
