use std::path::PathBuf;
use crate::glyph_scene::{PickCommand, Verb};
use super::args::RawOps;

/// Stage G: one scripted operation (picks and verbs interleave in CLI order).
#[derive(Clone, Debug)]
pub enum Op {
    Pick(PickCommand),
    Verb(Verb),
    /// Scripted Fly-camera pose: eye + yaw/pitch (RADIANS) — repro of
    /// oblique windowed camera states for --pick-px.
    CamPose([f32; 3], f32, f32),
    /// S3 spike: apply a Zed-sidecar highlight to instance colors (offscreen,
    /// before the first frame — same op-stream slot as picks/verbs).
    Highlight(PathBuf),
}

/// --pick-row/--pick-col upgrade the most recent --pick-file pick into a
/// deterministic RowCol pick (defaults: row 0 / col 0 for the unset half).
pub(crate) fn set_pick_row_col(ops: &mut Vec<Op>, row: Option<u32>, col: Option<u32>) {
    match ops.last_mut() {
        Some(Op::Pick(PickCommand::File(f))) => {
            let f = f.clone();
            ops.pop();
            ops.push(Op::Pick(PickCommand::RowCol {
                file: f,
                row: row.unwrap_or(0),
                col: col.unwrap_or(0),
            }));
        }
        Some(Op::Pick(PickCommand::RowCol { row: r, col: c, .. })) => {
            if let Some(row) = row {
                *r = row;
            }
            if let Some(col) = col {
                *c = col;
            }
        }
        _ => panic!("--pick-row/--pick-col must follow --pick-file"),
    }
}

/// Parse a `--verb` string into a Verb (clap `value_parser`). Forms:
///   recolor-glyph `[rrggbb]`      recolor-line `[rrggbb]`
///   nudge-glyph dx dy `[dz]`      scale-glyph f
///   move-group dx dy dz         scale-group s
///   tint-group rrggbb           tint-cycle
///   hide-group | show-group | toggle-hidden
///   set-glyph-background rrggbb`[aa]`   set-glyph-transform tx ty tz `[s]`
///   reset-glyph-group
/// (the last three, 2026-10-10: the group-per-glyph verbs had no CLI form,
/// so nothing scripted could reach them — the M3 witness needs one.)
pub fn parse_verb(s: &str) -> Result<Verb, String> {
    let t: Vec<&str> = s.split_whitespace().collect();
    let usage = format!(
        "unknown/malformed --verb {s:?} — expected recolor-glyph|recolor-line|\
         nudge-glyph|scale-glyph|move-group|scale-group|tint-group|tint-cycle|\
         hide-group|show-group|toggle-hidden|set-glyph-background|\
         set-glyph-transform|reset-glyph-group"
    );
    let f = |i: usize| -> Result<f32, String> {
        t.get(i)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("--verb {s:?}: bad/missing float at position {i}"))
    };
    let hex = |i: usize| -> Result<[u8; 3], String> {
        let h = t
            .get(i)
            .ok_or_else(|| format!("--verb {s:?}: missing rrggbb at position {i}"))?
            .trim_start_matches('#');
        let v = u32::from_str_radix(h, 16)
            .map_err(|_| format!("--verb {s:?}: bad hex color {h:?}"))?;
        Ok([((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8])
    };
    // rrggbb or rrggbbaa (alpha defaults to ff), as sRGB floats.
    let hex_rgba = |i: usize| -> Result<[f32; 4], String> {
        let h = t
            .get(i)
            .ok_or_else(|| format!("--verb {s:?}: missing rrggbb[aa] at position {i}"))?
            .trim_start_matches('#');
        if h.len() != 6 && h.len() != 8 {
            return Err(format!("--verb {s:?}: bad hex color {h:?} (want rrggbb or rrggbbaa)"));
        }
        let v = u32::from_str_radix(h, 16).map_err(|_| format!("--verb {s:?}: bad hex color {h:?}"))?;
        let v = if h.len() == 6 { (v << 8) | 0xFF } else { v };
        Ok([
            ((v >> 24) & 0xFF) as f32 / 255.0,
            ((v >> 16) & 0xFF) as f32 / 255.0,
            ((v >> 8) & 0xFF) as f32 / 255.0,
            (v & 0xFF) as f32 / 255.0,
        ])
    };
    Ok(match t.first().copied().unwrap_or("") {
        "recolor-glyph" => Verb::RecolorGlyph(if t.len() > 1 { Some(hex(1)?) } else { None }),
        "recolor-line" => Verb::RecolorLine(if t.len() > 1 { Some(hex(1)?) } else { None }),
        "nudge-glyph" => Verb::NudgeGlyph([f(1)?, f(2)?, if t.len() > 3 { f(3)? } else { 0.0 }]),
        "scale-glyph" => Verb::ScaleGlyph(f(1)?),
        "move-group" => Verb::MoveGroup([f(1)?, f(2)?, f(3)?]),
        "scale-group" => Verb::ScaleGroup(f(1)?),
        "tint-group" => {
            let [r, g, b] = hex(1)?;
            Verb::TintGroup([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
        }
        "tint-cycle" => Verb::TintCycle,
        "hide-group" => Verb::SetHidden(true),
        "show-group" => Verb::SetHidden(false),
        "toggle-hidden" => Verb::ToggleHidden,
        "set-glyph-background" => Verb::SetGlyphBackground(hex_rgba(1)?),
        "set-glyph-transform" => {
            // Translation, identity rotation, uniform scale (default 1).
            let sc = if t.len() > 4 { f(4)? } else { 1.0 };
            Verb::SetGlyphTransform([f(1)?, f(2)?, f(3)?], [0.0, 0.0, 0.0, 1.0], [sc, sc, sc])
        }
        "reset-glyph-group" => Verb::ResetGlyphGroup,
        _ => return Err(usage),
    })
}

/// Re-interleave the op stream in true CLI order. clap stores each flag's
/// values separately; `indices_of` yields one argv index PER VALUE (verified
/// against clap_builder 4.6.6: `push_arg_values` pushes an index per value),
/// so for multi-value flags (--pick-px, --cam-pose) both values and indices
/// are chunked by the flag's arity and zipped occurrence-by-occurrence.
pub(crate) fn build_ops(matches: &clap::ArgMatches, raw: &RawOps) -> Vec<Op> {
    enum Keyed {
        PickFile(String),
        Row(u32),
        Col(u32),
        Px(f32, f32),
        CamPose([f32; 3], f32, f32),
        Verb(Verb),
        Highlight(PathBuf),
    }
    let indices = |id: &str| -> Vec<usize> {
        matches.indices_of(id).map(Iterator::collect).unwrap_or_default()
    };
    let mut keyed: Vec<(usize, Keyed)> = Vec::new();
    for (i, f) in indices("pick_file").into_iter().zip(raw.pick_file.iter()) {
        keyed.push((i, Keyed::PickFile(f.clone())));
    }
    for (i, r) in indices("pick_row").into_iter().zip(raw.pick_row.iter()) {
        keyed.push((i, Keyed::Row(*r)));
    }
    for (i, c) in indices("pick_col").into_iter().zip(raw.pick_col.iter()) {
        keyed.push((i, Keyed::Col(*c)));
    }
    for (ic, xy) in indices("pick_px").chunks(2).zip(raw.pick_px.as_chunks::<2>().0) {
        keyed.push((ic[0], Keyed::Px(xy[0], xy[1])));
    }
    for (ic, v) in indices("cam_pose").chunks(5).zip(raw.cam_pose.as_chunks::<5>().0) {
        // Degrees on the CLI, radians in the op stream (unchanged semantics).
        keyed.push((
            ic[0],
            Keyed::CamPose([v[0], v[1], v[2]], v[3].to_radians(), v[4].to_radians()),
        ));
    }
    for (i, v) in indices("verb").into_iter().zip(raw.verb.iter()) {
        keyed.push((i, Keyed::Verb(v.clone())));
    }
    for (i, p) in indices("highlight").into_iter().zip(raw.highlight.iter()) {
        keyed.push((i, Keyed::Highlight(p.clone())));
    }
    keyed.sort_by_key(|(i, _)| *i);

    let mut ops = Vec::new();
    for (_, k) in keyed {
        match k {
            Keyed::PickFile(f) => ops.push(Op::Pick(PickCommand::File(f))),
            Keyed::Row(r) => set_pick_row_col(&mut ops, Some(r), None),
            Keyed::Col(c) => set_pick_row_col(&mut ops, None, Some(c)),
            Keyed::Px(x, y) => ops.push(Op::Pick(PickCommand::Pixel { x, y })),
            Keyed::CamPose(p, yaw, pitch) => ops.push(Op::CamPose(p, yaw, pitch)),
            Keyed::Verb(v) => ops.push(Op::Verb(v)),
            Keyed::Highlight(p) => ops.push(Op::Highlight(p)),
        }
    }
    ops
}
