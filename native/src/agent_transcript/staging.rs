use bevy_ecs::prelude::{ChildOf, Entity};
use bevy_transform::prelude::Transform;
use glam::{Quat, Vec3};

use crate::agent_transcript::{AgentSession, AgentTurn};
use crate::atlas::Atlas;
use crate::glyph_scene::{GlyphInstance, GroupRow};
use crate::layout::GlyphArena;
use crate::layout_stack::LayoutController;
use crate::repo::{RepoLayoutMode, RepoParams};
use crate::revision::{FileRevision, RevisionEngine};
use crate::spatial_scene::GlyphGroupBinding;
use crate::text::{cover_segment, pack_rgba8, StagedText, CELL_HEIGHT_WORLD, LINE_HEIGHT_FACTOR};

/// Wrap prose into lines with a maximum column width, breaking at spaces when possible.
fn wrap_prose(text: &str, max_cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in trimmed.split_whitespace() {
            if cur.is_empty() {
                if word.len() > max_cols {
                    out.push(word[..max_cols].to_string());
                    cur = word[max_cols..].to_string();
                } else {
                    cur.push_str(word);
                }
            } else if cur.len() + 1 + word.len() <= max_cols {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(cur);
                if word.len() > max_cols {
                    out.push(word[..max_cols].to_string());
                    cur = word[max_cols..].to_string();
                } else {
                    cur = word.to_string();
                }
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// Lay out colored text lines into glyph instances.
#[allow(clippy::too_many_arguments)]
fn layout_colored_lines(
    atlas: &Atlas,
    lines: &[(String, [u8; 3])],
    start_pos: [f32; 3],
    max_cols: usize,
    max_lines: usize,
    group_id: u32,
    instances: &mut Vec<GlyphInstance>,
    codepoints_decoded: &mut usize,
) {
    let fu_per_world = atlas.metrics.em_height_fu as f32 / CELL_HEIGHT_WORLD;
    let cell_w = atlas.metrics.advance_fu as f32 / fu_per_world;
    let line_h = CELL_HEIGHT_WORLD * LINE_HEIGHT_FACTOR;

    for (row, (line, color)) in lines.iter().take(max_lines).enumerate() {
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
                    color: pack_rgba8(*color, 255),
                    group_id,
                    advance: cell_w,
                    height: CELL_HEIGHT_WORLD,
                    flags: 0,
                    _pad: 0,
                });
            }
            col += 1;
        }
    }
}

/// Format the fixed header lines for the Left Page (Mind) of an Agent Turn Card.
fn format_turn_left_header(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    vec![
        (
            format!("TURN {}: {}", turn.turn_index + 1, turn.summary()),
            [255, 255, 255],
        ),
        (
            format!(
                "Mind • {} tool call(s) • {} think block(s)",
                turn.tool_calls.len(),
                turn.thinking.len()
            ),
            [150, 175, 205],
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            [70, 90, 115],
        ),
    ]
}

/// Format the body lines for the Left Page (Mind) of an Agent Turn Card.
fn format_turn_left_body(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    // 1. User Prompt
    lines.push(("[ USER PROMPT ]".to_string(), [130, 235, 215]));
    if let Some(ref prompt) = turn.prompt {
        for l in wrap_prose(prompt, 72) {
            lines.push((format!("  {l}"), [215, 225, 235]));
        }
    } else {
        lines.push(("  (No user prompt in this turn)".to_string(), [130, 145, 160]));
    }

    lines.push((
        "─────────────────────────────────────────────────────────────".to_string(),
        [70, 90, 115],
    ));

    // 2. Reasoning / Chain of Thought
    lines.push((
        "[ CHAIN OF THOUGHT / REASONING ]".to_string(),
        [250, 210, 100],
    ));
    let thinking_text = turn.combined_thinking();
    if !thinking_text.is_empty() {
        for l in wrap_prose(&thinking_text, 72) {
            lines.push((format!("  {l}"), [240, 230, 210]));
        }
    } else {
        lines.push((
            "  (No internal chain-of-thought logged in transcript)".to_string(),
            [130, 145, 160],
        ));
    }

    lines
}

/// Format the fixed header lines for the Right Page (Material Impact) of an Agent Turn Card.
fn format_turn_right_header(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    vec![
        (
            format!("TURN {} MATERIAL IMPACT", turn.turn_index + 1),
            [120, 245, 160],
        ),
        (
            format!(
                "Impact • {} file action(s) • {} tool invocation(s)",
                turn.file_actions.len(),
                turn.tool_calls.len()
            ),
            [140, 185, 160],
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            [70, 90, 115],
        ),
    ]
}

/// Format the body lines for the Right Page (Material Impact) of an Agent Turn Card.
fn format_turn_right_body(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    // 1. Touched Files
    lines.push((
        format!("[ TOUCHED FILES ({}) ]", turn.file_actions.len()),
        [140, 240, 175],
    ));
    if turn.file_actions.is_empty() {
        lines.push((
            "  • No file modifications in this turn".to_string(),
            [130, 145, 160],
        ));
    } else {
        for fa in &turn.file_actions {
            let (tag, color) = match fa.action {
                crate::spatial_scene::workdesk::FileActionKind::Edit => ("Edit", [250, 200, 90]),
                crate::spatial_scene::workdesk::FileActionKind::Write => ("Write", [120, 240, 150]),
                crate::spatial_scene::workdesk::FileActionKind::Read => ("Read", [130, 210, 250]),
                crate::spatial_scene::workdesk::FileActionKind::AstAnalysis => ("Ast", [215, 155, 250]),
            };
            let diff_note = if !fa.hunks.is_empty() {
                let added: usize = fa
                    .hunks
                    .iter()
                    .map(|h| h.lines.iter().filter(|l| l.starts_with('+')).count())
                    .sum();
                let removed: usize = fa
                    .hunks
                    .iter()
                    .map(|h| h.lines.iter().filter(|l| l.starts_with('-')).count())
                    .sum();
                format!(" (+{added} -{removed})")
            } else {
                String::new()
            };
            let summary_suffix = if !fa.summary.is_empty() {
                format!(" — {}", fa.summary)
            } else {
                String::new()
            };
            lines.push((
                format!("  • {tag} {}{diff_note}{summary_suffix}", fa.file_path),
                color,
            ));
        }
    }

    lines.push((
        "─────────────────────────────────────────────────────────────".to_string(),
        [70, 90, 115],
    ));

    // 2. Assistant Response
    lines.push(("[ ASSISTANT RESPONSE ]".to_string(), [250, 252, 255]));
    let assistant_text = turn.combined_assistant_text();
    if !assistant_text.is_empty() {
        for l in wrap_prose(&assistant_text, 72) {
            lines.push((format!("  {l}"), [220, 235, 245]));
        }
    } else {
        lines.push((
            "  (No assistant text message recorded in this turn)".to_string(),
            [130, 145, 160],
        ));
    }

    // 3. Tool Invocations
    if !turn.tool_calls.is_empty() {
        lines.push((
            "─────────────────────────────────────────────────────────────".to_string(),
            [70, 90, 115],
        ));
        lines.push((
            format!("[ TOOL INVOCATIONS ({}) ]", turn.tool_calls.len()),
            [130, 205, 245],
        ));
        for tc in &turn.tool_calls {
            let status = if tc.is_error { "error" } else { "ok" };
            lines.push((format!("  • {} [{status}]", tc.name), [185, 215, 235]));
        }
    }

    lines
}

/// Format the fixed header lines for a Workdesk File Revision Card ($R_k$).
fn format_revision_card_header(rev: &FileRevision, file_path: &str) -> Vec<(String, [u8; 3])> {
    let (action_str, color) = match rev.action {
        crate::spatial_scene::workdesk::FileActionKind::Edit => ("Edit", [250, 195, 80]),
        crate::spatial_scene::workdesk::FileActionKind::Write => ("Write", [110, 240, 150]),
        crate::spatial_scene::workdesk::FileActionKind::Read => ("Read", [120, 210, 250]),
        crate::spatial_scene::workdesk::FileActionKind::AstAnalysis => ("Ast", [210, 155, 250]),
    };
    vec![
        (
            format!("R{} • {action_str} • {file_path}", rev.revision_index),
            color,
        ),
        (
            format!(
                "Turn {} │ {} lines │ +{} -{}",
                rev.turn_index + 1,
                rev.line_count(),
                rev.diff_stats.added,
                rev.diff_stats.removed
            ),
            [150, 170, 190],
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            [70, 85, 105],
        ),
    ]
}

/// Format the body lines for a Workdesk File Revision Card ($R_k$).
///
/// Contains the whole file text (or all diff hunks) with line numbers and syntax styling.
/// Scaling is applied to the child glyph field to fit wholly within the page.
fn format_revision_card_body(rev: &FileRevision) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    if !rev.hunks.is_empty() {
        for hunk in &rev.hunks {
            lines.push((
                format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                ),
                [100, 195, 255],
            ));
            for hl in &hunk.lines {
                if hl.starts_with('+') {
                    lines.push((hl.clone(), [120, 245, 140]));
                } else if hl.starts_with('-') {
                    lines.push((hl.clone(), [245, 120, 120]));
                } else {
                    lines.push((hl.clone(), [200, 210, 220]));
                }
            }
        }

        // Full context snapshot after edit if text is present
        if !rev.text.is_empty() {
            lines.push((
                "── [ Snapshot after edit ] ──────────────────────────────".to_string(),
                [80, 95, 115],
            ));
            for (line_idx, l) in rev.text.lines().take(5000).enumerate() {
                let trimmed = l.trim_start();
                let color = if trimmed.starts_with("//")
                    || trimmed.starts_with('#')
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*')
                {
                    [130, 155, 170]
                } else {
                    [215, 225, 235]
                };
                lines.push((format!("{:4} │ {}", line_idx + 1, l), color));
            }
        }
    } else {
        // Base / whole-file snapshot mode
        if rev.text.is_empty() {
            lines.push((
                "  (Empty file or no snapshot captured)".to_string(),
                [130, 140, 155],
            ));
        } else {
            for (line_idx, l) in rev.text.lines().take(5000).enumerate() {
                let trimmed = l.trim_start();
                let color = if trimmed.starts_with("//")
                    || trimmed.starts_with('#')
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*')
                {
                    [130, 155, 170]
                } else {
                    [225, 230, 240]
                };
                lines.push((format!("{:4} │ {}", line_idx + 1, l), color));
            }
        }
    }

    lines
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

    let fu_per_world = atlas
        .map(|a| a.metrics.em_height_fu as f32 / CELL_HEIGHT_WORLD)
        .unwrap_or(1000.0);
    let cell_w = atlas
        .map(|a| a.metrics.advance_fu as f32 / fu_per_world)
        .unwrap_or(0.60);
    let line_h = CELL_HEIGHT_WORLD * LINE_HEIGHT_FACTOR;

    let mut next_gid = 1u32;

    // 1. Turn Deck Turn Cards: Left (Mind) and Right (Impact) Pages
    let mut turn_query = controller
        .scene
        .world
        .query::<(&crate::spatial_scene::AgentTurnCard, Entity)>();
    let turn_cards: Vec<(crate::spatial_scene::AgentTurnCard, Entity)> = turn_query
        .iter(&controller.scene.world)
        .map(|(c, e)| (c.clone(), e))
        .collect();

    for (card, _card_e) in turn_cards {
        if let Some(turn) = session.turns.get(card.turn_index) {
            // Left Page (Mind)
            let left_hdr = format_turn_left_header(turn);
            let left_body = format_turn_left_body(turn);

            let unscaled_h = (left_body.len() as f32 * line_h).max(1.0);
            let max_cols = left_body.iter().map(|(l, _)| l.len()).max().unwrap_or(1);
            let unscaled_w = (max_cols as f32 * cell_w).max(1.0);
            let s_y = 31.5 / unscaled_h;
            let s_x = 51.0 / unscaled_w;
            let s_left = s_y.min(s_x).clamp(0.005, 1.0);

            let left_hdr_gid = next_gid;
            next_gid += 1;
            controller.scene.world.spawn((
                Transform::from_translation(Vec3::new(-25.5, -2.0, 0.2)),
                ChildOf(card.left_page),
                GlyphGroupBinding {
                    group_id: left_hdr_gid,
                    tint: [1.0, 1.0, 1.0, 1.0],
                },
            ));

            let left_body_gid = next_gid;
            next_gid += 1;
            controller.scene.world.spawn((
                Transform {
                    translation: Vec3::new(-25.5, -7.0, 0.2),
                    scale: Vec3::new(s_left, s_left, 1.0),
                    rotation: Quat::IDENTITY,
                },
                ChildOf(card.left_page),
                GlyphGroupBinding {
                    group_id: left_body_gid,
                    tint: [1.0, 1.0, 1.0, 1.0],
                },
            ));

            if let Some(atlas) = atlas {
                layout_colored_lines(
                    atlas,
                    &left_hdr,
                    [0.0, 0.0, 0.0],
                    80,
                    4,
                    left_hdr_gid,
                    &mut raw_instances,
                    &mut codepoints_decoded,
                );
                layout_colored_lines(
                    atlas,
                    &left_body,
                    [0.0, 0.0, 0.0],
                    usize::MAX,
                    usize::MAX,
                    left_body_gid,
                    &mut raw_instances,
                    &mut codepoints_decoded,
                );
            }

            // Right Page (Impact)
            let right_hdr = format_turn_right_header(turn);
            let right_body = format_turn_right_body(turn);

            let unscaled_h = (right_body.len() as f32 * line_h).max(1.0);
            let max_cols = right_body.iter().map(|(l, _)| l.len()).max().unwrap_or(1);
            let unscaled_w = (max_cols as f32 * cell_w).max(1.0);
            let s_y = 31.5 / unscaled_h;
            let s_x = 51.0 / unscaled_w;
            let s_right = s_y.min(s_x).clamp(0.005, 1.0);

            let right_hdr_gid = next_gid;
            next_gid += 1;
            controller.scene.world.spawn((
                Transform::from_translation(Vec3::new(-25.5, -2.0, 0.2)),
                ChildOf(card.right_page),
                GlyphGroupBinding {
                    group_id: right_hdr_gid,
                    tint: [1.0, 1.0, 1.0, 1.0],
                },
            ));

            let right_body_gid = next_gid;
            next_gid += 1;
            controller.scene.world.spawn((
                Transform {
                    translation: Vec3::new(-25.5, -7.0, 0.2),
                    scale: Vec3::new(s_right, s_right, 1.0),
                    rotation: Quat::IDENTITY,
                },
                ChildOf(card.right_page),
                GlyphGroupBinding {
                    group_id: right_body_gid,
                    tint: [1.0, 1.0, 1.0, 1.0],
                },
            ));

            if let Some(atlas) = atlas {
                layout_colored_lines(
                    atlas,
                    &right_hdr,
                    [0.0, 0.0, 0.0],
                    80,
                    4,
                    right_hdr_gid,
                    &mut raw_instances,
                    &mut codepoints_decoded,
                );
                layout_colored_lines(
                    atlas,
                    &right_body,
                    [0.0, 0.0, 0.0],
                    usize::MAX,
                    usize::MAX,
                    right_body_gid,
                    &mut raw_instances,
                    &mut codepoints_decoded,
                );
            }
        }
    }

    // 2. Workdesk File Revision Cards ($R_k$)
    let mut rev_query = controller
        .scene
        .world
        .query::<(Entity, &crate::spatial_scene::FileRevisionCard)>();
    let rev_cards: Vec<(Entity, crate::spatial_scene::FileRevisionCard)> = rev_query
        .iter(&controller.scene.world)
        .map(|(e, c)| (e, c.clone()))
        .collect();

    for (card_e, card) in rev_cards {
        if let Some(history) = revision_engine.history(&card.file_path) {
            if let Some(rev) = history.get(card.revision_index) {
                let header_lines = format_revision_card_header(rev, &card.file_path);
                let body_lines = format_revision_card_body(rev);

                let unscaled_h = (body_lines.len() as f32 * line_h).max(1.0);
                let max_cols = body_lines.iter().map(|(l, _)| l.len()).max().unwrap_or(1);
                let unscaled_w = (max_cols as f32 * cell_w).max(1.0);

                let s_y = 30.0 / unscaled_h;
                let s_x = 51.0 / unscaled_w;
                let scale = s_y.min(s_x).clamp(0.005, 1.0);

                let header_gid = next_gid;
                next_gid += 1;
                controller.scene.world.spawn((
                    Transform::from_translation(Vec3::new(2.0, -1.8, 0.2)),
                    ChildOf(card_e),
                    GlyphGroupBinding {
                        group_id: header_gid,
                        tint: [1.0, 1.0, 1.0, 1.0],
                    },
                ));

                let body_gid = next_gid;
                next_gid += 1;
                controller.scene.world.spawn((
                    Transform {
                        translation: Vec3::new(2.0, -6.5, 0.2),
                        scale: Vec3::new(scale, scale, 1.0),
                        rotation: Quat::IDENTITY,
                    },
                    ChildOf(card_e),
                    GlyphGroupBinding {
                        group_id: body_gid,
                        tint: [1.0, 1.0, 1.0, 1.0],
                    },
                ));

                if let Some(atlas) = atlas {
                    layout_colored_lines(
                        atlas,
                        &header_lines,
                        [0.0, 0.0, 0.0],
                        80,
                        4,
                        header_gid,
                        &mut raw_instances,
                        &mut codepoints_decoded,
                    );
                    layout_colored_lines(
                        atlas,
                        &body_lines,
                        [0.0, 0.0, 0.0],
                        usize::MAX,
                        usize::MAX,
                        body_gid,
                        &mut raw_instances,
                        &mut codepoints_decoded,
                    );
                }
            }
        }
    }

    // 3. Workdesk Stack Title Labels (queried directly from ECS)
    let mut stack_query = controller.scene.world.query::<(
        &crate::spatial_scene::FileRevisionStack,
        &bevy_transform::prelude::GlobalTransform,
    )>();
    let stacks: Vec<(crate::spatial_scene::FileRevisionStack, glam::Vec3)> = stack_query
        .iter(&controller.scene.world)
        .map(|(s, g)| (s.clone(), g.translation()))
        .collect();

    if let Some(atlas) = atlas {
        for (stack, trans) in stacks {
            let rev_count = revision_engine
                .history(&stack.file_path)
                .map(|h| h.revisions.len())
                .unwrap_or(stack.revision_count);
            let label = format!("{} (R{})", stack.file_path, rev_count);
            layout_colored_lines(
                atlas,
                &[(label, [250, 245, 230])],
                [trans.x + 1.0, trans.y + 2.0, trans.z + 0.2],
                55,
                1,
                0,
                &mut raw_instances,
                &mut codepoints_decoded,
            );
        }
    }

    // Update global transforms for all child glyph fields
    controller.scene.update_transforms();

    let glyphs_emitted = raw_instances.len();
    let mut cover = cover_segment(
        &raw_instances,
        bounds_min,
        bounds_max,
        slot_ink,
    );
    // In agent carrel mode, glyph instances belong to dynamic ECS groups whose
    // world positions are transformed on the GPU via group rows. Sub-block culling
    // on untransformed local coordinates would falsely reject cards translated away
    // from the origin. Clearing blocks keeps segment-level culling over the full carrel.
    cover.blocks.clear();
    let segments = vec![cover];
    let instances = GlyphArena::from_vec(raw_instances);

    // Synchronize all ECS groups into the GPU group buffer
    let max_gid = controller
        .scene
        .world
        .query::<&GlyphGroupBinding>()
        .iter(&controller.scene.world)
        .map(|b| b.group_id)
        .max()
        .unwrap_or(0);
    let total_groups = (max_gid + 1) as usize;
    let mut groups = vec![GroupRow::identity([0.0, 0.0, 0.0]); total_groups.max(1)];
    controller.scene.sync_all_to_group_rows(&mut groups);

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
