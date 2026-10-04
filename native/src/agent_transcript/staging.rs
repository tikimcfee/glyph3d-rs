//! 3D Visual Staging for Agent Sessions and Workdesks.
//!
//! Converts an [`AgentSession`] and its accompanying [`RevisionEngine`]
//! into a [`StagedText`] package ready for the Slug renderer.

use crate::agent_transcript::AgentSession;
use crate::atlas::Atlas;
use crate::glyph_scene::{GlyphInstance, GroupRow};
use crate::layout::GlyphArena;
use crate::layout_stack::LayoutController;
use crate::repo::{RepoLayoutMode, RepoParams};
use crate::revision::RevisionEngine;
use crate::text::{cover_segment, pack_rgba8, StagedText, CELL_HEIGHT_WORLD, LINE_HEIGHT_FACTOR};

#[allow(clippy::too_many_arguments)]
fn layout_string(
    atlas: &Atlas,
    text: &str,
    start_pos: [f32; 3],
    color: [u8; 3],
    max_cols: usize,
    max_lines: usize,
    group_id: u32,
    instances: &mut Vec<GlyphInstance>,
    codepoints_decoded: &mut usize,
) {
    let fu_per_world = atlas.metrics.em_height_fu as f32 / CELL_HEIGHT_WORLD;
    let cell_w = atlas.metrics.advance_fu as f32 / fu_per_world;
    let line_h = CELL_HEIGHT_WORLD * LINE_HEIGHT_FACTOR;

    let mut row = 0;
    for line in text.lines().take(max_lines) {
        let mut col = 0;
        for ch in line.chars().take(max_cols) {
            *codepoints_decoded += 1;
            if ch == '\t' {
                col = (col + 4) & !3;
                continue;
            }
            let entry = atlas.lookup(ch as u32);
            if (entry.flags & crate::atlas::FLAG_MISSING) == 0 {
                instances.push(GlyphInstance {
                    pos: [
                        start_pos[0] + col as f32 * cell_w,
                        start_pos[1] - row as f32 * line_h,
                        start_pos[2],
                    ],
                    glyph_id: entry.glyph_id,
                    row: row as u32,
                    col: col as u32,
                    color: pack_rgba8(color, 255),
                    group_id,
                    advance: cell_w,
                    height: CELL_HEIGHT_WORLD,
                    flags: 0,
                    _pad: 0,
                });
            }
            col += 1;
        }
        row += 1;
    }
}

/// Stage an Agent Session and its Workdesk into a visual 3D scene.
pub fn stage_agent_session(
    atlas: Option<&Atlas>,
    slot_ink: &[Option<[f32; 4]>],
    session: AgentSession,
    revision_engine: RevisionEngine,
) -> StagedText {
    let params = RepoParams {
        layout_mode: RepoLayoutMode::Carrel,
        ..Default::default()
    };
    let mut controller = LayoutController::new(params);

    controller.spawn_agent_carrel_session(session.clone(), revision_engine.clone());

    // Dynamic bounding volume enclosing the carrel (Deck + Workdesk):
    let (bounds_min, bounds_max) = if let Some(carrel_e) = controller.active_carrel {
        if let Some((min, max)) = controller.scene.world_bounds(carrel_e) {
            let pad_x = ((max[0] - min[0]) * 0.08).max(25.0);
            let pad_y = ((max[1] - min[1]) * 0.08).max(25.0);
            let pad_z = ((max[2] - min[2]) * 0.1).max(20.0);
            (
                [min[0] - pad_x, min[1] - pad_y, min[2] - pad_z],
                [max[0] + pad_x, max[1] + pad_y, max[2] + pad_z],
            )
        } else {
            ([-200.0, -160.0, -100.0], [350.0, 40.0, 100.0])
        }
    } else {
        ([-200.0, -160.0, -100.0], [350.0, 40.0, 100.0])
    };

    let center = [
        (bounds_min[0] + bounds_max[0]) * 0.5,
        (bounds_min[1] + bounds_max[1]) * 0.5,
    ];
    let half_extent = [
        ((bounds_max[0] - bounds_min[0]) * 0.5).max(10.0),
        ((bounds_max[1] - bounds_min[1]) * 0.5).max(10.0),
    ];
    let focus_bounds = Some((center, half_extent));

    let mut raw_instances = Vec::new();
    let mut codepoints_decoded = 0;

    if let Some(atlas) = atlas {
        // Lay out text for active turn card and workdesk file stacks
        if let Some(turn) = session.turns.first() {
            // Turn title on Left Page (Mind Header)
            let title = format!("Turn {}: {}", turn.turn_index + 1, turn.summary());
            layout_string(
                atlas,
                &title,
                [-128.0, -3.8, 0.2],
                [240, 245, 255],
                65,
                1,
                0,
                &mut raw_instances,
                &mut codepoints_decoded,
            );

            // Mind Body: Prompt preview
            if let Some(ref prompt) = turn.prompt {
                layout_string(
                    atlas,
                    prompt,
                    [-128.0, -7.5, 0.2],
                    [200, 205, 215],
                    55,
                    20,
                    0,
                    &mut raw_instances,
                    &mut codepoints_decoded,
                );
            }

            // Right Page (Impact Header)
            let touched_summary = if turn.file_actions.is_empty() {
                "Material Impact: No files touched".to_string()
            } else {
                format!("Material Impact: {} file(s) touched", turn.file_actions.len())
            };
            layout_string(
                atlas,
                &touched_summary,
                [-70.0, -3.8, 0.2],
                [180, 235, 190],
                65,
                1,
                0,
                &mut raw_instances,
                &mut codepoints_decoded,
            );

            // Right Page Body: Touched files & tool calls
            let mut impact_lines = Vec::new();
            for tf in &turn.file_actions {
                let action_str = match tf.action {
                    crate::spatial_scene::workdesk::FileActionKind::Write => "Write",
                    crate::spatial_scene::workdesk::FileActionKind::Edit => "Edit",
                    crate::spatial_scene::workdesk::FileActionKind::Read => "Read",
                    crate::spatial_scene::workdesk::FileActionKind::AstAnalysis => "Ast",
                };
                let summary_suffix = if !tf.summary.is_empty() {
                    format!(" — {}", tf.summary)
                } else {
                    String::new()
                };
                impact_lines.push(format!("• {} {}{}", action_str, tf.file_path, summary_suffix));
            }
            if impact_lines.is_empty() && !turn.tool_calls.is_empty() {
                for tc in turn.tool_calls.iter().take(8) {
                    impact_lines.push(format!("• Tool: {}", tc.name));
                }
            }
            let impact_text = impact_lines.join("\n");
            layout_string(
                atlas,
                &impact_text,
                [-70.0, -7.5, 0.2],
                [200, 210, 220],
                55,
                20,
                0,
                &mut raw_instances,
                &mut codepoints_decoded,
            );
        }

        // Workdesk: Label each file stack with its file name and revision indicator
        let splay_cols = 4;
        let file_spacing = [60.0, 45.0];
        let file_paths = revision_engine.file_paths();
        for (i, path) in file_paths.iter().enumerate() {
            let col = i % splay_cols;
            let row = i / splay_cols;
            let stack_x = 75.0 + col as f32 * file_spacing[0];
            let stack_y = -(row as f32 * file_spacing[1]);

            // File label above stack
            let rev_count = revision_engine.history(path).map(|h| h.revisions.len()).unwrap_or(1);
            let label = format!("{path} (R{})", rev_count);
            layout_string(
                atlas,
                &label,
                [stack_x - 26.0, stack_y + 2.5, 0.2],
                [250, 245, 230],
                45,
                1,
                0,
                &mut raw_instances,
                &mut codepoints_decoded,
            );
        }
    }

    let glyphs_emitted = raw_instances.len();
    let segments = vec![cover_segment(
        &raw_instances,
        bounds_min,
        bounds_max,
        slot_ink,
    )];
    let instances = GlyphArena::from_vec(raw_instances);

    // Ensure at least one identity group exists for GPU group buffer allocation
    let groups = vec![GroupRow::identity([0.0, 0.0, 0.0])];

    StagedText {
        instances,
        groups,
        bounds_min,
        bounds_max,
        codepoints_decoded,
        glyphs_emitted,
        missing_or_bitmap: 0,
        focus_bounds,
        segments,
        pick: None,
        controller: Some(controller),
    }
}
