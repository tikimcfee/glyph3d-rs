//! The relayout rebuild: a request to change a scene's layout params (field
//! mode, layout strategy, repo layout mode, z dial, cluster mode, the agent
//! session and its deck options) applied to the `SceneChoice` the scene was
//! built from, and the scene built again from it. Moved out of the windowed
//! app on 2026-10-26 so any host (the egui app today, the makepad widget
//! next) rebuilds the same way: the host owns the choice and the options,
//! calls [`rebuild_for_relayout`] and swaps in what it returns.

use std::time::Instant;

use crate::glyph_scene::{CameraMode, UiProbe};
use crate::gpu::GpuContext;
use crate::scene::SceneLike;
use crate::{SceneChoice, SceneCullOptions};

/// What a host asks the rebuild to change. Any field may be set; the rebuild
/// applies every one and rebuilds only if one actually moved.
#[derive(Clone, Default)]
pub struct RelayoutRequest {
    pub z_wrap_spacing: Option<f64>,
    pub toggle_cluster: bool,
    pub toggle_layout: bool,
    /// A session the F7 browser read (what `agent transcript` was given,
    /// the `agent` that read it, and what it read).
    pub switch_agent_session: Option<(String, crate::agent_transcript::AgentCli, crate::agent_transcript::LoadedSession)>,
    pub carrel_options: Option<crate::spatial_scene::CarrelLayoutOptions>,
    pub set_strategy: Option<crate::repo::Strategy>,
    pub set_field_mode: Option<glyph_field::GlyphFieldMode>,
}

/// Mutate the scene choice's layout params, then build the scene again —
/// through the same builder startup used, so it is correct by construction
/// (the one true layout path rebuilds everything the params touch: cull
/// AABBs, per-file pick params, bounds) at the cost of a full reload per
/// apply, which is why the panel fires on release/click, not per tick. The
/// camera pose survives via `live`, the old scene's probe and its last frame
/// (as do its live background/cull settings); pick/selection/grab state
/// resets with the scene (a fresh load has none — same as the JS
/// `grid.layout` refold). Repo scenes carry the z dial and the cluster
/// toggle; text scenes carry both too; anything else has no layout params
/// and a stale request is a no-op. `probed` builds the new scene with a
/// probe (the host shows a panel). Returns the new scene, sized to `width` x
/// `height`, and its probe for the host to swap in; None when nothing moved
/// and nothing was rebuilt.
#[expect(clippy::too_many_arguments, reason = "the host's side of a rebuild: its context and surface, the choice and options it owns, and the old scene's probe, passed apart")]
pub fn rebuild_for_relayout(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    (width, height): (u32, u32),
    choice: &mut SceneChoice,
    cull_opts: &mut SceneCullOptions,
    live: Option<&UiProbe>,
    probed: bool,
    req: RelayoutRequest,
) -> Option<(Box<dyn SceneLike>, Option<UiProbe>)> {
    let mut changed = false;
    let mut note = String::new();
    let reset_pose = req.switch_agent_session.is_some();
    if let Some((session, agent, loaded)) = req.switch_agent_session {
        let emoji_sheet = match choice {
            SceneChoice::AgentSession { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::Text { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::Repo { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::Demo => crate::default_emoji_sheet(),
        };
        note += &format!("switch_agent_session -> {session} ");
        *choice = SceneChoice::AgentSession {
            session,
            agent,
            emoji_sheet,
            layout_options: crate::spatial_scene::CarrelLayoutOptions::default(),
            cached_session: std::sync::Arc::new(std::sync::RwLock::new(Some(loaded))),
        };
        changed = true;
    }
    if let SceneChoice::AgentSession { layout_options, .. } = choice {
        if let Some(new_opts) = req.carrel_options {
            if *layout_options != new_opts {
                *layout_options = new_opts;
                changed = true;
                note += &format!("carrel_options -> limit: {}, scroll: {} ", new_opts.deck_window_limit, new_opts.deck_scroll_offset);
            }
        }
    }
    // The z dial exists on repo scenes, and on text scenes since they became
    // one-file repo loads (2026-10-11).
    if let SceneChoice::Repo { z_wrap_spacing, .. } | SceneChoice::Text { z_wrap_spacing, .. } = choice {
        if let Some(new_z) = req.z_wrap_spacing {
            if z_wrap_spacing.to_bits() != new_z.to_bits() {
                *z_wrap_spacing = new_z;
                changed = true;
                note += &format!("z_wrap_spacing -> {new_z} ");
            }
        }
    }
    // Layout engine strategy (repo scenes only).
    if let Some(new_strategy) = req.set_strategy {
        if let SceneChoice::Repo { strategy, .. } = choice {
            if *strategy != new_strategy {
                *strategy = new_strategy;
                changed = true;
                note += &format!("strategy -> {:?} ", new_strategy);
            }
        }
    }
    // Glyph field render mode (all scenes).
    if let Some(new_mode) = req.set_field_mode {
        if cull_opts.field_mode != new_mode {
            cull_opts.field_mode = new_mode;
            changed = true;
            note += &format!("field_mode -> {:?} ", new_mode);
        }
    }
    // The cluster toggle rides any scene whose choice carries the mode —
    // repo and text alike (SceneChoice::Text has carried it since the pass
    // landed; the panel seed for text scenes is GlyphScene's
    // probe_cluster_mode).
    if req.toggle_cluster {
        let mode = match choice {
            SceneChoice::Repo { cluster_mode, .. } | SceneChoice::Text { cluster_mode, .. } => {
                Some(cluster_mode)
            }
            _ => None,
        };
        if let Some(mode) = mode {
            *mode = match *mode {
                crate::fold::ClusterMode::Leader => crate::fold::ClusterMode::Cluster,
                crate::fold::ClusterMode::Cluster => crate::fold::ClusterMode::Leader,
            };
            changed = true;
            note += &format!("cluster_mode -> {mode:?} ");
        }
    }
    if req.toggle_layout {
        if let SceneChoice::Repo { layout_mode, .. } = choice {
            *layout_mode = match *layout_mode {
                crate::repo::RepoLayoutMode::Shelf => crate::repo::RepoLayoutMode::Carrel,
                crate::repo::RepoLayoutMode::Carrel => crate::repo::RepoLayoutMode::Library,
                crate::repo::RepoLayoutMode::Library => crate::repo::RepoLayoutMode::Shelf,
            };
            changed = true;
            note += &format!("layout_mode -> {:?} ", *layout_mode);
        }
    }
    if !changed {
        return None; // a release without a move (a click, a typed repeat) rebuilds nothing
    }
    // Snapshot the camera and live cull settings BEFORE the swap: the probe holds
    // last frame's actual eye/yaw/pitch and live background/cull settings, and a rebuild
    // that teleports the viewer or resets the background would make the dial unusable.
    let (pose, live_cull_opts) = if let Some(p) = live {
        let p = p.borrow();
        (
            Some((p.eye, p.yaw, p.pitch)),
            SceneCullOptions {
                cull: cull_opts.cull,
                file_backgrounds: p.file_backgrounds,
                file_bg_color: p.file_bg_color,
                lod_min_px: Some(p.lod_min_px),
                greeking: p.greeking,
                greek_pure: p.greek_pure,
                greek_onset_px: Some(p.greek_onset_px),
                field_mode: cull_opts.field_mode,
                environment: p.environment,
                ground_y: cull_opts.ground_y,
            },
        )
    } else {
        (None, *cull_opts)
    };
    let t = Instant::now();
    let (mut scene, probe) = if probed {
        crate::build_scene_probed(ctx, color_format, choice, CameraMode::Fly, live_cull_opts)
    } else {
        (crate::build_scene_with_options(ctx, color_format, choice, CameraMode::Fly, live_cull_opts), None)
    };
    // The mode the scene BUILT may not be the one asked for (a load past the
    // Derived lanes falls back to Instanced, loudly — `layout_hyper::
    // derived_lane_limits`); the options follow the build, so the panel's
    // selector and the next request start from what is actually drawn.
    if let Some(built) = probe.as_ref().and_then(|p| p.borrow().field_mode) {
        if built != cull_opts.field_mode {
            note += &format!("(built {built}, not {}) ", cull_opts.field_mode);
            cull_opts.field_mode = built;
        }
    }
    scene.set_viewport(width, height);
    if !reset_pose {
        if let Some((eye, yaw, pitch)) = pose {
            scene.set_cam_pose(eye, yaw, pitch);
        }
    }
    println!(
        "relayout: {note}(scene rebuilt in {:?}; pick/selection/grab state reset)",
        t.elapsed()
    );
    Some((scene, probe))
}
