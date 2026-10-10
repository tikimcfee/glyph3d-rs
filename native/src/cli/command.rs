//! Strongly-typed execution commands dispatched from CLI options.

use std::path::PathBuf;
use crate::repo::{self, RepoParams, Strategy};
use crate::{SceneChoice, SceneCullOptions};
use super::args::{default_text_file, Cli, PresentMode};
use super::ops::Op;
use crate::default_emoji_sheet;

/// Top-level action requested via the CLI.
#[derive(Debug)]
pub enum CliCommand {
    /// Generate shell completions and exit.
    GenerateCompletion {
        shell: clap_complete::Shell,
    },
    /// Inspect and print GPU device profile / key, then exit.
    GpuInfo(GpuInfoMode),
    /// Reference port parity check over .pipe.bin / .bake.bin fixtures.
    Fixture(FixtureTask),
    /// Headless repo scan, layout, and statistics measurement without a GPU.
    RepoScanOnly {
        dir: PathBuf,
        params: RepoParams,
        strategy: Strategy,
        verify: bool,
    },
    /// Render execution plan (windowed or offscreen).
    Render(RenderPlan),
}

/// Mode for GPU information inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuInfoMode {
    Key,
    Profile,
}

/// Fixture parity tasks.
#[derive(Debug, Clone)]
pub enum FixtureTask {
    Reference(Vec<PathBuf>),
    Trie(Vec<PathBuf>),
    Fold(Vec<PathBuf>),
    Scan(Vec<PathBuf>),
    Bake(Vec<PathBuf>),
    /// HyperLayout vs the oracle-backed fold; the mode applies to directory
    /// and text inputs (fixtures carry their own).
    HyperOracle(Vec<PathBuf>, crate::fold::ClusterMode),
    /// The visible-set line table over the inputs, printed (M1 of the
    /// GPU-direction plan).
    LineTableStats(Vec<PathBuf>, crate::fold::ClusterMode),
}

/// Execution plan for rendering a scene.
#[derive(Debug)]
pub struct RenderPlan {
    pub choice: SceneChoice,
    pub cull_opts: SceneCullOptions,
    pub ops: Vec<Op>,
    pub target: RenderTarget,
}

/// Render output target: offscreen screenshot vs interactive window.
#[derive(Debug)]
pub enum RenderTarget {
    Offscreen {
        path: PathBuf,
        frames: u32,
        zoom: f32,
    },
    Windowed {
        no_ui: bool,
        shot: Option<(u64, PathBuf)>,
        present_mode: PresentMode,
        frames: Option<u32>,
    },
}

impl Cli {
    /// Resolve the parsed CLI flags into an explicit, strongly typed command.
    pub fn action(&self) -> CliCommand {
        if let Some(shell) = self.generate {
            return CliCommand::GenerateCompletion { shell };
        }

        if self.gpu_key {
            return CliCommand::GpuInfo(GpuInfoMode::Key);
        }
        if self.gpu_profile {
            return CliCommand::GpuInfo(GpuInfoMode::Profile);
        }

        // Fixture parity tasks
        if !self.fixture_reference.is_empty() {
            return CliCommand::Fixture(FixtureTask::Reference(self.fixture_reference.clone()));
        }
        if !self.fixture_trie.is_empty() {
            return CliCommand::Fixture(FixtureTask::Trie(self.fixture_trie.clone()));
        }
        if !self.fixture_fold.is_empty() {
            return CliCommand::Fixture(FixtureTask::Fold(self.fixture_fold.clone()));
        }
        if !self.fixture_scan.is_empty() {
            return CliCommand::Fixture(FixtureTask::Scan(self.fixture_scan.clone()));
        }
        if !self.fixture_bake.is_empty() {
            return CliCommand::Fixture(FixtureTask::Bake(self.fixture_bake.clone()));
        }
        if !self.hyper_oracle_check.is_empty() {
            return CliCommand::Fixture(FixtureTask::HyperOracle(
                self.hyper_oracle_check.clone(),
                self.cluster_mode,
            ));
        }
        if !self.line_table_stats.is_empty() {
            return CliCommand::Fixture(FixtureTask::LineTableStats(
                self.line_table_stats.clone(),
                self.cluster_mode,
            ));
        }

        if self.repo_scan_only {
            let dir = self
                .load_repo
                .clone()
                .expect("--repo-scan-only needs --load-repo");
            let params = repo::RepoParams {
                wrap_mode: self.wrap_mode,
                z_wrap_spacing: self.z_wrap_spacing,
                cluster_mode: self.cluster_mode,
                layout_mode: self.layout_mode,
                color_mode: self.color_mode,
                field_mode: self.field_mode,
                ..Default::default()
            };
            return CliCommand::RepoScanOnly {
                dir,
                params,
                strategy: self.repo_engine,
                verify: self.repo_verify,
            };
        }

        let emoji_sheet = self.emoji_sheet.clone().unwrap_or_else(default_emoji_sheet);
        let choice = if self.demo {
            SceneChoice::Demo
        } else if let Some(session_path) = &self.agent_session {
            SceneChoice::AgentSession {
                session_path: session_path.clone(),
                emoji_sheet,
                layout_options: crate::spatial_scene::CarrelLayoutOptions {
                    deck_window_limit: self.deck_window_limit,
                    deck_scroll_offset: self.deck_scroll_offset,
                    desk_revision_limit: self.desk_revision_limit,
                    desk_scroll_offset: 0,
                    max_file_stacks: 20,
                    active_beat: None,
                },
                cached_session: std::sync::Arc::new(std::sync::RwLock::new(None)),
            }
        } else if let Some(dir) = &self.load_repo {
            SceneChoice::Repo {
                dir: dir.clone(),
                strategy: self.repo_engine,
                verify: self.repo_verify,
                focus: self.focus_file.clone(),
                wrap_mode: self.wrap_mode,
                z_wrap_spacing: self.z_wrap_spacing,
                cluster_mode: self.cluster_mode,
                layout_mode: self.layout_mode,
                color_mode: self.color_mode,
                emoji_sheet,
            }
        } else if let Some(file) = &self.engine_render {
            SceneChoice::EngineText {
                file: file.clone(),
                emoji_sheet,
            }
        } else {
            SceneChoice::Text {
                file: self.render_file.clone().unwrap_or_else(default_text_file),
                copies: self.copies,
                emoji_sheet,
                cluster_mode: self.cluster_mode,
            }
        };

        let greek_pure = !self.greek_smooth;
        let cull_opts = SceneCullOptions {
            cull: !self.no_cull,
            file_backgrounds: self.file_backgrounds,
            file_bg_color: self
                .file_bg_color
                .unwrap_or(crate::config::settings().glyph_scene.file_bg_color),
            lod_min_px: self.show_glyphs_px,
            greeking: !self.no_greeking,
            greek_pure,
            greek_onset_px: self.text_detail_px,
            field_mode: self.field_mode,
            debug_tint: self.debug_tint.as_u32(),
            environment: self
                .environment
                .unwrap_or(crate::config::settings().environment.mode),
            ground_y: self.ground_y,
        };

        let target = match &self.screenshot {
            Some(path) => RenderTarget::Offscreen {
                path: path.clone(),
                frames: self.frames.unwrap_or(1),
                zoom: self.zoom,
            },
            None => {
                let shot = self.screenshot_frame.zip(self.screenshot_out.clone());
                RenderTarget::Windowed {
                    no_ui: self.no_ui,
                    shot,
                    present_mode: self.present_mode,
                    frames: self.frames,
                }
            }
        };

        CliCommand::Render(RenderPlan {
            choice,
            cull_opts,
            ops: self.ops.clone(),
            target,
        })
    }
}
