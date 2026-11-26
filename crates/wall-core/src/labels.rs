//! Port of `wall/labelPlacement.js`: greedy map-style label placement in
//! screen-space CSS pixels (**y down**, unlike [`crate::geom::Rect`]'s world
//! space).
//!
//! Each item supplies a target screen box and a label size; the result places
//! each label so no two labels overlap and every placed label stays inside the
//! viewport margin, adding a leader segment when the label is detached from its
//! target. A label first retries the candidate side it held on the previous
//! frame (hysteresis on the choice, never on the position, so labels track
//! their targets without lag and only flip sides when blocked). When every
//! strict slot would cover another terminal, a relaxed pass tries the detached
//! outer ring first before hiding the label.

use std::collections::HashMap;
use std::str::FromStr;

/// Candidate positions around a target box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Candidate {
    Above,
    Below,
    Right,
    Left,
    Above2,
    Below2,
}

impl Candidate {
    /// The wire/JS name of this candidate (`"above"`, `"below"`, ...).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Above => "above",
            Self::Below => "below",
            Self::Right => "right",
            Self::Left => "left",
            Self::Above2 => "above2",
            Self::Below2 => "below2",
        }
    }
}

impl FromStr for Candidate {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "above" => Ok(Self::Above),
            "below" => Ok(Self::Below),
            "right" => Ok(Self::Right),
            "left" => Ok(Self::Left),
            "above2" => Ok(Self::Above2),
            "below2" => Ok(Self::Below2),
            _ => Err(format!("[labelPlacement] unknown candidate {input}")),
        }
    }
}

/// Candidate positions in strict-pass preference order (`CANDIDATES` in `wall/labelPlacement.js`).
pub const CANDIDATES: [Candidate; 6] = [
    Candidate::Above,
    Candidate::Below,
    Candidate::Right,
    Candidate::Left,
    Candidate::Above2,
    Candidate::Below2,
];

/// Candidate positions in relaxed-pass order (`RELAXED_ORDER` in `wall/labelPlacement.js`).
///
/// When a label must sit over another terminal, trying the detached outer ring
/// first gives it a trace line to its own target rather than sitting snug
/// against a neighbour's edge.
pub const RELAXED_ORDER: [Candidate; 6] = [
    Candidate::Above2,
    Candidate::Below2,
    Candidate::Above,
    Candidate::Below,
    Candidate::Right,
    Candidate::Left,
];

/// Default touch slack in CSS pixels for [`leader_for`] when called standalone.
pub const DEFAULT_LEADER_SLACK: f64 = 1.5;

/// Screen-space axis-aligned box in CSS pixels, y down (`top <= bottom`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenBox {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl ScreenBox {
    /// Construct from explicit edges (`left`, `top`, `right`, `bottom`).
    pub const fn new(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self { left, top, right, bottom }
    }

    /// Construct from top-left corner and size (`width`, `height`), y down.
    pub fn from_top_left(left: f64, top: f64, width: f64, height: f64) -> Self {
        Self {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }
}

/// One label request, ordered highest priority first when passed to [`place_labels`].
#[derive(Clone, Debug, PartialEq)]
pub struct LabelItem {
    pub id: String,
    pub target: ScreenBox,
    pub w: f64,
    pub h: f64,
}

/// A trace line from the label's edge `(x1, y1)` to its target's border `(x2, y2)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leader {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

/// Placed label geometry and chosen candidate side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub label_box: ScreenBox,
    pub cand: Candidate,
    pub leader: Option<Leader>,
}

/// Viewport dimensions and spacing parameters for [`place_labels`], in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelView {
    pub w: f64,
    pub h: f64,
    pub margin: f64,
    pub gap: f64,
}

impl LabelView {
    pub const DEFAULT_MARGIN: f64 = 4.0;
    pub const DEFAULT_GAP: f64 = 4.0;

    /// Viewport of `w` × `h` CSS pixels with the default 4 px margin and 4 px gap.
    pub const fn new(w: f64, h: f64) -> Self {
        Self {
            w,
            h,
            margin: Self::DEFAULT_MARGIN,
            gap: Self::DEFAULT_GAP,
        }
    }
}

/// Whether two screen-space boxes share positive area (touching edges do not overlap).
pub fn boxes_overlap(a: ScreenBox, b: ScreenBox) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

/// Compute the label box for one candidate position around `target`.
pub fn candidate_box(
    cand: Candidate,
    target: ScreenBox,
    width: f64,
    height: f64,
    gap: f64,
) -> ScreenBox {
    let (x, y) = match cand {
        Candidate::Above => (target.left, target.top - gap - height),
        Candidate::Below => (target.left, target.bottom + gap),
        Candidate::Right => (target.right + gap, target.top),
        Candidate::Left => (target.left - gap - width, target.top),
        Candidate::Above2 => (target.left, target.top - 2.0 * gap - 2.0 * height),
        Candidate::Below2 => (target.left, target.bottom + 2.0 * gap + height),
    };
    ScreenBox {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    }
}

/// Compute the leader segment from `label_box` to `target`, or `None` when they
/// touch within `slack` CSS pixels. Both endpoints are nearest points on their
/// respective borders.
pub fn leader_for(label_box: ScreenBox, target: ScreenBox, slack: f64) -> Option<Leader> {
    let center_x = (label_box.left + label_box.right) / 2.0;
    let center_y = (label_box.top + label_box.bottom) / 2.0;

    // Nearest point of the target to the label's centre, pushed onto its border.
    let mut target_x = center_x.max(target.left).min(target.right);
    let mut target_y = center_y.max(target.top).min(target.bottom);
    if target_x > target.left
        && target_x < target.right
        && target_y > target.top
        && target_y < target.bottom
    {
        let distances = [
            target_y - target.top,
            target.bottom - target_y,
            target_x - target.left,
            target.right - target_x,
        ];
        let min_distance = distances
            .into_iter()
            .fold(f64::INFINITY, f64::min);
        if min_distance == distances[0] {
            target_y = target.top;
        } else if min_distance == distances[1] {
            target_y = target.bottom;
        } else if min_distance == distances[2] {
            target_x = target.left;
        } else {
            target_x = target.right;
        }
    }

    // Nearest point of the label box to that border point.
    let label_x = target_x.max(label_box.left).min(label_box.right);
    let label_y = target_y.max(label_box.top).min(label_box.bottom);
    if (target_x - label_x).hypot(target_y - label_y) <= slack {
        return None;
    }
    Some(Leader {
        x1: label_x,
        y1: label_y,
        x2: target_x,
        y2: target_y,
    })
}

/// Place labels greedily in `items`' priority order.
///
/// When `obstacles` is `None`, every item's own `target` acts as an obstacle
/// for other items' labels during the strict pass (matching the JS default
/// `obstacles = items.map((i) => i.target)`). An entry mapped to `None` in the
/// returned map is hidden because no candidate fit inside `view` without
/// colliding with an already-placed label.
pub fn place_labels(
    items: &[LabelItem],
    view: LabelView,
    prev: &HashMap<String, Candidate>,
    obstacles: Option<&[ScreenBox]>,
) -> HashMap<String, Option<Placement>> {
    let default_obstacles: Vec<ScreenBox>;
    let obstacle_boxes: &[ScreenBox] = match obstacles {
        Some(explicit) => explicit,
        None => {
            default_obstacles = items.iter().map(|item| item.target).collect();
            &default_obstacles
        }
    };

    let in_view = |b: ScreenBox| {
        b.left >= view.margin
            && b.top >= view.margin
            && b.right <= view.w - view.margin
            && b.bottom <= view.h - view.margin
    };

    let mut placed: Vec<ScreenBox> = Vec::new();
    let mut out: HashMap<String, Option<Placement>> = HashMap::with_capacity(items.len());

    for item in items {
        let mut strict_order = CANDIDATES;
        if let Some(&held) = prev.get(&item.id) {
            strict_order[0] = held;
            let mut slot = 1;
            for cand in CANDIDATES {
                if cand != held {
                    strict_order[slot] = cand;
                    slot += 1;
                }
            }
        }

        let mut hit: Option<Placement> = None;
        for strict in [true, false] {
            let order = if strict { &strict_order } else { &RELAXED_ORDER };
            for &cand in order {
                let candidate = candidate_box(cand, item.target, item.w, item.h, view.gap);
                if !in_view(candidate) {
                    continue;
                }
                if placed.iter().any(|&existing| boxes_overlap(candidate, existing)) {
                    continue;
                }
                if strict
                    && obstacle_boxes
                        .iter()
                        .any(|&obs| obs != item.target && boxes_overlap(candidate, obs))
                {
                    continue;
                }
                // Adjacent (one gap away) needs no trace line.
                let leader = leader_for(candidate, item.target, view.gap + DEFAULT_LEADER_SLACK);
                hit = Some(Placement {
                    label_box: candidate,
                    cand,
                    leader,
                });
                break;
            }
            if hit.is_some() {
                break;
            }
        }

        if let Some(placed_hit) = hit {
            placed.push(placed_hit.label_box);
        }
        out.insert(item.id.clone(), hit);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sbox(left: f64, top: f64, width: f64, height: f64) -> ScreenBox {
        ScreenBox::from_top_left(left, top, width, height)
    }

    const VIEW: LabelView = LabelView {
        w: 1000.0,
        h: 800.0,
        margin: 4.0,
        gap: 4.0,
    };

    #[test]
    fn lone_label_sits_above_its_target_touching_with_no_leader() {
        let target = sbox(100.0, 100.0, 50.0, 30.0);
        let items = vec![LabelItem {
            id: "a".to_string(),
            target,
            w: 80.0,
            h: 16.0,
        }];
        let placement = place_labels(&items, VIEW, &HashMap::new(), None)["a"]
            .expect("lone label must place");
        assert_eq!(placement.cand, Candidate::Above);
        assert_eq!(
            placement.label_box,
            candidate_box(Candidate::Above, target, 80.0, 16.0, 4.0)
        );
        assert_eq!(placement.leader, None);
    }

    #[test]
    fn crowded_targets_never_overlap_and_first_keeps_preferred_side() {
        let items: Vec<LabelItem> = (0..8)
            .map(|i| LabelItem {
                id: format!("t{i}"),
                target: sbox(200.0 + f64::from(i) * 30.0, 300.0, 20.0, 14.0),
                w: 70.0,
                h: 16.0,
            })
            .collect();
        let out = place_labels(&items, VIEW, &HashMap::new(), None);
        assert_eq!(out["t0"].expect("t0 placed").cand, Candidate::Above);

        let boxes: Vec<ScreenBox> = out.values().flatten().map(|p| p.label_box).collect();
        for i in 0..boxes.len() {
            for j in (i + 1)..boxes.len() {
                assert!(!boxes_overlap(boxes[i], boxes[j]));
            }
        }
        for b in boxes {
            assert!(b.left >= 4.0 && b.right <= 996.0 && b.top >= 4.0 && b.bottom <= 796.0);
        }
    }

    #[test]
    fn choice_is_sticky_while_previous_side_stays_free() {
        let target = sbox(100.0, 100.0, 50.0, 30.0);
        let items = vec![LabelItem {
            id: "a".to_string(),
            target,
            w: 80.0,
            h: 16.0,
        }];
        let prev = HashMap::from([("a".to_string(), Candidate::Right)]);
        let placement = place_labels(&items, VIEW, &prev, None)["a"].expect("placed on right");
        assert_eq!(placement.cand, Candidate::Right);
    }

    #[test]
    fn strict_pass_avoids_other_targets_and_relaxed_allows_before_hiding() {
        let a = sbox(100.0, 100.0, 50.0, 30.0);
        let b = sbox(90.0, 60.0, 200.0, 36.0);
        let items = vec![LabelItem {
            id: "a".to_string(),
            target: a,
            w: 80.0,
            h: 16.0,
        }];
        let out = place_labels(&items, VIEW, &HashMap::new(), Some(&[a, b]));
        assert!(!boxes_overlap(out["a"].expect("placed").label_box, b));

        let tiny = LabelView {
            w: 60.0,
            h: 40.0,
            margin: 4.0,
            gap: 4.0,
        };
        let too_wide = vec![LabelItem {
            id: "x".to_string(),
            target: sbox(10.0, 10.0, 40.0, 20.0),
            w: 200.0,
            h: 16.0,
        }];
        let hidden = place_labels(&too_wide, tiny, &HashMap::new(), None);
        assert_eq!(hidden["x"], None);
    }

    #[test]
    fn relaxed_fallback_prefers_detached_ring_with_trace_line() {
        let mut targets = Vec::new();
        for r in 0..3 {
            for c in 0..3 {
                targets.push(sbox(
                    300.0 + f64::from(c) * 70.0,
                    300.0 + f64::from(r) * 50.0,
                    60.0,
                    40.0,
                ));
            }
        }
        let mid = targets[4];
        let items = vec![LabelItem {
            id: "mid".to_string(),
            target: mid,
            w: 90.0,
            h: 16.0,
        }];
        let out = place_labels(&items, VIEW, &HashMap::new(), Some(&targets));
        let placement = out["mid"].expect("mid placed in relaxed ring");
        assert!(matches!(placement.cand, Candidate::Above2 | Candidate::Below2));
        assert!(placement.leader.is_some());
    }

    #[test]
    fn leader_runs_from_label_edge_to_nearest_point_on_target_border() {
        let target = sbox(100.0, 100.0, 50.0, 30.0);
        let far = ScreenBox::new(300.0, 110.0, 360.0, 126.0);
        let leader = leader_for(far, target, DEFAULT_LEADER_SLACK);
        assert_eq!(
            leader,
            Some(Leader {
                x1: 300.0,
                y1: 118.0,
                x2: 150.0,
                y2: 118.0,
            })
        );
        let touching = candidate_box(Candidate::Above, target, 80.0, 16.0, 0.0);
        assert_eq!(leader_for(touching, target, DEFAULT_LEADER_SLACK), None);
    }

    #[test]
    fn every_candidate_is_reachable_and_known() {
        for cand in CANDIDATES {
            let parsed: Candidate = cand.as_str().parse().expect("known candidate parses");
            assert_eq!(parsed, cand);
            let _ = candidate_box(parsed, sbox(0.0, 0.0, 1.0, 1.0), 1.0, 1.0, 1.0);
        }
        let err = "nope".parse::<Candidate>().unwrap_err();
        assert_eq!(err, "[labelPlacement] unknown candidate nope");
    }
}
