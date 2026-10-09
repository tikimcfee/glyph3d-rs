//! Spatial shelf packing layout and directory/extension tints.

use crate::glyph_scene::GroupRow;
use super::{FileView, RepoParams};

/// Per-directory tints (multiplied with the syntax colors through the group
/// color, colorBlend 0): `[repo] dir_tints`. Stage G's `tint-cycle` verb
/// walks this same palette. Never empty (`config` refuses that).
pub fn dir_tints() -> &'static [[f32; 3]] {
    &crate::config::settings().repo.dir_tints
}

pub(crate) fn dir_tint(dir: &str) -> [f32; 3] {
    // FNV-1a 32-bit over the directory path.
    let mut h: u32 = 0x811C9DC5;
    for b in dir.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    let tints = dir_tints();
    tints[(h as usize) % tints.len()]
}

/// File-extension LOD backdrop tint (in linear sRGB space):
/// `[[repo.extension_tints]]`, first entry listing the extension wins, else
/// `[repo] extension_tint_fallback`. Per file, not per glyph — a linear scan
/// of a dozen entries.
pub fn extension_tint(path: &str) -> [f32; 3] {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let repo = &crate::config::settings().repo;
    repo.extension_tints
        .iter()
        .find(|e| e.extensions.iter().any(|x| x == ext))
        .map_or(repo.extension_tint_fallback, |e| e.tint)
}

/// Packed SHELF layout, classed by height: files are stably partitioned into
/// height classes (small / medium / large), each class shelf-packed
/// left-to-right in path order (consecutive files — same directory — stay
/// adjacent within a class), classes stacked top to bottom. Shelf packing
/// alone pays each shelf its tallest member's height; with a real repo's
/// skewed heights (1-page files next to 400-page lock files) that wastes
/// 2-3× the field's height. Classing keeps shelf-mates similar in height,
/// so the field's aspect actually approaches `grid_aspect`.
pub(crate) fn layout_shelf(
    views: &mut [FileView],
    params: &RepoParams,
) -> (Vec<GroupRow>, [f32; 3], [f32; 3]) {
    let page_h = params.page_rows as f32 * params.line_height as f32;
    // Class bounds in world units: ≤2 pages tall, ≤16 pages tall, monsters.
    let class_of = |h: f32| -> usize {
        if h <= 2.5 * page_h {
            0
        } else if h <= 16.5 * page_h {
            1
        } else {
            2
        }
    };
    let mut order: Vec<usize> = (0..views.len()).collect();
    order.sort_by_key(|&i| class_of(views[i].height)); // stable: path order kept within a class

    let area: f32 = views
        .iter()
        .map(|v| (v.width + params.gap_x) * (v.height + params.gap_y))
        .sum();
    let target_w = (area * params.grid_aspect).sqrt().max(1.0);

    let mut offsets: Vec<[f32; 3]> = vec![[0.0; 3]; views.len()];
    let mut x = 0f32; // pen: left edge of the next page on this shelf
    let mut shelf_top = 0f32; // y of the current shelf's top edge
    let mut shelf_h = 0f32; // tallest page on the current shelf
    let mut max_x = 0f32;
    let mut prev_class = 0usize;
    for &i in &order {
        let v = &views[i];
        let cls = class_of(v.height);
        // Class boundary or full shelf → close the shelf, drop down.
        if (cls != prev_class) || (x > 0.0 && x + v.width > target_w) {
            shelf_top -= shelf_h + params.gap_y;
            x = 0.0;
            shelf_h = 0.0;
            prev_class = cls;
        }
        offsets[i] = [x, shelf_top, 0.0];
        x += v.width + params.gap_x;
        max_x = max_x.max(x - params.gap_x);
        shelf_h = shelf_h.max(v.height);
    }
    let min_y = shelf_top - shelf_h;

    let mut groups = Vec::with_capacity(views.len());
    // Scene depth is the union of the placed files' own depth, not a constant.
    // Seeded at 0 so a wholly planar field still reports a zero-thickness slab
    // rather than an inverted one.
    let (mut min_z, mut max_z) = (0.0f32, 0.0f32);
    for (v, off) in views.iter_mut().zip(offsets.iter()) {
        v.offset = *off;
        min_z = min_z.min(v.offset[2] + v.z_min);
        max_z = max_z.max(v.offset[2] + v.z_max);
        
        groups.push(GroupRow::tinted(v.offset, dir_tint(&v.dir)));
    }
    log::info!(
        "layout [shelf]: target_w {:.0}, field {:.0}x{:.0}",
        target_w,
        max_x.max(1.0),
        -min_y,
    );
    (
        groups,
        [0.0, min_y, min_z],
        [max_x.max(1.0), params.line_height as f32, max_z],
    )
}
