use bevy_ecs::prelude::{ChildOf, Entity};
use bevy_transform::prelude::Transform;
use glam::{Quat, Vec3};

use crate::agent_transcript::{AgentSession, AgentTurn, TranscriptEvent, TranscriptEventKind};
use crate::atlas::Atlas;
use crate::glyph_scene::{GlyphInstance, GroupRow};
use crate::layout::GlyphArena;
use crate::layout_stack::LayoutController;
use crate::repo::{RepoLayoutMode, RepoParams};
use crate::revision::{FileRevision, RevisionEngine};
use crate::spatial_scene::GlyphGroupBinding;
use crate::text::{pack_rgba8, StagedText, CELL_HEIGHT_WORLD, LINE_HEIGHT_FACTOR};

/// The transcript text palette: `[agent_text]` in config/defaults.toml.
/// Colour says WHICH KIND of event a card is: the card's banner carries the
/// kind's hue (`[agent_cards.banners]`) and every section heading inside the
/// page takes its kind's text accent, so a run of edits reads as a run of one
/// colour while scrolling the deck. Titles stay white — they overlap their
/// banner, and same-hue text on it loses contrast. Everything else uses a few
/// neutral roles (body, meta, muted, rule, diff).
fn pal() -> &'static crate::config::AgentTextSettings {
    &crate::config::settings().agent_text
}

/// Wrap prose into lines with a maximum column width, breaking at spaces when possible.
/// Uses character counts and Unicode scalar boundaries rather than byte slicing to avoid panics.
fn wrap_prose(text: &str, max_cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_cols = 0;
        for word in trimmed.split_whitespace() {
            let word_cols = word.chars().count();
            if cur.is_empty() {
                if word_cols > max_cols {
                    // Split oversized word across lines safely by characters
                    let mut chunk = String::new();
                    let mut chunk_cols = 0;
                    for ch in word.chars() {
                        if chunk_cols >= max_cols {
                            out.push(chunk);
                            chunk = String::new();
                            chunk_cols = 0;
                        }
                        chunk.push(ch);
                        chunk_cols += 1;
                    }
                    if !chunk.is_empty() {
                        cur = chunk;
                        cur_cols = chunk_cols;
                    }
                } else {
                    cur.push_str(word);
                    cur_cols = word_cols;
                }
            } else if cur_cols + 1 + word_cols <= max_cols {
                cur.push(' ');
                cur.push_str(word);
                cur_cols += 1 + word_cols;
            } else {
                out.push(cur);
                cur = String::new();
                cur_cols = 0;
                if word_cols > max_cols {
                    let mut chunk = String::new();
                    let mut chunk_cols = 0;
                    for ch in word.chars() {
                        if chunk_cols >= max_cols {
                            out.push(chunk);
                            chunk = String::new();
                            chunk_cols = 0;
                        }
                        chunk.push(ch);
                        chunk_cols += 1;
                    }
                    if !chunk.is_empty() {
                        cur = chunk;
                        cur_cols = chunk_cols;
                    }
                } else {
                    cur.push_str(word);
                    cur_cols = word_cols;
                }
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// Normalize text ensuring literal escaped newlines are converted to actual linebreaks.
pub fn normalize_multiline_text(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\n') && text.contains("\\n") {
        std::borrow::Cow::Owned(
            text.replace("\\r\\n", "\n")
                .replace("\\n", "\n")
                .replace("\\r", "\n")
                .replace("\\t", "\t")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\"),
        )
    } else {
        std::borrow::Cow::Borrowed(text)
    }
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
            pal().title,
        ),
        (
            format!(
                "Mind • {} tool call(s) • {} think block(s)",
                turn.tool_calls.len(),
                turn.thinking.len()
            ),
            pal().subtitle,
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            pal().rule,
        ),
    ]
}

/// Format the body lines for the Left Page (Mind) of an Agent Turn Card.
fn format_turn_left_body(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    // 1. User Prompt
    lines.push(("[ USER PROMPT ]".to_string(), pal().kinds.user_prompt));
    if let Some(ref prompt) = turn.prompt {
        for l in wrap_prose(prompt, 72) {
            lines.push((format!("  {l}"), pal().body));
        }
    } else {
        lines.push(("  (No user prompt in this turn)".to_string(), pal().muted));
    }

    lines.push((
        "─────────────────────────────────────────────────────────────".to_string(),
        pal().rule,
    ));

    // 2. Reasoning / Chain of Thought
    lines.push((
        "[ CHAIN OF THOUGHT / REASONING ]".to_string(),
        pal().kinds.thinking,
    ));
    let thinking_text = turn.combined_thinking();
    if !thinking_text.is_empty() {
        for l in wrap_prose(&thinking_text, 72) {
            lines.push((format!("  {l}"), pal().body));
        }
    } else {
        lines.push((
            "  (No internal chain-of-thought logged in transcript)".to_string(),
            pal().muted,
        ));
    }

    lines
}

/// Format the fixed header lines for the Right Page (Material Impact) of an Agent Turn Card.
fn format_turn_right_header(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    vec![
        (
            format!("TURN {} MATERIAL IMPACT", turn.turn_index + 1),
            pal().title,
        ),
        (
            format!(
                "Impact • {} file action(s) • {} tool invocation(s)",
                turn.file_actions.len(),
                turn.tool_calls.len()
            ),
            pal().subtitle,
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            pal().rule,
        ),
    ]
}

/// Format the body lines for the Right Page (Material Impact) of an Agent Turn Card.
fn format_turn_right_body(turn: &AgentTurn) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    // 1. Touched Files
    lines.push((
        format!("[ TOUCHED FILES ({}) ]", turn.file_actions.len()),
        pal().heading,
    ));
    if turn.file_actions.is_empty() {
        lines.push((
            "  • No file modifications in this turn".to_string(),
            pal().muted,
        ));
    } else {
        for fa in &turn.file_actions {
            let (tag, color) = match fa.action {
                crate::spatial_scene::workdesk::FileActionKind::Edit => ("Edit", pal().kinds.file_edit),
                crate::spatial_scene::workdesk::FileActionKind::Write => ("Write", pal().kinds.file_write),
                crate::spatial_scene::workdesk::FileActionKind::Read => ("Read", pal().kinds.file_read),
                crate::spatial_scene::workdesk::FileActionKind::AstAnalysis => ("Ast", pal().kinds.ast_analysis),
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
        pal().rule,
    ));

    // 2. Assistant Response
    lines.push(("[ ASSISTANT RESPONSE ]".to_string(), pal().kinds.assistant_response));
    let assistant_text = turn.combined_assistant_text();
    if !assistant_text.is_empty() {
        for l in wrap_prose(&assistant_text, 72) {
            lines.push((format!("  {l}"), pal().body));
        }
    } else {
        lines.push((
            "  (No assistant text message recorded in this turn)".to_string(),
            pal().muted,
        ));
    }

    // 3. Tool Invocations
    if !turn.tool_calls.is_empty() {
        lines.push((
            "─────────────────────────────────────────────────────────────".to_string(),
            pal().rule,
        ));
        lines.push((
            format!("[ TOOL INVOCATIONS ({}) ]", turn.tool_calls.len()),
            pal().kinds.tool_invocation,
        ));
        for tc in &turn.tool_calls {
            let status = if tc.is_error { "error" } else { "ok" };
            lines.push((format!("  • {} [{status}]", tc.name), pal().body));
        }
    }

    lines
}

/// Format the fixed header lines for the Left Page (Spec / Metadata) of an Atomic Beat Card.
fn format_beat_left_header(event: &TranscriptEvent) -> Vec<(String, [u8; 3])> {
    let (tag, subtitle) = match &event.kind {
        TranscriptEventKind::UserPrompt { .. } => ("USER REQUEST", "Interactive Input • Mind"),
        TranscriptEventKind::Thinking { .. } => ("INTERNAL REASONING", "Chain of Thought • Mind"),
        TranscriptEventKind::FileRead { .. } => {
            ("FILE READ", "Inspection • Workspace Spec")
        }
        TranscriptEventKind::FileEdit { .. } => {
            ("FILE EDIT", "Source Mutation • Spec & Hunks")
        }
        TranscriptEventKind::FileWrite { .. } => {
            ("FILE WRITE", "New File Creation • Workspace Spec")
        }
        TranscriptEventKind::Command { .. } => {
            ("COMMAND CONSOLE", "Shell Execution • Process Spec")
        }
        TranscriptEventKind::ToolInvocation { .. } => {
            ("TOOL INVOCATION", "Agent Action • Invocation Spec")
        }
        TranscriptEventKind::AssistantResponse { .. } => {
            ("ASSISTANT RESPONSE", "Agent Dialogue • Outgoing Message")
        }
    };

    vec![
        (
            format!("BEAT {}: {tag}", event.index + 1),
            pal().title,
        ),
        (
            format!("Turn {} • {subtitle}", event.turn_index + 1),
            pal().subtitle,
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            pal().rule,
        ),
    ]
}

/// Format the body lines for the Left Page (Spec / Metadata) of an Atomic Beat Card.
fn format_beat_left_body(event: &TranscriptEvent) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    match &event.kind {
        TranscriptEventKind::UserPrompt { prompt } => {
            lines.push(("[ USER REQUEST SPEC ]".to_string(), pal().kinds.user_prompt));
            lines.push((
                format!(
                    "  Length: {} chars │ {} lines",
                    prompt.len(),
                    prompt.lines().count()
                ),
                pal().meta,
            ));
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ PROMPT OVERVIEW ]".to_string(), pal().kinds.user_prompt));
            for l in wrap_prose(prompt, 72).into_iter().take(20) {
                lines.push((format!("  {l}"), pal().body));
            }
        }
        TranscriptEventKind::Thinking { thought } => {
            lines.push(("[ REASONING METRICS ]".to_string(), pal().kinds.thinking));
            let word_count = thought.split_whitespace().count();
            lines.push((
                format!(
                    "  Length: {} chars │ {} words │ {} lines",
                    thought.len(),
                    word_count,
                    thought.lines().count()
                ),
                pal().meta,
            ));
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ THOUGHT SUMMARY ]".to_string(), pal().kinds.thinking));
            for l in wrap_prose(thought, 72).into_iter().take(20) {
                lines.push((format!("  {l}"), pal().body));
            }
        }
        TranscriptEventKind::FileRead {
            file_path,
            action_record,
            content,
        } => {
            lines.push(("[ FILE READ SPEC ]".to_string(), pal().kinds.file_read));
            lines.push((format!("  Target: {file_path}"), pal().emphasis));
            if !action_record.summary.is_empty() {
                lines.push((
                    format!("  Summary: {}", action_record.summary),
                    pal().meta,
                ));
            }
            if let Some(c) = content {
                lines.push((
                    format!(
                        "  Content Size: {} lines │ {} bytes",
                        c.lines().count(),
                        c.len()
                    ),
                    pal().meta,
                ));
            }
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ ACTION METADATA ]".to_string(), pal().kinds.file_read));
            lines.push((
                format!("  Tool ID: {}", action_record.tool_id),
                pal().meta,
            ));
            lines.push((
                "  Kind: Inspect / Read whole file into context".to_string(),
                pal().meta,
            ));
        }
        TranscriptEventKind::FileEdit {
            file_path,
            action_record,
            post_edit_content,
        } => {
            lines.push(("[ FILE EDIT SPEC ]".to_string(), pal().kinds.file_edit));
            lines.push((format!("  Target: {file_path}"), pal().emphasis));
            if !action_record.summary.is_empty() {
                lines.push((
                    format!("  Summary: {}", action_record.summary),
                    pal().meta,
                ));
            }
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ DIFF STATISTICS ]".to_string(), pal().kinds.file_edit));
            let added: usize = action_record
                .hunks
                .iter()
                .map(|h| h.lines.iter().filter(|l| l.starts_with('+')).count())
                .sum();
            let removed: usize = action_record
                .hunks
                .iter()
                .map(|h| h.lines.iter().filter(|l| l.starts_with('-')).count())
                .sum();
            lines.push((
                format!(
                    "  +{added} additions │ -{removed} deletions │ {} hunks",
                    action_record.hunks.len()
                ),
                pal().meta,
            ));
            if let Some(ref post) = post_edit_content {
                lines.push((
                    format!(
                        "  Post-Edit File Size: {} lines │ {} bytes",
                        post.lines().count(),
                        post.len()
                    ),
                    pal().meta,
                ));
            }
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ HUNKS OVERVIEW ]".to_string(), pal().kinds.file_edit));
            for (hi, hunk) in action_record.hunks.iter().enumerate().take(6) {
                lines.push((
                    format!(
                        "  Hunk #{}: -{},{} +{},{}",
                        hi + 1,
                        hunk.old_start,
                        hunk.old_lines,
                        hunk.new_start,
                        hunk.new_lines
                    ),
                    pal().diff_hunk,
                ));
            }
        }
        TranscriptEventKind::FileWrite {
            file_path,
            action_record,
            content,
        } => {
            lines.push(("[ FILE WRITE SPEC ]".to_string(), pal().kinds.file_write));
            lines.push((format!("  Created: {file_path}"), pal().emphasis));
            if !action_record.summary.is_empty() {
                lines.push((
                    format!("  Summary: {}", action_record.summary),
                    pal().meta,
                ));
            }
            if let Some(c) = content {
                lines.push((
                    format!(
                        "  Size: {} lines │ {} bytes",
                        c.lines().count(),
                        c.len()
                    ),
                    pal().meta,
                ));
            }
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ ACTION METADATA ]".to_string(), pal().kinds.file_write));
            lines.push((
                format!("  Tool ID: {}", action_record.tool_id),
                pal().meta,
            ));
            lines.push((
                "  Kind: Write new file to workspace".to_string(),
                pal().meta,
            ));
        }
        TranscriptEventKind::Command {
            name,
            command_line,
            output,
            is_error,
        } => {
            lines.push(("[ COMMAND SPEC ]".to_string(), pal().kinds.command));
            lines.push((format!("  Tool: {name}"), pal().emphasis));
            let status_str = if *is_error {
                "FAILED (Non-zero exit)"
            } else {
                "SUCCESS (0)"
            };
            let status_color = if *is_error {
                pal().error
            } else {
                pal().ok
            };
            lines.push((format!("  Status: {status_str}"), status_color));
            if let Some(out) = output {
                lines.push((
                    format!(
                        "  Output Captured: {} lines │ {} bytes",
                        out.lines().count(),
                        out.len()
                    ),
                    pal().meta,
                ));
            }
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ COMMAND LINE ]".to_string(), pal().kinds.command));
            for l in wrap_prose(&format!("$ {command_line}"), 72) {
                lines.push((format!("  {l}"), pal().body));
            }
        }
        TranscriptEventKind::ToolInvocation {
            name,
            input,
            output,
            is_error,
        } => {
            lines.push(("[ TOOL INVOCATION SPEC ]".to_string(), pal().kinds.tool_invocation));
            lines.push((format!("  Tool: {name}"), pal().emphasis));
            let status_str = if *is_error { "ERROR" } else { "SUCCESS" };
            let status_color = if *is_error {
                pal().error
            } else {
                pal().ok
            };
            lines.push((format!("  Status: {status_str}"), status_color));
            if let Some(out) = output {
                lines.push((
                    format!(
                        "  Response Size: {} lines │ {} bytes",
                        out.lines().count(),
                        out.len()
                    ),
                    pal().meta,
                ));
            }
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ INPUT ARGUMENTS ]".to_string(), pal().kinds.tool_invocation));
            let input_str = serde_json::to_string_pretty(input).unwrap_or_default();
            for l in input_str.lines().take(20) {
                lines.push((format!("  {l}"), pal().body));
            }
        }
        TranscriptEventKind::AssistantResponse { message } => {
            lines.push(("[ ASSISTANT MESSAGE SPEC ]".to_string(), pal().kinds.assistant_response));
            lines.push((
                format!(
                    "  Length: {} chars │ {} lines",
                    message.len(),
                    message.lines().count()
                ),
                pal().meta,
            ));
            lines.push((
                "─────────────────────────────────────────────────────────────".to_string(),
                pal().rule,
            ));
            lines.push(("[ MESSAGE SUMMARY ]".to_string(), pal().kinds.assistant_response));
            for l in wrap_prose(message, 72).into_iter().take(20) {
                lines.push((format!("  {l}"), pal().body));
            }
        }
    }

    lines
}

/// Format the fixed header lines for the Right Page (Artifact / Payload) of an Atomic Beat Card.
fn format_beat_right_header(event: &TranscriptEvent) -> Vec<(String, [u8; 3])> {
    let (tag, subtitle) = match &event.kind {
        TranscriptEventKind::UserPrompt { .. } => ("USER PROMPT PAYLOAD", "Raw User Prompt Text"),
        TranscriptEventKind::Thinking { .. } => ("FULL CHAIN OF THOUGHT", "Internal Model Deliberation"),
        TranscriptEventKind::FileRead { .. } => {
            ("FILE SNAPSHOT", "Scale-presented whole file")
        }
        TranscriptEventKind::FileEdit { .. } => {
            ("FILE ARTIFACT & DIFF", "Edited lines highlighted in context")
        }
        TranscriptEventKind::FileWrite { .. } => {
            ("CREATED FILE ARTIFACT", "Complete file content")
        }
        TranscriptEventKind::Command { .. } => {
            ("TERMINAL CONSOLE OUTPUT", "Stdout / Stderr Stream Capture")
        }
        TranscriptEventKind::ToolInvocation { .. } => {
            ("TOOL RESULT PAYLOAD", "Execution Output / Response")
        }
        TranscriptEventKind::AssistantResponse { .. } => {
            ("ASSISTANT MESSAGE BODY", "Complete conversational response")
        }
    };

    vec![
        (
            format!("ARTIFACT: {tag}"),
            pal().title,
        ),
        (
            format!("Beat {} • {subtitle}", event.index + 1),
            pal().subtitle,
        ),
        (
            "─────────────────────────────────────────────────────────────".to_string(),
            pal().rule,
        ),
    ]
}

/// Format the body lines for the Right Page (Artifact / Payload) of an Atomic Beat Card.
fn format_beat_right_body(
    event: &TranscriptEvent,
    revision_engine: &RevisionEngine,
) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    match &event.kind {
        TranscriptEventKind::UserPrompt { prompt } => {
            for l in wrap_prose(prompt, 72) {
                lines.push((l, pal().body));
            }
        }
        TranscriptEventKind::Thinking { thought } => {
            for l in wrap_prose(thought, 72) {
                lines.push((l, pal().body));
            }
        }
        TranscriptEventKind::FileRead {
            file_path,
            content,
            ..
        } => {
            let text_opt = content.as_deref().or_else(|| {
                revision_engine
                    .history(file_path)
                    .and_then(|h| h.get(0))
                    .map(|r| r.text.as_str())
            });
            if let Some(raw_text) = text_opt {
                let text = normalize_multiline_text(raw_text);
                for (line_idx, l) in text.lines().take(5000).enumerate() {
                    let trimmed = l.trim_start();
                    let color = if trimmed.starts_with("//")
                        || trimmed.starts_with('#')
                        || trimmed.starts_with("/*")
                        || trimmed.starts_with('*')
                    {
                        pal().comment
                    } else {
                        pal().body
                    };
                    lines.push((format!("{:4} │ {}", line_idx + 1, l), color));
                }
            } else {
                lines.push((
                    "  (File content read into agent context)".to_string(),
                    pal().muted,
                ));
            }
        }
        TranscriptEventKind::FileEdit {
            file_path,
            action_record,
            post_edit_content,
        } => {
            // 1. Diff Hunks
            if !action_record.hunks.is_empty() {
                lines.push(("[ UNIFIED DIFF HUNKS ]".to_string(), pal().kinds.file_edit));
                for hunk in &action_record.hunks {
                    lines.push((
                        format!(
                            "@@ -{},{} +{},{} @@",
                            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                        ),
                        pal().diff_hunk,
                    ));
                    for hl in &hunk.lines {
                        if hl.starts_with('+') {
                            lines.push((hl.clone(), pal().diff_add));
                        } else if hl.starts_with('-') {
                            lines.push((hl.clone(), pal().diff_remove));
                        } else {
                            lines.push((hl.clone(), pal().diff_context));
                        }
                    }
                }
                lines.push((
                    "─────────────────────────────────────────────────────────────".to_string(),
                    pal().rule,
                ));
            }

            // 2. Full post-edit file with edited lines highlighted
            let text_opt = post_edit_content.as_deref().or_else(|| {
                revision_engine
                    .history(file_path)
                    .and_then(|h| h.revision_for_turn(event.turn_index))
                    .map(|r| r.text.as_str())
            });

            if let Some(raw_text) = text_opt {
                let text = normalize_multiline_text(raw_text);
                lines.push((
                    "[ FULL FILE WITH HIGHLIGHTED EDITS ]".to_string(),
                    pal().kinds.file_edit,
                ));
                let mut added_lines = std::collections::HashSet::new();
                for hunk in &action_record.hunks {
                    let mut cur = hunk.new_start;
                    for hl in &hunk.lines {
                        if hl.starts_with('+') {
                            added_lines.insert(cur);
                            cur += 1;
                        } else if !hl.starts_with('-') {
                            cur += 1;
                        }
                    }
                }

                for (line_idx, l) in text.lines().take(5000).enumerate() {
                    let line_no = line_idx + 1;
                    if added_lines.contains(&line_no) {
                        lines.push((format!("{:4} +│ {}", line_no, l), pal().diff_add));
                    } else {
                        let trimmed = l.trim_start();
                        let color = if trimmed.starts_with("//")
                            || trimmed.starts_with('#')
                            || trimmed.starts_with("/*")
                            || trimmed.starts_with('*')
                        {
                            pal().comment
                        } else {
                            pal().body
                        };
                        lines.push((format!("{:4}  │ {}", line_no, l), color));
                    }
                }
            } else {
                lines.push((
                    "  (Post-edit snapshot not recorded)".to_string(),
                    pal().muted,
                ));
            }
        }
        TranscriptEventKind::FileWrite {
            file_path,
            content,
            ..
        } => {
            let text_opt = content.as_deref().or_else(|| {
                revision_engine
                    .history(file_path)
                    .and_then(|h| h.revision_for_turn(event.turn_index))
                    .map(|r| r.text.as_str())
            });
            if let Some(raw_text) = text_opt {
                let text = normalize_multiline_text(raw_text);
                for (line_idx, l) in text.lines().take(5000).enumerate() {
                    let trimmed = l.trim_start();
                    let color = if trimmed.starts_with("//")
                        || trimmed.starts_with('#')
                        || trimmed.starts_with("/*")
                        || trimmed.starts_with('*')
                    {
                        pal().comment
                    } else {
                        pal().body
                    };
                    lines.push((format!("{:4} │ {}", line_idx + 1, l), color));
                }
            } else {
                lines.push(("  (Empty file created)".to_string(), pal().muted));
            }
        }
        TranscriptEventKind::Command {
            output, is_error, ..
        } => {
            if let Some(raw_out) = output {
                let out = normalize_multiline_text(raw_out);
                let color = if *is_error {
                    pal().error
                } else {
                    pal().body
                };
                for l in out.lines().take(5000) {
                    lines.push((l.to_string(), color));
                }
            } else {
                lines.push((
                    "  (Command produced no stdout/stderr output)".to_string(),
                    pal().muted,
                ));
            }
        }
        TranscriptEventKind::ToolInvocation {
            output, is_error, ..
        } => {
            if let Some(raw_out) = output {
                let out = normalize_multiline_text(raw_out);
                let color = if *is_error {
                    pal().error
                } else {
                    pal().body
                };
                for l in out.lines().take(5000) {
                    lines.push((l.to_string(), color));
                }
            } else {
                lines.push((
                    "  (Tool completed with no response payload)".to_string(),
                    pal().muted,
                ));
            }
        }
        TranscriptEventKind::AssistantResponse { message } => {
            for l in wrap_prose(message, 72) {
                lines.push((l, pal().body));
            }
        }
    }

    lines
}

/// Helper to extract clean shortened path for card display.
pub fn format_short_file_name(file_path: &str) -> String {
    let parts: Vec<&str> = file_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() > 2 {
        parts[parts.len() - 2..].join("/")
    } else if let Some(last) = parts.last() {
        last.to_string()
    } else {
        file_path.to_string()
    }
}

/// Format the single crisp header line for a Workdesk File Revision Card ($R_k$).
///
/// Clean, atomic identifier: no redundant turn numbers, no synthetic summaries, no extra rules.
fn format_revision_card_header(rev: &FileRevision, file_path: &str) -> Vec<(String, [u8; 3])> {
    let (action_str, color) = match rev.action {
        crate::spatial_scene::workdesk::FileActionKind::Edit => ("Edit", pal().kinds.file_edit),
        crate::spatial_scene::workdesk::FileActionKind::Write => ("Write", pal().kinds.file_write),
        crate::spatial_scene::workdesk::FileActionKind::Read => ("Read", pal().kinds.file_read),
        crate::spatial_scene::workdesk::FileActionKind::AstAnalysis => ("Ast", pal().kinds.ast_analysis),
    };
    let short_name = format_short_file_name(file_path);
    vec![
        (
            format!("R{} • {action_str} • {short_name}", rev.revision_index),
            color,
        ),
    ]
}

/// Format the body lines for a Workdesk File Revision Card ($R_k$).
///
/// Whole-chunk singular page: renders the entire file (or read portion)
/// with edited/added lines highlighted in context (+│ in green/amber ink).
/// Scaling is applied to the child glyph field to fit wholly within the page.
fn format_revision_card_body(rev: &FileRevision) -> Vec<(String, [u8; 3])> {
    let mut lines = Vec::new();

    let norm_text = normalize_multiline_text(&rev.text);
    if !norm_text.is_empty() {
        // Collect line numbers that were added or modified in this revision
        let mut added_lines = std::collections::HashSet::new();
        for hunk in &rev.hunks {
            let mut cur = hunk.new_start;
            for hl in &hunk.lines {
                if hl.starts_with('+') {
                    added_lines.insert(cur);
                    cur += 1;
                } else if !hl.starts_with('-') {
                    cur += 1;
                }
            }
        }

        // Render whole file / chunk with highlighted edits
        for (line_idx, l) in norm_text.lines().take(5000).enumerate() {
            let line_no = line_idx + 1;
            if added_lines.contains(&line_no) {
                lines.push((format!("{:4} +│ {}", line_no, l), pal().diff_add));
            } else {
                let trimmed = l.trim_start();
                let color = if trimmed.starts_with("//")
                    || trimmed.starts_with('#')
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*')
                {
                    pal().comment
                } else {
                    pal().body
                };
                lines.push((format!("{:4}  │ {}", line_no, l), color));
            }
        }
    } else if !rev.hunks.is_empty() {
        // Diff-only fallback if full snapshot text is unavailable
        for hunk in &rev.hunks {
            lines.push((
                format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                ),
                pal().diff_hunk,
            ));
            for hl in &hunk.lines {
                if hl.starts_with('+') {
                    lines.push((hl.clone(), pal().diff_add));
                } else if hl.starts_with('-') {
                    lines.push((hl.clone(), pal().diff_remove));
                } else {
                    lines.push((hl.clone(), pal().diff_context));
                }
            }
        }
    } else {
        lines.push((
            "  (Empty file)".to_string(),
            pal().muted,
        ));
    }

    lines
}

/// Stage an Agent Session and its Workdesk into a visual 3D scene with custom layout options.
pub fn stage_agent_session_with_options(
    atlas: Option<&Atlas>,
    _slot_ink: &[Option<[f32; 4]>],
    session: AgentSession,
    revision_engine: RevisionEngine,
    layout_options: crate::spatial_scene::CarrelLayoutOptions,
) -> StagedText {
    let params = RepoParams {
        layout_mode: RepoLayoutMode::Carrel,
        ..Default::default()
    };
    let mut controller = LayoutController::new(params);

    controller.spawn_agent_carrel_session_with_options(session.clone(), revision_engine.clone(), layout_options);

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

    // Focus camera directly on the active front workstation (Deck on left, Workdesk on right)
    // rather than the entire deep multi-turn Z-splay.
    let focus_bounds = Some(([50.0, -10.0], [175.0, 60.0]));

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

    #[derive(Clone, Copy)]
    struct GroupInfo {
        slot_base: u32,
        slot_count: u32,
        local_min: [f32; 3],
        local_max: [f32; 3],
    }

    let mut group_info: Vec<Option<GroupInfo>> = Vec::new();
    let record_group = |gid: u32, instances: &[GlyphInstance], base: u32, info: &mut Vec<Option<GroupInfo>>| {
        let count = (instances.len() as u32).saturating_sub(base);
        let (min, max) = if count > 0 {
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut min_z = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut max_z = f32::NEG_INFINITY;
            for inst in &instances[base as usize..] {
                min_x = min_x.min(inst.pos[0]);
                min_y = min_y.min(inst.pos[1] - inst.height);
                min_z = min_z.min(inst.pos[2]);
                max_x = max_x.max(inst.pos[0] + inst.advance);
                max_y = max_y.max(inst.pos[1]);
                max_z = max_z.max(inst.pos[2]);
            }
            ([min_x, min_y, min_z], [max_x, max_y, max_z])
        } else {
            ([0.0, 0.0, 0.0], [0.0, 0.0, 0.0])
        };
        let idx = gid as usize;
        if idx >= info.len() {
            info.resize_with(idx + 1, || None);
        }
        info[idx] = Some(GroupInfo {
            slot_base: base,
            slot_count: count,
            local_min: min,
            local_max: max,
        });
    };

    // 1. Turn Deck Turn Cards: Left (Mind) and Right (Impact) Pages
    let mut turn_query = controller
        .scene
        .world
        .query::<(&crate::spatial_scene::AgentTurnCard, Entity)>();
    let turn_cards: Vec<(crate::spatial_scene::AgentTurnCard, Entity)> = turn_query
        .iter(&controller.scene.world)
        .map(|(c, e)| (c.clone(), e))
        .collect();

    let events = session.linearize_events(Some(&revision_engine));

    for (card, _card_e) in turn_cards {
        let (left_hdr, left_body, right_hdr, right_body) =
            if let Some(event) = events.get(card.event_index) {
                (
                    format_beat_left_header(event),
                    format_beat_left_body(event),
                    format_beat_right_header(event),
                    format_beat_right_body(event, &revision_engine),
                )
            } else if let Some(turn) = session.turns.get(card.turn_index) {
                (
                    format_turn_left_header(turn),
                    format_turn_left_body(turn),
                    format_turn_right_header(turn),
                    format_turn_right_body(turn),
                )
            } else {
                continue;
            };

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

            let base_lh = raw_instances.len() as u32;
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
            }
            record_group(left_hdr_gid, &raw_instances, base_lh, &mut group_info);

            let base_lb = raw_instances.len() as u32;
            if let Some(atlas) = atlas {
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
            record_group(left_body_gid, &raw_instances, base_lb, &mut group_info);

            // Right Page
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

            let base_rh = raw_instances.len() as u32;
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
            }
            record_group(right_hdr_gid, &raw_instances, base_rh, &mut group_info);

            let base_rb = raw_instances.len() as u32;
            if let Some(atlas) = atlas {
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
            record_group(right_body_gid, &raw_instances, base_rb, &mut group_info);
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

                let s_y = 32.5 / unscaled_h;
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
                        translation: Vec3::new(2.0, -4.5, 0.2),
                        scale: Vec3::new(scale, scale, 1.0),
                        rotation: Quat::IDENTITY,
                    },
                    ChildOf(card_e),
                    GlyphGroupBinding {
                        group_id: body_gid,
                        tint: [1.0, 1.0, 1.0, 1.0],
                    },
                ));

                let base_h = raw_instances.len() as u32;
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
                }
                record_group(header_gid, &raw_instances, base_h, &mut group_info);

                let base_b = raw_instances.len() as u32;
                if let Some(atlas) = atlas {
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
                record_group(body_gid, &raw_instances, base_b, &mut group_info);
            }
        }
    }

    // 3. Workdesk Stack Title Labels (queried directly from ECS into Group 0)
    let mut stack_query = controller.scene.world.query::<(
        &crate::spatial_scene::FileRevisionStack,
        &bevy_transform::prelude::GlobalTransform,
    )>();
    let stacks: Vec<(crate::spatial_scene::FileRevisionStack, glam::Vec3)> = stack_query
        .iter(&controller.scene.world)
        .map(|(s, g)| (s.clone(), g.translation()))
        .collect();

    let base_labels = raw_instances.len() as u32;
    if let Some(atlas) = atlas {
        for (stack, trans) in stacks {
            let rev_count = revision_engine
                .history(&stack.file_path)
                .map(|h| h.revisions.len())
                .unwrap_or(stack.revision_count);
            let short_name = format_short_file_name(&stack.file_path);
            let label = format!("{} (R{})", short_name, rev_count);
            layout_colored_lines(
                atlas,
                &[(label, pal().emphasis)],
                [trans.x + 1.0, trans.y + 2.0, trans.z + 0.2],
                55,
                1,
                0,
                &mut raw_instances,
                &mut codepoints_decoded,
            );
        }
    }
    record_group(0, &raw_instances, base_labels, &mut group_info);

    // Update global transforms for all child glyph fields
    controller.scene.update_transforms();

    let glyphs_emitted = raw_instances.len();

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

    // Build per-group SegCull segments so frustum culling actively culls off-screen cards
    let mut segments = Vec::with_capacity(total_groups);
    for (gid, g) in groups.iter().enumerate().take(total_groups) {
        let off = [g.cols[0][0], g.cols[0][1], g.cols[0][2]];
        let sc = [
            if g.cols[3][0].abs() > 1e-6 { g.cols[3][0] } else { 1.0 },
            if g.cols[3][1].abs() > 1e-6 { g.cols[3][1] } else { 1.0 },
            if g.cols[3][2].abs() > 1e-6 { g.cols[3][2] } else { 1.0 },
        ];
        if let Some(Some(data)) = group_info.get(gid) {
            if data.slot_count > 0 {
                let w_min = [
                    data.local_min[0] * sc[0] + off[0] - crate::glyph_scene::SEG_CULL_PAD_MIN[0],
                    data.local_min[1] * sc[1] + off[1] - crate::glyph_scene::SEG_CULL_PAD_MIN[1],
                    data.local_min[2] * sc[2] + off[2] - 0.2,
                ];
                let w_max = [
                    data.local_max[0] * sc[0] + off[0] + crate::glyph_scene::SEG_CULL_PAD_MAX[0],
                    data.local_max[1] * sc[1] + off[1] + crate::glyph_scene::SEG_CULL_PAD_MAX[1],
                    data.local_max[2] * sc[2] + off[2] + 0.2,
                ];
                let tint = [g.cols[2][0], g.cols[2][1], g.cols[2][2], 0.7];
                segments.push(crate::glyph_scene::SegCull {
                    min: w_min,
                    max: w_max,
                    slot_base: data.slot_base,
                    slot_count: data.slot_count,
                    tint,
                    blocks: Vec::new(),
                });
                continue;
            }
        }
        segments.push(crate::glyph_scene::SegCull {
            min: off,
            max: off,
            slot_base: 0,
            slot_count: 0,
            tint: [0.0; 4],
            blocks: Vec::new(),
        });
    }

    let instances = GlyphArena::from_vec(raw_instances);

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
        item_params: Vec::new(),
    }
}

/// Stage an Agent Session and its Workdesk into a visual 3D scene with default options.
pub fn stage_agent_session(
    atlas: Option<&Atlas>,
    slot_ink: &[Option<[f32; 4]>],
    session: AgentSession,
    revision_engine: RevisionEngine,
) -> StagedText {
    stage_agent_session_with_options(
        atlas,
        slot_ink,
        session,
        revision_engine,
        crate::spatial_scene::CarrelLayoutOptions::default(),
    )
}
