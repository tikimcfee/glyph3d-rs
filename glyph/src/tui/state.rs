//! The launcher's state: every choice it offers, the focus model, the
//! persisted config, and the renderer arguments a state builds.

use serde::Deserialize;
use std::path::PathBuf;

use crate::paths::root;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    Repo,
    File,
    Agent,
    Demo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Shelf,
    Carrel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    Back,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Syntax,
    Flat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterMode {
    Cluster,
    Leader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoEngine {
    Hyper,
    Direct,
    Batch,
}

impl RepoEngine {
    pub fn description(self) -> &'static str {
        match self {
            RepoEngine::Hyper => {
                "Parallel CPU Rayon layout writing unified memory. Cache-blocked CPU fold, sub-second repo loading."
            }
            RepoEngine::Direct => {
                "Direct CPU layout path bypassing intermediate wire records (single-threaded direct arena write)."
            }
            RepoEngine::Batch => {
                "Batched sequential CPU layout engine path writing intermediate wire records."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    Fifo,
    Mailbox,
    Immediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMode {
    Instanced,
    Derived,
    /// Experimental (`--field-mode visible`, M2 of `out/VISIBLE-MODE.md`):
    /// no slot per glyph; the lines in view are laid out per frame on the GPU.
    Visible,
}

impl FieldMode {
    pub fn description(self) -> &'static str {
        match self {
            FieldMode::Instanced => {
                "32 B RenderSlot per glyph (pre-computed 3D world coordinates X, Y, Z, color, size, UV)."
            }
            FieldMode::Derived => {
                "20 B DerivedSlot per glyph (X, row, glyph/wrap, color, group). Y/Z dynamically derived on GPU in vertex shader."
            }
            FieldMode::Visible => {
                "EXPERIMENTAL: no slot per glyph. Source bytes + line table resident on the GPU; the lines in view are laid out per frame (F8 HUD shows the tiers)."
            }
        }
    }

    /// The `--field-mode` spelling (`glyph_field::GlyphFieldMode::as_str`).
    pub fn cli_name(self) -> &'static str {
        match self {
            FieldMode::Instanced => "instanced",
            FieldMode::Derived => "derived",
            FieldMode::Visible => "visible",
        }
    }

    /// The launch config's spelling, case-insensitive; anything else is the
    /// CLI default (instanced), as the renderer's own parse falls back.
    pub fn from_config(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "derived" => FieldMode::Derived,
            "visible" => FieldMode::Visible,
            _ => FieldMode::Instanced,
        }
    }

    /// The three-way cycle (◄/► and Enter), like `RepoEngine`'s.
    pub fn next(self) -> Self {
        match self {
            FieldMode::Instanced => FieldMode::Derived,
            FieldMode::Derived => FieldMode::Visible,
            FieldMode::Visible => FieldMode::Instanced,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            FieldMode::Instanced => FieldMode::Visible,
            FieldMode::Derived => FieldMode::Instanced,
            FieldMode::Visible => FieldMode::Derived,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusField {
    TargetMode,
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
    Greeking,
    FileBackgrounds,
    Cull,
    FieldMode,
    UiOverlay,
    PresentMode,
}

impl FocusField {
    pub const ALL: &'static [FocusField] = &[
        FocusField::TargetMode,
        FocusField::RepoPath,
        FocusField::FocusFile,
        FocusField::FilePath,
        FocusField::SessionPath,
        FocusField::LayoutMode,
        FocusField::WrapMode,
        FocusField::ZWrapSpacing,
        FocusField::RepoEngine,
        FocusField::ColorMode,
        FocusField::ClusterMode,
        FocusField::Greeking,
        FocusField::FileBackgrounds,
        FocusField::Cull,
        FocusField::FieldMode,
        FocusField::UiOverlay,
        FocusField::PresentMode,
    ];

    pub fn is_text_input(self) -> bool {
        matches!(
            self,
            FocusField::RepoPath
                | FocusField::FocusFile
                | FocusField::FilePath
                | FocusField::SessionPath
        )
    }

    pub fn next(self, target: TargetMode) -> Self {
        let mut idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        loop {
            idx = (idx + 1) % Self::ALL.len();
            let candidate = Self::ALL[idx];
            if candidate.is_applicable(target) {
                return candidate;
            }
        }
    }

    pub fn prev(self, target: TargetMode) -> Self {
        let mut idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        loop {
            idx = if idx == 0 { Self::ALL.len() - 1 } else { idx - 1 };
            let candidate = Self::ALL[idx];
            if candidate.is_applicable(target) {
                return candidate;
            }
        }
    }

    pub fn is_applicable(self, target: TargetMode) -> bool {
        match self {
            FocusField::RepoPath | FocusField::FocusFile | FocusField::LayoutMode
            | FocusField::RepoEngine => target == TargetMode::Repo,
            FocusField::FilePath => target == TargetMode::File,
            FocusField::SessionPath => target == TargetMode::Agent,
            FocusField::ZWrapSpacing => target != TargetMode::Demo,
            _ => true,
        }
    }
}

/// Repo presets the launcher cycles through when `launch_config.toml` names
/// none. Both are repo-relative, so they exist in every checkout. A machine
/// with its own corpora lists them under `repo_presets` in its launch config.
pub const DEFAULT_REPO_PRESETS: &[&str] = &[".", "native/fixtures/g-pick-repo"];

use glyph_session_dirs::{Harness, SessionDir};

/// Most-recent transcripts the launcher offers on the session field.
const SESSION_PICKS: usize = 20;

#[derive(Debug, Deserialize, Default)]
struct FileLaunchConfig {
    file_backgrounds: Option<bool>,
    wrap_mode: Option<String>,
    z_wrap_spacing: Option<f64>,
    cluster_mode: Option<String>,
    color_mode: Option<String>,
    no_cull: Option<bool>,
    no_ui: Option<bool>,
    greeking: Option<bool>,
    load_repo: Option<String>,
    repo_engine: Option<String>,
    field_mode: Option<String>,
    agent_session: Option<String>,
    repo_presets: Option<Vec<String>>,
    claude_projects_dir: Option<String>,
    antigravity_brain_dir: Option<String>,
    kimi_sessions_dir: Option<String>,
}

pub struct LauncherState {
    pub target: TargetMode,
    pub repo_path: String,
    pub focus_file: String,
    pub file_path: String,
    pub session_path: String,
    pub layout_mode: LayoutMode,
    pub wrap_mode: WrapMode,
    pub color_mode: ColorMode,
    pub cluster_mode: ClusterMode,
    pub repo_engine: RepoEngine,
    pub present_mode: PresentMode,
    pub file_backgrounds: bool,
    pub greeking: bool,
    pub cull: bool,
    pub field_mode: FieldMode,
    pub ui_overlay: bool,
    pub z_wrap_spacing: f64,
    pub focus: FocusField,
    pub status_message: String,
    pub config_source: Option<String>,
    /// What ◄/► cycles through on the repo field.
    pub repo_presets: Vec<String>,
    /// Per-app session locations from the launch config (unset = app default,
    /// empty = off). Resolved into `session_dirs` by `load_config_file`.
    pub session_overrides: glyph_session_dirs::Overrides,
    /// Where session discovery looks. Empty in `defaults()`, so tests never
    /// scan the machine; `new()` resolves it against this machine.
    pub session_dirs: Vec<SessionDir>,
}

impl LauncherState {
    /// The newest transcripts across every resolved session directory, newest
    /// first. Each harness lays its transcripts out differently:
    /// - Claude Code: `<root>/<project>/<id>.jsonl`
    /// - Antigravity: `<root>/<id>/.system_generated/logs/transcript.jsonl`
    /// - Kimi Code:   `<root>/<workspace>/<session>/agents/main/wire.jsonl`
    pub fn discover_agent_sessions(&self) -> Vec<String> {
        let subdirs = |dir: &std::path::Path| -> Vec<PathBuf> {
            std::fs::read_dir(dir)
                .map(|it| it.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
                .unwrap_or_default()
        };
        let mut transcripts = Vec::new();
        for dir in &self.session_dirs {
            match dir.harness {
                Harness::ClaudeCode => {
                    for project in subdirs(&dir.path) {
                        if let Ok(files) = std::fs::read_dir(&project) {
                            transcripts.extend(
                                files
                                    .flatten()
                                    .map(|f| f.path())
                                    .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl")),
                            );
                        }
                    }
                }
                Harness::Antigravity => transcripts.extend(
                    subdirs(&dir.path)
                        .into_iter()
                        .map(|conv| conv.join(".system_generated/logs/transcript.jsonl")),
                ),
                Harness::KimiCode => {
                    for workspace in subdirs(&dir.path) {
                        transcripts.extend(
                            subdirs(&workspace).into_iter().map(|s| s.join("agents/main/wire.jsonl")),
                        );
                    }
                }
            }
        }
        let mut found: Vec<_> = transcripts
            .into_iter()
            .filter_map(|p| Some((p.metadata().ok().filter(|m| m.is_file())?.modified().ok()?, p)))
            .collect();
        found.sort_by_key(|a| std::cmp::Reverse(a.0));
        found.into_iter().take(SESSION_PICKS).map(|(_, p)| p.display().to_string()).collect()
    }

    /// One line: which session directories are scanned, and why.
    pub fn session_dirs_report(&self) -> String {
        glyph_session_dirs::describe(&self.session_dirs)
    }

    /// The launcher with built-in defaults only: reads no file, scans no
    /// directory. Tests start here so they do not depend on the machine.
    pub fn defaults() -> Self {
        Self {
            target: TargetMode::Repo,
            repo_path: ".".to_string(),
            focus_file: String::new(),
            file_path: "native/src/main.rs".to_string(),
            session_path: String::new(),
            layout_mode: LayoutMode::Shelf,
            wrap_mode: WrapMode::Back,
            color_mode: ColorMode::Syntax,
            cluster_mode: ClusterMode::Cluster,
            repo_engine: RepoEngine::Hyper,
            present_mode: PresentMode::Fifo,
            file_backgrounds: true,
            greeking: true,
            cull: true,
            field_mode: FieldMode::Derived,
            ui_overlay: true,
            z_wrap_spacing: 0.15,
            focus: FocusField::TargetMode,
            status_message: "Ready to launch".to_string(),
            config_source: None,
            repo_presets: DEFAULT_REPO_PRESETS.iter().map(|p| p.to_string()).collect(),
            session_overrides: glyph_session_dirs::Overrides::default(),
            session_dirs: Vec::new(),
        }
    }

    /// The launcher as a user sees it: built-in defaults, then the launch
    /// config, then the newest discovered session if the config named none.
    pub fn new() -> Self {
        let mut state = Self::defaults();
        state.load_config_file();
        if state.session_path.is_empty() {
            if let Some(newest) = state.discover_agent_sessions().into_iter().next() {
                state.session_path = newest;
            }
        }
        state
    }

    pub fn load_config_file(&mut self) {
        let candidates = [
            PathBuf::from("launch_config.toml"),
            root().join("launch_config.toml"),
        ];
        for path in &candidates {
            if path.is_file() {
                if let Ok(content) = std::fs::read_to_string(path) {
                    if let Ok(cfg) = toml::from_str::<FileLaunchConfig>(&content) {
                        if let Some(fb) = cfg.file_backgrounds {
                            self.file_backgrounds = fb;
                        }
                        if let Some(zw) = cfg.z_wrap_spacing {
                            self.z_wrap_spacing = zw;
                        }
                        if let Some(wm) = cfg.wrap_mode.as_deref() {
                            self.wrap_mode = match wm {
                                "down" => WrapMode::Down,
                                _ => WrapMode::Back,
                            };
                        }
                        if let Some(cm) = cfg.color_mode.as_deref() {
                            self.color_mode = match cm {
                                "flat" => ColorMode::Flat,
                                _ => ColorMode::Syntax,
                            };
                        }
                        if let Some(cl) = cfg.cluster_mode.as_deref() {
                            self.cluster_mode = match cl {
                                "leader" => ClusterMode::Leader,
                                _ => ClusterMode::Cluster,
                            };
                        }
                        if let Some(nc) = cfg.no_cull {
                            self.cull = !nc;
                        }
                        if let Some(nu) = cfg.no_ui {
                            self.ui_overlay = !nu;
                        }
                        if let Some(gr) = cfg.greeking {
                            self.greeking = gr;
                        }
                        if let Some(lr) = cfg.load_repo {
                            self.repo_path = lr;
                        }
                        if let Some(re) = cfg.repo_engine.as_deref() {
                            self.repo_engine = match re.to_lowercase().as_str() {
                                "direct" => RepoEngine::Direct,
                                "batch" => RepoEngine::Batch,
                                _ => RepoEngine::Hyper,
                            };
                        }
                        if let Some(fm) = cfg.field_mode.as_deref() {
                            self.field_mode = FieldMode::from_config(fm);
                        }
                        if let Some(asess) = cfg.agent_session {
                            if !asess.trim().is_empty() {
                                self.session_path = glyph_session_dirs::expand_home(std::path::Path::new(&asess))
                                    .display()
                                    .to_string();
                            }
                        }
                        if let Some(presets) = cfg.repo_presets {
                            self.repo_presets = presets;
                        }
                        self.session_overrides = glyph_session_dirs::Overrides {
                            claude: cfg.claude_projects_dir.map(PathBuf::from),
                            antigravity: cfg.antigravity_brain_dir.map(PathBuf::from),
                            kimi: cfg.kimi_sessions_dir.map(PathBuf::from),
                        };
                        self.session_dirs = glyph_session_dirs::resolve(&self.session_overrides);
                        self.config_source = Some(path.display().to_string());
                        self.status_message = format!("Loaded defaults from {}", path.display());
                        return;
                    }
                }
            }
        }
        self.config_source = None;
        self.session_overrides = glyph_session_dirs::Overrides::default();
        self.session_dirs = glyph_session_dirs::resolve(&self.session_overrides);
    }

    pub fn build_cli_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        match self.target {
            TargetMode::Repo => {
                args.push("--load-repo".to_string());
                args.push(if self.repo_path.trim().is_empty() {
                    ".".to_string()
                } else {
                    self.repo_path.clone()
                });
                if !self.focus_file.trim().is_empty() {
                    args.push("--focus-file".to_string());
                    args.push(self.focus_file.clone());
                }
                match self.layout_mode {
                    LayoutMode::Shelf => {
                        args.push("--layout-mode".to_string());
                        args.push("shelf".to_string());
                    }
                    LayoutMode::Carrel => {
                        args.push("--layout-mode".to_string());
                        args.push("carrel".to_string());
                    }
                }
                args.push("--repo-engine".to_string());
                args.push(match self.repo_engine {
                    RepoEngine::Hyper => "hyper".to_string(),
                    RepoEngine::Direct => "direct".to_string(),
                    RepoEngine::Batch => "batch".to_string(),
                });
            }
            TargetMode::File => {
                args.push("--render-file".to_string());
                args.push(if self.file_path.trim().is_empty() {
                    "native/src/main.rs".to_string()
                } else {
                    self.file_path.clone()
                });
            }
            TargetMode::Agent => {
                args.push("--agent-session".to_string());
                args.push(if self.session_path.trim().is_empty() {
                    self.discover_agent_sessions()
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "transcript.jsonl".to_string())
                } else {
                    self.session_path.clone()
                });
            }
            TargetMode::Demo => {
                args.push("--demo".to_string());
            }
        }

        if self.target != TargetMode::Demo {
            args.push("--z-wrap-spacing".to_string());
            args.push(format!("{:.2}", self.z_wrap_spacing));
        }

        args.push("--wrap-mode".to_string());
        args.push(match self.wrap_mode {
            WrapMode::Back => "back".to_string(),
            WrapMode::Down => "down".to_string(),
        });

        args.push("--color-mode".to_string());
        args.push(match self.color_mode {
            ColorMode::Syntax => "syntax".to_string(),
            ColorMode::Flat => "flat".to_string(),
        });

        args.push("--cluster-mode".to_string());
        args.push(match self.cluster_mode {
            ClusterMode::Cluster => "cluster".to_string(),
            ClusterMode::Leader => "leader".to_string(),
        });

        args.push("--field-mode".to_string());
        args.push(self.field_mode.cli_name().to_string());

        if self.file_backgrounds {
            args.push("--file-backgrounds".to_string());
        }
        if !self.greeking {
            args.push("--no-greeking".to_string());
        }
        if !self.cull {
            args.push("--no-cull".to_string());
        }
        if !self.ui_overlay {
            args.push("--no-ui".to_string());
        }
        if self.present_mode != PresentMode::Fifo {
            args.push("--present-mode".to_string());
            args.push(match self.present_mode {
                PresentMode::Fifo => "fifo".to_string(),
                PresentMode::Mailbox => "mailbox".to_string(),
                PresentMode::Immediate => "immediate".to_string(),
            });
        }

        args
    }

    pub fn command_preview(&self) -> String {
        let args = self.build_cli_args();
        format!("glyph3d-native {}", args.join(" "))
    }

    pub fn toggle_current(&mut self) {
        match self.focus {
            FocusField::TargetMode => {
                self.target = match self.target {
                    TargetMode::Repo => TargetMode::File,
                    TargetMode::File => TargetMode::Agent,
                    TargetMode::Agent => TargetMode::Demo,
                    TargetMode::Demo => TargetMode::Repo,
                };
            }
            FocusField::ZWrapSpacing => {
                self.z_wrap_spacing = match (self.z_wrap_spacing * 100.0).round() as i32 {
                    10 => 0.15,
                    15 => 0.25,
                    25 => 0.50,
                    50 => 0.10,
                    _ => 0.15,
                };
            }
            FocusField::LayoutMode => {
                self.layout_mode = match self.layout_mode {
                    LayoutMode::Shelf => LayoutMode::Carrel,
                    LayoutMode::Carrel => LayoutMode::Shelf,
                };
            }
            FocusField::WrapMode => {
                self.wrap_mode = match self.wrap_mode {
                    WrapMode::Back => WrapMode::Down,
                    WrapMode::Down => WrapMode::Back,
                };
            }
            FocusField::RepoEngine => {
                self.repo_engine = match self.repo_engine {
                    RepoEngine::Hyper => RepoEngine::Direct,
                    RepoEngine::Direct => RepoEngine::Batch,
                    RepoEngine::Batch => RepoEngine::Hyper,
                };
            }
            FocusField::ColorMode => {
                self.color_mode = match self.color_mode {
                    ColorMode::Syntax => ColorMode::Flat,
                    ColorMode::Flat => ColorMode::Syntax,
                };
            }
            FocusField::ClusterMode => {
                self.cluster_mode = match self.cluster_mode {
                    ClusterMode::Cluster => ClusterMode::Leader,
                    ClusterMode::Leader => ClusterMode::Cluster,
                };
            }
            FocusField::PresentMode => {
                self.present_mode = match self.present_mode {
                    PresentMode::Fifo => PresentMode::Mailbox,
                    PresentMode::Mailbox => PresentMode::Immediate,
                    PresentMode::Immediate => PresentMode::Fifo,
                };
            }
            FocusField::FileBackgrounds => {
                self.file_backgrounds = !self.file_backgrounds;
            }
            FocusField::Greeking => {
                self.greeking = !self.greeking;
            }
            FocusField::Cull => {
                self.cull = !self.cull;
            }
            FocusField::FieldMode => {
                self.field_mode = self.field_mode.next();
            }
            FocusField::UiOverlay => {
                self.ui_overlay = !self.ui_overlay;
            }
            FocusField::RepoPath | FocusField::FocusFile | FocusField::FilePath | FocusField::SessionPath => {}
        }
    }

    fn no_sessions_message(&self) -> String {
        if self.session_dirs.is_empty() {
            self.session_dirs_report()
        } else {
            format!("No transcripts in: {}", self.session_dirs_report())
        }
    }

    pub fn cycle_next(&mut self) {
        match self.focus {
            FocusField::TargetMode => {
                self.target = match self.target {
                    TargetMode::Repo => TargetMode::File,
                    TargetMode::File => TargetMode::Agent,
                    TargetMode::Agent => TargetMode::Demo,
                    TargetMode::Demo => TargetMode::Repo,
                };
            }
            FocusField::RepoPath => {
                if self.repo_presets.is_empty() {
                    return;
                }
                let idx = self.repo_presets.iter().position(|p| *p == self.repo_path);
                let next_idx = match idx {
                    Some(i) => (i + 1) % self.repo_presets.len(),
                    None => 0,
                };
                self.repo_path = self.repo_presets[next_idx].clone();
            }
            FocusField::SessionPath => {
                let sessions = self.discover_agent_sessions();
                if sessions.is_empty() {
                    self.status_message = self.no_sessions_message();
                } else {
                    let idx = sessions.iter().position(|s| s == &self.session_path);
                    let next_idx = match idx {
                        Some(i) => (i + 1) % sessions.len(),
                        None => 0,
                    };
                    self.session_path = sessions[next_idx].clone();
                }
            }
            FocusField::ZWrapSpacing => {
                self.z_wrap_spacing = ((self.z_wrap_spacing + 0.05) * 100.0).round() / 100.0;
                if self.z_wrap_spacing > 2.0 {
                    self.z_wrap_spacing = 2.0;
                }
            }
            FocusField::LayoutMode => {
                self.layout_mode = LayoutMode::Carrel;
            }
            FocusField::WrapMode => {
                self.wrap_mode = WrapMode::Down;
            }
            FocusField::RepoEngine => {
                self.repo_engine = match self.repo_engine {
                    RepoEngine::Hyper => RepoEngine::Direct,
                    RepoEngine::Direct => RepoEngine::Batch,
                    RepoEngine::Batch => RepoEngine::Hyper,
                };
            }
            FocusField::ColorMode => {
                self.color_mode = ColorMode::Flat;
            }
            FocusField::ClusterMode => {
                self.cluster_mode = ClusterMode::Leader;
            }
            FocusField::PresentMode => {
                self.present_mode = match self.present_mode {
                    PresentMode::Fifo => PresentMode::Mailbox,
                    PresentMode::Mailbox => PresentMode::Immediate,
                    PresentMode::Immediate => PresentMode::Fifo,
                };
            }
            FocusField::FileBackgrounds => self.file_backgrounds = true,
            FocusField::Greeking => self.greeking = true,
            FocusField::Cull => self.cull = true,
            FocusField::FieldMode => self.field_mode = self.field_mode.next(),
            FocusField::UiOverlay => self.ui_overlay = true,
            FocusField::FocusFile | FocusField::FilePath => {}
        }
    }

    pub fn cycle_prev(&mut self) {
        match self.focus {
            FocusField::TargetMode => {
                self.target = match self.target {
                    TargetMode::Repo => TargetMode::Demo,
                    TargetMode::File => TargetMode::Repo,
                    TargetMode::Agent => TargetMode::File,
                    TargetMode::Demo => TargetMode::Agent,
                };
            }
            FocusField::RepoPath => {
                if self.repo_presets.is_empty() {
                    return;
                }
                let idx = self.repo_presets.iter().position(|p| *p == self.repo_path);
                let prev_idx = match idx {
                    Some(i) => if i == 0 { self.repo_presets.len() - 1 } else { i - 1 },
                    None => self.repo_presets.len() - 1,
                };
                self.repo_path = self.repo_presets[prev_idx].clone();
            }
            FocusField::SessionPath => {
                let sessions = self.discover_agent_sessions();
                if sessions.is_empty() {
                    self.status_message = self.no_sessions_message();
                } else {
                    let idx = sessions.iter().position(|s| s == &self.session_path);
                    let prev_idx = match idx {
                        Some(i) => if i == 0 { sessions.len() - 1 } else { i - 1 },
                        None => sessions.len() - 1,
                    };
                    self.session_path = sessions[prev_idx].clone();
                }
            }
            FocusField::ZWrapSpacing => {
                self.z_wrap_spacing = ((self.z_wrap_spacing - 0.05) * 100.0).round() / 100.0;
                if self.z_wrap_spacing < 0.01 {
                    self.z_wrap_spacing = 0.01;
                }
            }
            FocusField::LayoutMode => {
                self.layout_mode = LayoutMode::Shelf;
            }
            FocusField::WrapMode => {
                self.wrap_mode = WrapMode::Back;
            }
            FocusField::RepoEngine => {
                self.repo_engine = match self.repo_engine {
                    RepoEngine::Hyper => RepoEngine::Batch,
                    RepoEngine::Direct => RepoEngine::Hyper,
                    RepoEngine::Batch => RepoEngine::Direct,
                };
            }
            FocusField::ColorMode => {
                self.color_mode = ColorMode::Syntax;
            }
            FocusField::ClusterMode => {
                self.cluster_mode = ClusterMode::Cluster;
            }
            FocusField::PresentMode => {
                self.present_mode = match self.present_mode {
                    PresentMode::Fifo => PresentMode::Immediate,
                    PresentMode::Mailbox => PresentMode::Fifo,
                    PresentMode::Immediate => PresentMode::Mailbox,
                };
            }
            FocusField::FileBackgrounds => self.file_backgrounds = false,
            FocusField::Greeking => self.greeking = false,
            FocusField::Cull => self.cull = false,
            FocusField::FieldMode => self.field_mode = self.field_mode.prev(),
            FocusField::UiOverlay => self.ui_overlay = false,
            FocusField::FocusFile | FocusField::FilePath => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_launcher_state_initial_build_args() {
        let state = LauncherState::defaults();
        let args = state.build_cli_args();
        assert!(args.contains(&"--load-repo".to_string()));
        assert!(args.contains(&"--wrap-mode".to_string()));
        assert!(args.contains(&"--color-mode".to_string()));
        assert!(args.contains(&"--z-wrap-spacing".to_string()));
    }

    #[test]
    fn test_target_mode_cycle() {
        let mut state = LauncherState::defaults();
        assert_eq!(state.target, TargetMode::Repo);
        state.focus = FocusField::TargetMode;
        state.toggle_current();
        assert_eq!(state.target, TargetMode::File);
        state.toggle_current();
        assert_eq!(state.target, TargetMode::Agent);
        state.toggle_current();
        assert_eq!(state.target, TargetMode::Demo);
        state.toggle_current();
        assert_eq!(state.target, TargetMode::Repo);
    }

    #[test]
    fn test_focus_navigation_skips_inapplicable() {
        let mut focus = FocusField::TargetMode;
        // In Repo mode, FilePath and SessionPath should be skipped
        let mut visited = Vec::new();
        for _ in 0..FocusField::ALL.len() {
            focus = focus.next(TargetMode::Repo);
            visited.push(focus);
        }
        assert!(!visited.contains(&FocusField::FilePath));
        assert!(!visited.contains(&FocusField::SessionPath));
        assert!(visited.contains(&FocusField::RepoPath));
        assert!(visited.contains(&FocusField::ZWrapSpacing));

        // In File mode, RepoPath, FocusFile, and SessionPath should be skipped
        focus = FocusField::TargetMode;
        visited.clear();
        for _ in 0..FocusField::ALL.len() {
            focus = focus.next(TargetMode::File);
            visited.push(focus);
        }
        assert!(!visited.contains(&FocusField::RepoPath));
        assert!(!visited.contains(&FocusField::FocusFile));
        assert!(!visited.contains(&FocusField::SessionPath));
        assert!(visited.contains(&FocusField::FilePath));

        // In Agent mode, RepoPath, FocusFile, and FilePath should be skipped
        focus = FocusField::TargetMode;
        visited.clear();
        for _ in 0..FocusField::ALL.len() {
            focus = focus.next(TargetMode::Agent);
            visited.push(focus);
        }
        assert!(!visited.contains(&FocusField::RepoPath));
        assert!(!visited.contains(&FocusField::FocusFile));
        assert!(!visited.contains(&FocusField::FilePath));
        assert!(visited.contains(&FocusField::SessionPath));
    }

    #[test]
    fn test_command_preview_format() {
        let mut state = LauncherState::defaults();
        state.target = TargetMode::Demo;
        let preview = state.command_preview();
        assert!(preview.starts_with("glyph3d-native --demo"));
    }

    #[test]
    fn test_agent_session_build_args() {
        let mut state = LauncherState::defaults();
        state.target = TargetMode::Agent;
        state.session_path = "/path/to/my_transcript.jsonl".to_string();
        let args = state.build_cli_args();
        assert!(args.contains(&"--agent-session".to_string()));
        assert!(args.contains(&"/path/to/my_transcript.jsonl".to_string()));
        assert!(args.contains(&"--z-wrap-spacing".to_string()));
    }

    #[test]
    fn test_z_wrap_spacing_dial() {
        let mut state = LauncherState::defaults();
        state.focus = FocusField::ZWrapSpacing;
        state.z_wrap_spacing = 0.15;
        state.cycle_next();
        assert_eq!(state.z_wrap_spacing, 0.20);
        state.cycle_prev();
        assert_eq!(state.z_wrap_spacing, 0.15);
        state.toggle_current();
        assert_eq!(state.z_wrap_spacing, 0.25);
    }

    #[test]
    fn test_repo_preset_cycling() {
        let mut state = LauncherState::defaults();
        state.focus = FocusField::RepoPath;
        state.repo_path = ".".to_string();
        state.cycle_next();
        assert_eq!(state.repo_path, "native/fixtures/g-pick-repo");
        state.cycle_next();
        assert_eq!(state.repo_path, ".");

        // A configured list replaces the built-ins, and an empty one is inert.
        state.repo_presets = vec!["a".to_string(), "b".to_string()];
        state.cycle_next();
        assert_eq!(state.repo_path, "a");
        state.cycle_prev();
        assert_eq!(state.repo_path, "b");
        state.repo_presets.clear();
        state.cycle_next();
        assert_eq!(state.repo_path, "b");
    }

    #[test]
    fn test_session_discovery_reads_each_harness_layout() {
        let mut state = LauncherState::defaults();
        assert!(state.session_dirs.is_empty());
        assert!(state.discover_agent_sessions().is_empty());

        let root = std::env::temp_dir().join(format!("glyph-tui-sessions-{}", std::process::id()));
        let claude = root.join("claude/some-project/s.jsonl");
        let agy = root.join("agy/conv-1/.system_generated/logs/transcript.jsonl");
        let kimi = root.join("kimi/wd_p_1/session_2/agents/main/wire.jsonl");
        for f in [&claude, &agy, &kimi] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "{}\n").unwrap();
        }
        let dir = |harness, sub: &str| SessionDir {
            harness,
            path: root.join(sub),
            origin: glyph_session_dirs::Origin::Config,
        };
        state.session_dirs =
            vec![dir(Harness::ClaudeCode, "claude"), dir(Harness::Antigravity, "agy"), dir(Harness::KimiCode, "kimi")];
        let mut found = state.discover_agent_sessions();
        found.sort();
        let mut want: Vec<String> = [&claude, &agy, &kimi].iter().map(|p| p.display().to_string()).collect();
        want.sort();
        assert_eq!(found, want);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_config_keys_for_presets_and_session_dirs() {
        let cfg: FileLaunchConfig = toml::from_str(
            "repo_presets = [\".\", \"../corpus\"]\n\
             claude_projects_dir = \"/x/claude\"\n\
             antigravity_brain_dir = \"~/brain\"\n\
             kimi_sessions_dir = \"\"\n",
        )
        .unwrap();
        assert_eq!(cfg.repo_presets, Some(vec![".".to_string(), "../corpus".to_string()]));
        assert_eq!(cfg.claude_projects_dir.as_deref(), Some("/x/claude"));
        assert_eq!(cfg.antigravity_brain_dir.as_deref(), Some("~/brain"));
        assert_eq!(cfg.kimi_sessions_dir.as_deref(), Some(""));
    }

    #[test]
    fn test_directional_selection_and_toggles() {
        let mut state = LauncherState::defaults();

        // LayoutMode: Left = Shelf, Right = Carrel
        state.focus = FocusField::LayoutMode;
        state.cycle_next();
        assert_eq!(state.layout_mode, LayoutMode::Carrel);
        state.cycle_prev();
        assert_eq!(state.layout_mode, LayoutMode::Shelf);

        // RepoEngine: 3-way cycle
        state.focus = FocusField::RepoEngine;
        assert_eq!(state.repo_engine, RepoEngine::Hyper);
        state.cycle_next();
        assert_eq!(state.repo_engine, RepoEngine::Direct);
        state.cycle_next();
        assert_eq!(state.repo_engine, RepoEngine::Batch);
        state.cycle_prev();
        assert_eq!(state.repo_engine, RepoEngine::Direct);
        state.cycle_prev();
        assert_eq!(state.repo_engine, RepoEngine::Hyper);

        // Booleans: Left = false, Right = true
        state.focus = FocusField::Greeking;
        state.cycle_prev();
        assert!(!state.greeking);
        state.cycle_next();
        assert!(state.greeking);

        // FieldMode: 3-way cycle like RepoEngine (Instanced → Derived →
        // Visible → Instanced; ◄ walks it backwards)
        state.focus = FocusField::FieldMode;
        assert_eq!(state.field_mode, FieldMode::Derived, "the launcher's default");
        state.cycle_prev();
        assert_eq!(state.field_mode, FieldMode::Instanced);
        state.cycle_next();
        assert_eq!(state.field_mode, FieldMode::Derived);
        state.cycle_next();
        assert_eq!(state.field_mode, FieldMode::Visible);
        state.cycle_next();
        assert_eq!(state.field_mode, FieldMode::Instanced);
        state.cycle_prev();
        assert_eq!(state.field_mode, FieldMode::Visible);
        state.toggle_current();
        assert_eq!(state.field_mode, FieldMode::Instanced);
        state.toggle_current();
        assert_eq!(state.field_mode, FieldMode::Derived);
    }

    /// Every launcher mode reaches the renderer under the renderer's own
    /// spelling, and the config's spelling round-trips (unknown → instanced,
    /// as the renderer's parse falls back).
    #[test]
    fn test_field_mode_names_and_build_args() {
        for mode in [FieldMode::Instanced, FieldMode::Derived, FieldMode::Visible] {
            assert_eq!(FieldMode::from_config(mode.cli_name()), mode);
            assert_eq!(FieldMode::from_config(&mode.cli_name().to_uppercase()), mode);
            let mut state = LauncherState::defaults();
            state.field_mode = mode;
            let args = state.build_cli_args();
            let at = args.iter().position(|a| a == "--field-mode").expect("--field-mode is always passed");
            assert_eq!(args[at + 1], mode.cli_name());
        }
        assert_eq!(FieldMode::from_config("vertexy"), FieldMode::Instanced);
        let mut state = LauncherState::defaults();
        state.field_mode = FieldMode::Visible;
        assert!(state.build_cli_args().contains(&"visible".to_string()));
    }

    #[test]
    fn test_focus_field_visual_order_matches_all() {
        // Ensure ALL starts with TargetMode and ends with PresentMode
        assert_eq!(FocusField::ALL[0], FocusField::TargetMode);
        assert_eq!(*FocusField::ALL.last().unwrap(), FocusField::PresentMode);
        // Verify Next and Prev wrap around cleanly
        let next_wrap = FocusField::PresentMode.next(TargetMode::Repo);
        assert_eq!(next_wrap, FocusField::TargetMode);
        let prev_wrap = FocusField::TargetMode.prev(TargetMode::Repo);
        assert_eq!(prev_wrap, FocusField::PresentMode);
    }

    #[test]
    fn test_repo_engine_descriptions() {
        assert_eq!(
            RepoEngine::Hyper.description(),
            "Parallel CPU Rayon layout writing unified memory. Cache-blocked CPU fold, sub-second repo loading."
        );
        assert_eq!(
            RepoEngine::Direct.description(),
            "Direct CPU layout path bypassing intermediate wire records (single-threaded direct arena write)."
        );
        assert_eq!(
            RepoEngine::Batch.description(),
            "Batched sequential CPU layout engine path writing intermediate wire records."
        );
    }

    #[test]
    fn test_field_mode_descriptions() {
        assert_eq!(
            FieldMode::Instanced.description(),
            "32 B RenderSlot per glyph (pre-computed 3D world coordinates X, Y, Z, color, size, UV)."
        );
        assert_eq!(
            FieldMode::Derived.description(),
            "20 B DerivedSlot per glyph (X, row, glyph/wrap, color, group). Y/Z dynamically derived on GPU in vertex shader."
        );
        assert!(FieldMode::Visible.description().starts_with("EXPERIMENTAL"));
        assert!(FieldMode::Visible.description().contains("no slot per glyph"));
    }
}
