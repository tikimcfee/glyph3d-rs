//! Interactive launcher TUI for glyph3d-native.
//!
//! Provides a mission-control terminal interface for configuring launch
//! flags, inspecting hardware/GPU profile and build currency, and launching
//! the 3D windowed renderer with a clean terminal handoff.

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame, Terminal,
};
use serde::Deserialize;
use std::io::{self, stdout, Stdout};
use std::path::PathBuf;
use std::process::Command;

use crate::{gpu_key, gpu_profile, is_current, root, Manifest};

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
    Cubecl,
    Direct,
    Batch,
}

impl RepoEngine {
    pub fn description(self) -> &'static str {
        match self {
            RepoEngine::Hyper => {
                "Parallel CPU Rayon layout writing unified memory. Cache-blocked CPU fold, sub-second repo loading."
            }
            RepoEngine::Cubecl => {
                "Pure GPU parallel compute pipeline (Metal/WGPU) with in-flight UTF-8 decode, parallel Blelloch scan & direct slot emission."
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
                                "cubecl" => RepoEngine::Cubecl,
                                "direct" => RepoEngine::Direct,
                                "batch" => RepoEngine::Batch,
                                _ => RepoEngine::Hyper,
                            };
                        }
                        if let Some(fm) = cfg.field_mode.as_deref() {
                            self.field_mode = match fm.to_lowercase().as_str() {
                                "derived" => FieldMode::Derived,
                                _ => FieldMode::Instanced,
                            };
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
                    RepoEngine::Cubecl => "cubecl".to_string(),
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
        args.push(match self.field_mode {
            FieldMode::Instanced => "instanced".to_string(),
            FieldMode::Derived => "derived".to_string(),
        });

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
                    RepoEngine::Hyper => RepoEngine::Cubecl,
                    RepoEngine::Cubecl => RepoEngine::Direct,
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
                self.field_mode = match self.field_mode {
                    FieldMode::Instanced => FieldMode::Derived,
                    FieldMode::Derived => FieldMode::Instanced,
                };
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
                    RepoEngine::Hyper => RepoEngine::Cubecl,
                    RepoEngine::Cubecl => RepoEngine::Direct,
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
            FocusField::FieldMode => self.field_mode = FieldMode::Derived,
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
                    RepoEngine::Cubecl => RepoEngine::Hyper,
                    RepoEngine::Direct => RepoEngine::Cubecl,
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
            FocusField::FieldMode => self.field_mode = FieldMode::Instanced,
            FocusField::UiOverlay => self.ui_overlay = false,
            FocusField::FocusFile | FocusField::FilePath => {}
        }
    }
}

pub fn run(manifest: &Manifest) -> bool {
    let mut terminal = match init_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("FAIL  could not initialize terminal: {e}");
            return false;
        }
    };

    let mut state = LauncherState::new();
    let res = run_loop(&mut terminal, &mut state, manifest);
    let _ = restore_terminal(&mut terminal);
    res
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut LauncherState,
    manifest: &Manifest,
) -> bool {
    loop {
        if let Err(e) = terminal.draw(|f| draw_ui(f, state, manifest)) {
            eprintln!("FAIL  render error: {e}");
            return false;
        }

        if let Ok(Event::Key(key)) = event::read() {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return true;
                }
                KeyCode::Esc => {
                    return true;
                }
                KeyCode::Char('q') | KeyCode::Char('Q') if !state.focus.is_text_input() => {
                    return true;
                }
                KeyCode::BackTab => {
                    state.focus = state.focus.prev(state.target);
                }
                KeyCode::Tab => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        state.focus = state.focus.prev(state.target);
                    } else {
                        state.focus = state.focus.next(state.target);
                    }
                }
                KeyCode::Down => {
                    state.focus = state.focus.next(state.target);
                }
                KeyCode::Up => {
                    state.focus = state.focus.prev(state.target);
                }
                KeyCode::Left => {
                    state.cycle_prev();
                }
                KeyCode::Right => {
                    state.cycle_next();
                }
                KeyCode::Char(' ') => {
                    if state.focus.is_text_input() {
                        match state.focus {
                            FocusField::RepoPath => state.repo_path.push(' '),
                            FocusField::FocusFile => state.focus_file.push(' '),
                            FocusField::FilePath => state.file_path.push(' '),
                            FocusField::SessionPath => state.session_path.push(' '),
                            _ => {}
                        }
                    } else {
                        state.toggle_current();
                    }
                }
                KeyCode::Char('b') | KeyCode::Char('B') if !state.focus.is_text_input() => {
                    // Build action: build with --features cubecl
                    run_external_action(terminal, "Building glyph3d-native (with --features cubecl)...", || {
                        let _ = Command::new("cargo")
                            .arg("build")
                            .arg("--release")
                            .arg("-p")
                            .arg("glyph3d-native")
                            .arg("--features")
                            .arg("cubecl")
                            .current_dir(root())
                            .status();
                    });
                    state.status_message = "Build completed (release + cubecl).".to_string();
                }
                KeyCode::Char('t') | KeyCode::Char('T') if !state.focus.is_text_input() => {
                    // Test gates
                    run_external_action(terminal, "Running verification gates...", || {
                        let _ = Command::new("cargo")
                            .arg("glyph")
                            .arg("test")
                            .current_dir(root())
                            .status();
                    });
                    state.status_message = "Gate test completed.".to_string();
                }
                KeyCode::Char('r') | KeyCode::Char('R') if !state.focus.is_text_input() => {
                    state.load_config_file();
                }
                KeyCode::Enter => {
                    // Launch windowed renderer
                    let exe = root().join("target/release/glyph3d-native");
                    if !exe.exists() {
                        state.status_message =
                            "Binary not found: press [B] to build first.".to_string();
                        continue;
                    }
                    let args = state.build_cli_args();
                    let cwd = std::env::current_dir().unwrap_or_else(|_| root());

                    run_external_action(terminal, "Launching glyph3d-native...", || {
                        let _ = Command::new(&exe)
                            .args(&args)
                            .current_dir(cwd)
                            .status();
                    });
                    state.status_message = "Returned from glyph3d-native.".to_string();
                }
                KeyCode::Backspace => match state.focus {
                    FocusField::RepoPath => {
                        state.repo_path.pop();
                    }
                    FocusField::FocusFile => {
                        state.focus_file.pop();
                    }
                    FocusField::FilePath => {
                        state.file_path.pop();
                    }
                    FocusField::SessionPath => {
                        state.session_path.pop();
                    }
                    _ => {}
                },
                KeyCode::Char(c) => match state.focus {
                    FocusField::RepoPath => {
                        state.repo_path.push(c);
                    }
                    FocusField::FocusFile => {
                        state.focus_file.push(c);
                    }
                    FocusField::FilePath => {
                        state.file_path.push(c);
                    }
                    FocusField::SessionPath => {
                        state.session_path.push(c);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn run_external_action<F: FnOnce()>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    msg: &str,
    action: F,
) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    println!("\n── {msg}\n");
    action();
    println!("\n[Press Enter to return to launcher...]");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    let _ = enable_raw_mode();
    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    let _ = terminal.hide_cursor();
    let _ = terminal.clear();
}

fn draw_ui(f: &mut Frame, state: &LauncherState, manifest: &Manifest) {
    let size = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(14),   // 2-Column Body
            Constraint::Length(3), // Live command preview
            Constraint::Length(1), // Footer keybinds
        ])
        .split(size);

    draw_header(f, chunks[0]);
    draw_body(f, chunks[1], state, manifest);
    draw_preview(f, chunks[2], state);
    draw_footer(f, chunks[3]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let header_text = vec![
        Line::from(vec![
            Span::styled(" GLYPH3D ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
            Span::styled(" Interactive Launcher & Configuration Mission Control", Style::default().fg(Color::White).bold()),
        ]),
        Line::from(vec![
            Span::styled(" Navigate: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Tab / Up / Down", Style::default().fg(Color::Yellow)),
            Span::styled("   Select/Cycle: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Left / Right / Space", Style::default().fg(Color::Yellow)),
            Span::styled("   Launch: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::Cyan));
    let p = Paragraph::new(header_text).block(block);
    f.render_widget(p, area);
}

fn draw_body(f: &mut Frame, area: Rect, state: &LauncherState, manifest: &Manifest) {
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Left column: Target & Layout
            Constraint::Percentage(50), // Right column: Graphics & Environment
        ])
        .split(area);

    draw_left_column(f, main_chunks[0], state);
    draw_right_column(f, main_chunks[1], state, manifest);
}

fn draw_left_column(f: &mut Frame, area: Rect, state: &LauncherState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // 1. Launch Target
            Constraint::Min(8),    // 2. Layout & Layout Engine
        ])
        .split(area);

    draw_target_section(f, chunks[0], state);
    draw_layout_section(f, chunks[1], state);
}

fn draw_right_column(f: &mut Frame, area: Rect, state: &LauncherState, manifest: &Manifest) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // 3. Graphics & Shading
            Constraint::Min(6),    // 4. Hardware & Environment
        ])
        .split(area);

    draw_graphics_section(f, chunks[0], state);
    draw_sidebar(f, chunks[1], state, manifest);
}

fn line_prefix(focused: bool) -> Span<'static> {
    if focused {
        Span::styled("▶ ", Style::default().fg(Color::Cyan).bold())
    } else {
        Span::raw("  ")
    }
}

fn draw_target_section(f: &mut Frame, area: Rect, state: &LauncherState) {
    let is_focused = state.focus == FocusField::TargetMode;
    let mut lines = Vec::new();

    // Line 1: Target Mode
    lines.push(Line::from(vec![
        line_prefix(is_focused),
        Span::styled(
            "Target: ",
            if is_focused {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        format_radio("Repo", state.target == TargetMode::Repo, is_focused),
        Span::raw(" "),
        format_radio("File", state.target == TargetMode::File, is_focused),
        Span::raw(" "),
        format_radio("Agent", state.target == TargetMode::Agent, is_focused),
        Span::raw(" "),
        format_radio("Demo", state.target == TargetMode::Demo, is_focused),
    ]));

    // Line 2 & 3: Depending on target mode
    match state.target {
        TargetMode::Repo => {
            let repo_focused = state.focus == FocusField::RepoPath;
            let focus_file_focused = state.focus == FocusField::FocusFile;
            lines.push(Line::from(vec![
                line_prefix(repo_focused),
                Span::styled(
                    "Repo:   ",
                    if repo_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{:<22}", if state.repo_path.is_empty() { "." } else { &state.repo_path }),
                    if repo_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
                Span::styled(" (◄/► presets)", Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(vec![
                line_prefix(focus_file_focused),
                Span::styled(
                    "Focus:  ",
                    if focus_file_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{:<25}", if state.focus_file.is_empty() { "(all files)" } else { &state.focus_file }),
                    if focus_file_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
            ]));
        }
        TargetMode::File => {
            let file_focused = state.focus == FocusField::FilePath;
            lines.push(Line::from(vec![
                line_prefix(file_focused),
                Span::styled(
                    "File:   ",
                    if file_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{:<25}", state.file_path),
                    if file_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Staged directly via Slug WGSL pipeline", Style::default().fg(Color::DarkGray)),
            ]));
        }
        TargetMode::Agent => {
            let session_focused = state.focus == FocusField::SessionPath;
            lines.push(Line::from(vec![
                line_prefix(session_focused),
                Span::styled(
                    "Session:",
                    if session_focused {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(" "),
                Span::styled(
                    format!("{:<28}", if state.session_path.is_empty() { "(press ◄/► to pick recent)" } else { &state.session_path }),
                    if session_focused {
                        Style::default().bg(Color::Blue).fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("3D Rolodex deck & Carrel navigation (◄/► recent)", Style::default().fg(Color::DarkGray)),
            ]));
        }
        TargetMode::Demo => {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Stage A 1M quad-field stress demo", Style::default().fg(Color::Magenta)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Evaluates raw GPU fillrate & instancing", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let is_section_focused = matches!(
        state.focus,
        FocusField::TargetMode
            | FocusField::RepoPath
            | FocusField::FocusFile
            | FocusField::FilePath
            | FocusField::SessionPath
    );

    let block = Block::default()
        .title(" 1. Launch Target ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_section_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_layout_section(f: &mut Frame, area: Rect, state: &LauncherState) {
    let is_layout = state.focus == FocusField::LayoutMode;
    let is_wrap = state.focus == FocusField::WrapMode;
    let is_z_spacing = state.focus == FocusField::ZWrapSpacing;
    let is_engine = state.focus == FocusField::RepoEngine;
    let is_color = state.focus == FocusField::ColorMode;
    let is_cluster = state.focus == FocusField::ClusterMode;

    let lines = vec![
        Line::from(vec![
            line_prefix(is_layout),
            Span::styled(
                "Layout:   ",
                if is_layout {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("shelf", state.layout_mode == LayoutMode::Shelf, is_layout),
            Span::raw(" "),
            format_choice("carrel", state.layout_mode == LayoutMode::Carrel, is_layout),
        ]),
        Line::from(vec![
            line_prefix(is_wrap),
            Span::styled(
                "Wrap:     ",
                if is_wrap {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("back (Z-depth)", state.wrap_mode == WrapMode::Back, is_wrap),
            Span::raw(" "),
            format_choice("down (cols)", state.wrap_mode == WrapMode::Down, is_wrap),
        ]),
        Line::from(vec![
            line_prefix(is_z_spacing),
            Span::styled(
                "Z Spacing:",
                if is_z_spacing {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(" "),
            format_dial(state.z_wrap_spacing, is_z_spacing),
            Span::styled(" × em cell (◄/► dial)", Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            line_prefix(is_engine),
            Span::styled(
                "Engine:   ",
                if is_engine {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("hyper (Rayon)", state.repo_engine == RepoEngine::Hyper, is_engine),
            Span::raw(" "),
            format_choice("cubecl (GPU)", state.repo_engine == RepoEngine::Cubecl, is_engine),
            Span::raw(" "),
            format_choice("direct", state.repo_engine == RepoEngine::Direct, is_engine),
            Span::raw(" "),
            format_choice("batch", state.repo_engine == RepoEngine::Batch, is_engine),
        ]),
        Line::from(vec![
            line_prefix(is_color),
            Span::styled(
                "Color:    ",
                if is_color {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("syntax (lexer)", state.color_mode == ColorMode::Syntax, is_color),
            Span::raw(" "),
            format_choice("flat", state.color_mode == ColorMode::Flat, is_color),
        ]),
        Line::from(vec![
            line_prefix(is_cluster),
            Span::styled(
                "Cluster:  ",
                if is_cluster {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("cluster (UAX29)", state.cluster_mode == ClusterMode::Cluster, is_cluster),
            Span::raw(" "),
            format_choice("leader", state.cluster_mode == ClusterMode::Leader, is_cluster),
        ]),
    ];

    let is_section_focused = matches!(
        state.focus,
        FocusField::LayoutMode
            | FocusField::WrapMode
            | FocusField::ZWrapSpacing
            | FocusField::RepoEngine
            | FocusField::ColorMode
            | FocusField::ClusterMode
    );

    let mut block = Block::default()
        .title(" 2. Layout & Layout Engine ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_section_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    if is_engine {
        if let Some(subtitle) = format_subtitle_description(state.repo_engine.description(), area.width) {
            block = block.title_bottom(subtitle);
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_graphics_section(f: &mut Frame, area: Rect, state: &LauncherState) {
    let is_greeking = state.focus == FocusField::Greeking;
    let is_cards = state.focus == FocusField::FileBackgrounds;
    let is_cull = state.focus == FocusField::Cull;
    let is_field_mode = state.focus == FocusField::FieldMode;
    let is_ui = state.focus == FocusField::UiOverlay;
    let is_present = state.focus == FocusField::PresentMode;

    let show_hint = area.width >= 48;

    let lines = vec![
        Line::from(vec![
            line_prefix(is_greeking),
            Span::styled(
                "Greeking:       ",
                if is_greeking {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.greeking, is_greeking),
            if show_hint {
                Span::styled(" (subpixel ink)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_cards),
            Span::styled(
                "File Cards:     ",
                if is_cards {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.file_backgrounds, is_cards),
            if show_hint {
                Span::styled(" (bounds)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_cull),
            Span::styled(
                "Frustum Culling:",
                if is_cull {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.cull, is_cull),
            if show_hint {
                Span::styled(" (AABB cull)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_field_mode),
            Span::styled(
                "Field Mode:     ",
                if is_field_mode {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("instanced (32B)", state.field_mode == FieldMode::Instanced, is_field_mode),
            Span::raw(" "),
            format_choice("derived (20B)", state.field_mode == FieldMode::Derived, is_field_mode),
        ]),
        Line::from(vec![
            line_prefix(is_ui),
            Span::styled(
                "Egui Overlay:   ",
                if is_ui {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_toggle(state.ui_overlay, is_ui),
            if show_hint {
                Span::styled(" (in-app UI)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            line_prefix(is_present),
            Span::styled(
                "Present Mode:   ",
                if is_present {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            format_choice("fifo", state.present_mode == PresentMode::Fifo, is_present),
            Span::raw(" "),
            format_choice("mailbox", state.present_mode == PresentMode::Mailbox, is_present),
            Span::raw(" "),
            format_choice("immediate", state.present_mode == PresentMode::Immediate, is_present),
        ]),
    ];

    let is_section_focused = matches!(
        state.focus,
        FocusField::Greeking
            | FocusField::FileBackgrounds
            | FocusField::Cull
            | FocusField::FieldMode
            | FocusField::UiOverlay
            | FocusField::PresentMode
    );

    let mut block = Block::default()
        .title(" 3. Graphics & Shading ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_section_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    if is_field_mode {
        if let Some(subtitle) = format_subtitle_description(state.field_mode.description(), area.width) {
            block = block.title_bottom(subtitle);
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_sidebar(f: &mut Frame, area: Rect, state: &LauncherState, manifest: &Manifest) {
    let gpu = gpu_key().unwrap_or("unknown");
    let prof = gpu_profile()
        .unwrap_or("unavailable")
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let renderer_art = manifest.artifact.get("renderer");
    let is_curr = renderer_art.map(|a| is_current("renderer", a)).unwrap_or(false);

    let diag_lines = vec![
        Line::from(vec![
            Span::styled("GPU Adapter:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if prof.is_empty() { "Probing..." } else { &prof },
                Style::default().fg(Color::Cyan).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("Golden Key:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(gpu, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Build Status: ", Style::default().fg(Color::DarkGray)),
            if is_curr {
                Span::styled("● CURRENT (up to date)", Style::default().fg(Color::Green).bold())
            } else {
                Span::styled("○ STALE (press B to build)", Style::default().fg(Color::Red).bold())
            },
        ]),
        Line::from(vec![
            Span::styled("Config File:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                state.config_source.as_deref().unwrap_or("None (default flags)"),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("Status:       ", Style::default().fg(Color::DarkGray)),
            Span::styled(&state.status_message, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("Sessions:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(state.session_dirs_report(), Style::default().fg(Color::White)),
        ]),
    ];

    let block = Block::default()
        .title(" 4. Environment & Status ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(diag_lines).block(block), area);
}

fn draw_preview(f: &mut Frame, area: Rect, state: &LauncherState) {
    let cmd = state.command_preview();
    let p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("$ ", Style::default().fg(Color::Green).bold()),
            Span::styled(cmd, Style::default().fg(Color::White).bold()),
        ]),
    ])
    .block(
        Block::default()
            .title(" Live Invocation Preview ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Green)),
    );
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let keys = Line::from(vec![
        Span::styled(" [Enter] ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        Span::styled("Launch  ", Style::default().fg(Color::White)),
        Span::styled(" [B] ", Style::default().bg(Color::Blue).fg(Color::White).bold()),
        Span::styled("Build  ", Style::default().fg(Color::White)),
        Span::styled(" [T] ", Style::default().bg(Color::Magenta).fg(Color::White).bold()),
        Span::styled("Test Gates  ", Style::default().fg(Color::White)),
        Span::styled(" [R] ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
        Span::styled("Reload Config  ", Style::default().fg(Color::White)),
        Span::styled(" [Space] ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::styled("Toggle  ", Style::default().fg(Color::White)),
        Span::styled(" [◄/►] ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::styled("Adjust/Preset  ", Style::default().fg(Color::White)),
        Span::styled(" [Q / Esc] ", Style::default().bg(Color::Red).fg(Color::White).bold()),
        Span::styled("Quit", Style::default().fg(Color::White)),
    ]);
    let p = Paragraph::new(keys).alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn format_radio<'a>(label: &'a str, selected: bool, focused: bool) -> Span<'a> {
    if selected {
        if focused {
            Span::styled(format!("[● {label}]"), Style::default().bg(Color::Cyan).fg(Color::Black).bold())
        } else {
            Span::styled(format!("[● {label}]"), Style::default().fg(Color::Cyan).bold())
        }
    } else {
        Span::styled(format!("[○ {label}]"), Style::default().fg(Color::DarkGray))
    }
}

fn format_choice<'a>(label: &'a str, selected: bool, focused: bool) -> Span<'a> {
    if selected {
        if focused {
            Span::styled(format!("[{label}]"), Style::default().bg(Color::Cyan).fg(Color::Black).bold())
        } else {
            Span::styled(format!("[{label}]"), Style::default().fg(Color::Cyan).bold())
        }
    } else {
        Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray))
    }
}

fn format_dial<'a>(val: f64, focused: bool) -> Span<'a> {
    let text = format!("[◄ {:.2} ►]", val);
    if focused {
        Span::styled(text, Style::default().bg(Color::Cyan).fg(Color::Black).bold())
    } else {
        Span::styled(text, Style::default().fg(Color::Yellow).bold())
    }
}

fn format_toggle(enabled: bool, focused: bool) -> Span<'static> {
    if enabled {
        if focused {
            Span::styled("[● ENABLED]", Style::default().bg(Color::Green).fg(Color::Black).bold())
        } else {
            Span::styled("[● ENABLED]", Style::default().fg(Color::Green).bold())
        }
    } else if focused {
        Span::styled("[○ DISABLED]", Style::default().bg(Color::Red).fg(Color::White).bold())
    } else {
        Span::styled("[○ DISABLED]", Style::default().fg(Color::Red))
    }
}

fn format_subtitle_description(description: &str, area_width: u16) -> Option<Line<'static>> {
    // Require sufficient width to render border corners, margins, icon, and meaningful text.
    if area_width < 18 {
        return None;
    }

    // Border corners (2), border margins (2), prefix " ℹ " (3), suffix " " (1) = 8 columns.
    let available_width = area_width.saturating_sub(8) as usize;
    if available_width < 10 {
        return None;
    }

    let character_count = description.chars().count();
    let text = if character_count <= available_width {
        description.to_string()
    } else {
        let max_characters = available_width.saturating_sub(1);
        let prefix: String = description.chars().take(max_characters).collect();
        format!("{prefix}…")
    };

    Some(Line::from(vec![
        Span::styled(" ℹ ", Style::default().fg(Color::Cyan).bold()),
        Span::styled(format!("{text} "), Style::default().fg(Color::White)),
    ]))
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

        // RepoEngine: 4-way cycle
        state.focus = FocusField::RepoEngine;
        assert_eq!(state.repo_engine, RepoEngine::Hyper);
        state.cycle_next();
        assert_eq!(state.repo_engine, RepoEngine::Cubecl);
        state.cycle_next();
        assert_eq!(state.repo_engine, RepoEngine::Direct);
        state.cycle_next();
        assert_eq!(state.repo_engine, RepoEngine::Batch);
        state.cycle_prev();
        assert_eq!(state.repo_engine, RepoEngine::Direct);
        state.cycle_prev();
        assert_eq!(state.repo_engine, RepoEngine::Cubecl);
        state.cycle_prev();
        assert_eq!(state.repo_engine, RepoEngine::Hyper);

        // Booleans: Left = false, Right = true
        state.focus = FocusField::Greeking;
        state.cycle_prev();
        assert!(!state.greeking);
        state.cycle_next();
        assert!(state.greeking);

        // FieldMode: Left = Instanced, Right = Derived
        state.focus = FocusField::FieldMode;
        state.cycle_prev();
        assert_eq!(state.field_mode, FieldMode::Instanced);
        state.cycle_next();
        assert_eq!(state.field_mode, FieldMode::Derived);
        state.toggle_current();
        assert_eq!(state.field_mode, FieldMode::Instanced);
        state.toggle_current();
        assert_eq!(state.field_mode, FieldMode::Derived);
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
            RepoEngine::Cubecl.description(),
            "Pure GPU parallel compute pipeline (Metal/WGPU) with in-flight UTF-8 decode, parallel Blelloch scan & direct slot emission."
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
    }

    #[test]
    fn test_format_subtitle_description_narrow_and_wide() {
        let desc = "Short test string";
        // Less than 18 columns returns None
        assert!(format_subtitle_description(desc, 17).is_none());

        // Wide area returns full string
        let wide = format_subtitle_description(desc, 60).unwrap();
        assert_eq!(wide.spans[0].content, " ℹ ");
        assert_eq!(wide.spans[1].content, "Short test string ");

        // Narrow area truncates with ellipsis
        // Area width 22: available_width = 14. 13 chars + '…' + ' '
        let narrow = format_subtitle_description(desc, 22).unwrap();
        assert_eq!(narrow.spans[0].content, " ℹ ");
        assert_eq!(narrow.spans[1].content, "Short test st… ");
    }

    #[test]
    fn test_tui_subtitles_rendered_on_select() {
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // 1. RepoEngine focused: layout section should display the engine description
        let mut state = LauncherState::defaults();
        state.focus = FocusField::RepoEngine;
        state.repo_engine = RepoEngine::Hyper;

        terminal
            .draw(|f| {
                draw_layout_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("Parallel CPU Rayon layout"));

        // Change engine to Cubecl: subtitle updates immediately
        state.repo_engine = RepoEngine::Cubecl;
        terminal
            .draw(|f| {
                draw_layout_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("Pure GPU parallel compute"));

        // Unfocus RepoEngine: layout section should not show engine subtitle
        state.focus = FocusField::LayoutMode;
        terminal
            .draw(|f| {
                draw_layout_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(!content.contains("Parallel CPU Rayon layout"));
        assert!(!content.contains("Pure GPU parallel compute"));

        // 2. FieldMode focused: graphics section should display field mode description
        state.focus = FocusField::FieldMode;
        state.field_mode = FieldMode::Derived;
        terminal
            .draw(|f| {
                draw_graphics_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("20 B DerivedSlot"));

        // Change field mode to Instanced: subtitle updates immediately
        state.field_mode = FieldMode::Instanced;
        terminal
            .draw(|f| {
                draw_graphics_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("32 B RenderSlot"));

        // Unfocus FieldMode: graphics section should not show field mode subtitle
        state.focus = FocusField::Greeking;
        terminal
            .draw(|f| {
                draw_graphics_section(f, Rect::new(0, 0, 60, 10), &state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(!content.contains("32 B RenderSlot"));
        assert!(!content.contains("20 B DerivedSlot"));
    }
}
