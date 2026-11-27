//! World-space 2D chrome shapes drawn by the wall's instanced shape pass
//! (`native/src/shaders/wall_shapes.wgsl`).
//!
//! Primitives only — the app and `wall-core` decide what is selected, where a
//! group frame sits, or where a map label's leader runs.

use bytemuck::{Pod, Zeroable};
use glam::Vec2;

use super::metrics::WorldRect;

/// GPU primitive kind tags matching `native/src/shaders/wall_shapes.wgsl`.
pub const SHAPE_KIND_RECT: u32 = 0;
pub const SHAPE_KIND_LINE: u32 = 1;
pub const SHAPE_KIND_DOT: u32 = 2;
pub const SHAPE_KIND_TRI: u32 = 3;

/// Screen-pixel dash pattern for stroked shapes (`<= 0.0` means solid).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DashStyle {
    /// Length of each drawn dash in physical screen pixels.
    pub dash_px: f32,
    /// Length of each gap between dashes in physical screen pixels.
    pub gap_px: f32,
}

impl DashStyle {
    /// Continuous unbroken stroke.
    pub const SOLID: Self = Self {
        dash_px: 0.0,
        gap_px: 0.0,
    };

    /// Dashed stroke with `dash_px` on and `gap_px` off (physical pixels).
    pub const fn new(dash_px: f32, gap_px: f32) -> Self {
        Self { dash_px, gap_px }
    }

    /// True when both dash and gap lengths are positive.
    pub fn is_dashed(self) -> bool {
        self.dash_px > 0.0 && self.gap_px > 0.0
    }
}

/// World-space axis-aligned rectangle (filled, outlined in screen pixels, optionally dashed/rounded).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectShape {
    pub rect: WorldRect,
    pub z: f32,
    /// Interior fill colour (sRGB RGBA; `None` = hollow).
    pub fill: Option<[f32; 4]>,
    /// Border colour (sRGB RGBA; `None` = no border).
    pub stroke: Option<[f32; 4]>,
    /// Border width in physical screen pixels.
    pub stroke_px: f32,
    /// Solid or dashed border pattern in screen pixels.
    pub dash: DashStyle,
    /// Corner radius in physical screen pixels (`0.0` = sharp corners).
    pub corner_radius_px: f32,
}

/// World-space line segment (solid or dashed, screen-pixel thickness).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineShape {
    pub from: Vec2,
    pub to: Vec2,
    pub z: f32,
    pub color: [f32; 4],
    pub width_px: f32,
    pub dash: DashStyle,
}

/// Closed world-space polygon outline (e.g. selection lasso) with optional translucent fill.
#[derive(Clone, Debug, PartialEq)]
pub struct PolygonShape {
    pub points: Vec<Vec2>,
    pub z: f32,
    /// Optional interior fill (sRGB RGBA, triangulated via ear clipping).
    pub fill: Option<[f32; 4]>,
    /// Perimeter stroke colour (sRGB RGBA).
    pub stroke: [f32; 4],
    /// Perimeter stroke width in physical screen pixels.
    pub stroke_px: f32,
    /// Solid or dashed outline pattern in screen pixels (continuous across vertices).
    pub dash: DashStyle,
}

/// World-space leader segment from `from` to `to` with a circular dot anchored at `to`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeaderShape {
    pub from: Vec2,
    pub to: Vec2,
    pub z: f32,
    pub color: [f32; 4],
    pub width_px: f32,
    pub dot_radius_px: f32,
    pub dash: DashStyle,
}

/// World-space circular dot at `center` with a screen-pixel radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DotShape {
    pub center: Vec2,
    pub z: f32,
    pub color: [f32; 4],
    pub radius_px: f32,
}

/// One vector shape drawn in world space at a world `z` by [`super::WallRenderer`].
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Rect(RectShape),
    Line(LineShape),
    Polygon(PolygonShape),
    Leader(LeaderShape),
    Dot(DotShape),
}

impl Shape {
    /// World depth of this shape (larger `z` is nearer and covers smaller `z`).
    pub fn z(&self) -> f32 {
        match self {
            Self::Rect(r) => r.z,
            Self::Line(l) => l.z,
            Self::Polygon(p) => p.z,
            Self::Leader(l) => l.z,
            Self::Dot(d) => d.z,
        }
    }
}

/// Per-frame uniform for `native/src/shaders/wall_shapes.wgsl` (80 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct ShapeFrameUniform {
    pub view_proj: [[f32; 4]; 4],
    pub viewport: [f32; 2],
    pub target_is_srgb: u32,
    pub _pad0: u32,
}

/// One instanced GPU shape record read by `native/src/shaders/wall_shapes.wgsl` (80 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct ShapeGpu {
    pub geom: [f32; 4],
    pub extra: [f32; 4],
    pub fill_color: [f32; 4],
    pub stroke_color: [f32; 4],
    pub z: f32,
    pub kind: u32,
    pub _pad0: u32,
    pub _pad1: u32,
}

/// Expand `shape` into GPU instances, calling `emit(opaque, gpu)` for each.
///
/// `zoom` (physical screen pixels per world unit) is used only to keep polygon
/// perimeter dash offsets continuous across vertices.
pub fn expand_shape(shape: &Shape, zoom: f32, mut emit: impl FnMut(bool, ShapeGpu)) {
    match shape {
        Shape::Rect(r) => {
            if r.rect.width() <= 0.0 || r.rect.height() <= 0.0 {
                return;
            }
            let fill = r.fill.unwrap_or([0.0; 4]);
            let stroke = r.stroke.unwrap_or([0.0; 4]);
            let has_fill = fill[3] > 0.0;
            let has_stroke = stroke[3] > 0.0 && r.stroke_px > 0.0;
            if !has_fill && !has_stroke {
                return;
            }
            let opaque = r.corner_radius_px <= 0.0
                && fill[3] >= 1.0
                && (!has_stroke || stroke[3] >= 1.0);
            let (dash_px, gap_px) = if r.dash.is_dashed() {
                (r.dash.dash_px, r.dash.gap_px)
            } else {
                (0.0, 0.0)
            };
            emit(
                opaque,
                ShapeGpu {
                    geom: [r.rect.min.x, r.rect.min.y, r.rect.max.x, r.rect.max.y],
                    extra: [
                        if has_stroke { r.stroke_px } else { 0.0 },
                        dash_px,
                        gap_px,
                        r.corner_radius_px.max(0.0),
                    ],
                    fill_color: fill,
                    stroke_color: stroke,
                    z: r.z,
                    kind: SHAPE_KIND_RECT,
                    _pad0: 0,
                    _pad1: 0,
                },
            );
        }
        Shape::Line(l) => {
            if l.width_px <= 0.0 || l.color[3] <= 0.0 {
                return;
            }
            let (dash_px, gap_px) = if l.dash.is_dashed() {
                (l.dash.dash_px, l.dash.gap_px)
            } else {
                (0.0, 0.0)
            };
            emit(
                false,
                ShapeGpu {
                    geom: [l.from.x, l.from.y, l.to.x, l.to.y],
                    extra: [l.width_px, dash_px, gap_px, 0.0],
                    fill_color: [0.0; 4],
                    stroke_color: l.color,
                    z: l.z,
                    kind: SHAPE_KIND_LINE,
                    _pad0: 0,
                    _pad1: 0,
                },
            );
        }
        Shape::Dot(d) => {
            if d.radius_px <= 0.0 || d.color[3] <= 0.0 {
                return;
            }
            emit(
                false,
                ShapeGpu {
                    geom: [d.center.x, d.center.y, d.radius_px, 0.0],
                    extra: [0.0; 4],
                    fill_color: d.color,
                    stroke_color: [0.0; 4],
                    z: d.z,
                    kind: SHAPE_KIND_DOT,
                    _pad0: 0,
                    _pad1: 0,
                },
            );
        }
        Shape::Leader(l) => {
            if l.color[3] <= 0.0 {
                return;
            }
            if l.width_px > 0.0 {
                let (dash_px, gap_px) = if l.dash.is_dashed() {
                    (l.dash.dash_px, l.dash.gap_px)
                } else {
                    (0.0, 0.0)
                };
                emit(
                    false,
                    ShapeGpu {
                        geom: [l.from.x, l.from.y, l.to.x, l.to.y],
                        extra: [l.width_px, dash_px, gap_px, 0.0],
                        fill_color: [0.0; 4],
                        stroke_color: l.color,
                        z: l.z,
                        kind: SHAPE_KIND_LINE,
                        _pad0: 0,
                        _pad1: 0,
                    },
                );
            }
            if l.dot_radius_px > 0.0 {
                emit(
                    false,
                    ShapeGpu {
                        geom: [l.to.x, l.to.y, l.dot_radius_px, 0.0],
                        extra: [0.0; 4],
                        fill_color: l.color,
                        stroke_color: [0.0; 4],
                        z: l.z,
                        kind: SHAPE_KIND_DOT,
                        _pad0: 0,
                        _pad1: 0,
                    },
                );
            }
        }
        Shape::Polygon(p) => {
            if p.points.len() < 2 {
                return;
            }
            if let Some(fill) = p.fill {
                if fill[3] > 0.0 && p.points.len() >= 3 {
                    for [a, b, c] in triangulate_polygon(&p.points) {
                        emit(
                            false,
                            ShapeGpu {
                                geom: [a.x, a.y, b.x, b.y],
                                extra: [c.x, c.y, 0.0, 0.0],
                                fill_color: fill,
                                stroke_color: [0.0; 4],
                                z: p.z,
                                kind: SHAPE_KIND_TRI,
                                _pad0: 0,
                                _pad1: 0,
                            },
                        );
                    }
                }
            }
            if p.stroke_px > 0.0 && p.stroke[3] > 0.0 {
                let (dash_px, gap_px) = if p.dash.is_dashed() {
                    (p.dash.dash_px, p.dash.gap_px)
                } else {
                    (0.0, 0.0)
                };
                let safe_zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
                let mut offset_px = 0.0_f32;
                let n = p.points.len();
                for i in 0..n {
                    let a = p.points[i];
                    let b = p.points[(i + 1) % n];
                    let seg_px = (b - a).length() * safe_zoom;
                    if seg_px < 1e-4 {
                        continue;
                    }
                    emit(
                        false,
                        ShapeGpu {
                            geom: [a.x, a.y, b.x, b.y],
                            extra: [p.stroke_px, dash_px, gap_px, offset_px],
                            fill_color: [0.0; 4],
                            stroke_color: p.stroke,
                            z: p.z,
                            kind: SHAPE_KIND_LINE,
                            _pad0: 0,
                            _pad1: 0,
                        },
                    );
                    offset_px += seg_px;
                }
            }
        }
    }
}

fn cross2(a: Vec2, b: Vec2) -> f32 {
    a.x * b.y - a.y * b.x
}

fn signed_area2(pts: &[Vec2]) -> f32 {
    let mut sum = 0.0_f32;
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        sum += a.x * b.y - b.x * a.y;
    }
    sum
}

fn point_in_triangle_ccw(p: Vec2, a: Vec2, b: Vec2, c: Vec2) -> bool {
    let eps = -1e-6;
    cross2(b - a, p - a) >= eps && cross2(c - b, p - b) >= eps && cross2(a - c, p - c) >= eps
}

/// Triangulate a simple 2D polygon into counter-clockwise triangles via ear clipping.
///
/// Returns an empty list for polygons with fewer than 3 distinct vertices or
/// near-zero area. Self-intersecting polygons exit cleanly without looping.
pub fn triangulate_polygon(points: &[Vec2]) -> Vec<[Vec2; 3]> {
    let mut clean: Vec<Vec2> = Vec::with_capacity(points.len());
    for &pt in points {
        if clean
            .last()
            .is_none_or(|&prev| (pt - prev).length_squared() > 1e-12)
        {
            clean.push(pt);
        }
    }
    while clean.len() >= 2 && (clean[0] - *clean.last().expect("non-empty")).length_squared() <= 1e-12
    {
        clean.pop();
    }
    if clean.len() < 3 {
        return Vec::new();
    }

    let area2 = signed_area2(&clean);
    if area2.abs() < 1e-9 {
        return Vec::new();
    }
    if area2 < 0.0 {
        clean.reverse();
    }

    let mut idx: Vec<usize> = (0..clean.len()).collect();
    let mut out = Vec::with_capacity(clean.len().saturating_sub(2));
    let mut guard = clean.len() * clean.len() + 1;

    while idx.len() > 3 && guard > 0 {
        guard -= 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let prev = idx[(i + m - 1) % m];
            let curr = idx[i];
            let next = idx[(i + 1) % m];
            let (a, b, c) = (clean[prev], clean[curr], clean[next]);
            if cross2(b - a, c - b) <= 1e-9 {
                continue;
            }
            let contains_other = idx.iter().any(|&k| {
                k != prev && k != curr && k != next && point_in_triangle_ccw(clean[k], a, b, c)
            });
            if !contains_other {
                out.push([a, b, c]);
                idx.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped {
            break;
        }
    }
    if idx.len() == 3 {
        out.push([clean[idx[0]], clean[idx[1]], clean[idx[2]]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_struct_sizes_match_wgsl() {
        assert_eq!(std::mem::size_of::<ShapeFrameUniform>(), 80);
        assert_eq!(std::mem::size_of::<ShapeGpu>(), 80);
    }

    #[test]
    fn triangulate_handles_convex_concave_cw_and_degenerate_polygons() {
        // Concave L-shape (clockwise input): area = 4*4 - 2*2 = 12.
        let l_cw = [
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 4.0),
            Vec2::new(2.0, 4.0),
            Vec2::new(2.0, 2.0),
            Vec2::new(4.0, 2.0),
            Vec2::new(4.0, 0.0),
        ];
        let tris = triangulate_polygon(&l_cw);
        assert_eq!(tris.len(), 4);
        let tri_area: f32 = tris
            .iter()
            .map(|&[a, b, c]| 0.5 * cross2(b - a, c - a).abs())
            .sum();
        assert!((tri_area - 12.0).abs() < 1e-4, "area={tri_area}");

        // Collinear / too short -> empty without panic.
        assert!(triangulate_polygon(&[Vec2::ZERO, Vec2::X]).is_empty());
        assert!(triangulate_polygon(&[Vec2::ZERO, Vec2::X, Vec2::new(2.0, 0.0)]).is_empty());
    }

    #[test]
    fn expand_polygon_accumulates_continuous_dash_offsets() {
        let poly = Shape::Polygon(PolygonShape {
            points: vec![
                Vec2::new(0.0, 0.0),
                Vec2::new(10.0, 0.0),
                Vec2::new(10.0, 5.0),
                Vec2::new(0.0, 5.0),
            ],
            z: 3.0,
            fill: Some([0.5, 0.7, 1.0, 0.08]),
            stroke: [0.5, 0.7, 1.0, 0.85],
            stroke_px: 1.5,
            dash: DashStyle::new(6.0, 4.0),
        });
        let mut tris = 0usize;
        let mut offsets = Vec::new();
        expand_shape(&poly, 10.0, |opaque, g| {
            assert!(!opaque);
            if g.kind == SHAPE_KIND_TRI {
                tris += 1;
            } else if g.kind == SHAPE_KIND_LINE {
                offsets.push(g.extra[3]);
            }
        });
        assert_eq!(tris, 2);
        assert_eq!(offsets, vec![0.0, 100.0, 150.0, 250.0]);
    }

    #[test]
    fn expand_leader_emits_segment_and_dot() {
        let leader = Shape::Leader(LeaderShape {
            from: Vec2::new(0.0, 0.0),
            to: Vec2::new(5.0, 3.0),
            z: 2.0,
            color: [0.6, 0.8, 1.0, 0.9],
            width_px: 1.0,
            dot_radius_px: 2.5,
            dash: DashStyle::SOLID,
        });
        let mut kinds = Vec::new();
        expand_shape(&leader, 20.0, |opaque, g| {
            assert!(!opaque);
            kinds.push(g.kind);
        });
        assert_eq!(kinds, vec![SHAPE_KIND_LINE, SHAPE_KIND_DOT]);
    }
}
