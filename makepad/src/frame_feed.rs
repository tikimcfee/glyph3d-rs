//! The engine's frame as the makepad passes take it: each `FrameData` the
//! scene prepares (`SceneLike::prepare_frame`, the CPU half of a frame)
//! becomes one `G3FrameInputs` for `G3Renderer::draw_frame`. Most of it is
//! a borrow; what this keeps across frames is what the renderer uploads
//! only on change:
//!
//! - the float tables (`glyph3d_native::float_tables`): the atlas's
//!   glyph map, curves and advances and the emoji sheet once per process;
//!   the item table per scene; the group overrides when they change;
//! - the group table as float texels, written run by run as the frame
//!   reports changed rows, and the dirty rects since the renderer last
//!   consumed a generation (a frame's runs are handed over once, so they
//!   accumulate here until the renderer has seen them);
//! - the field's host blocks, viewed as the `f32`-carried words makepad
//!   publishes: the same allocation (`Arc` identity is the block's
//!   generation), never a copy.

use std::sync::Arc;

use glyph3d_native::atlas::Atlas;
use glyph3d_native::float_tables::{self, FloatTexture, TexelRect};
use glyph3d_native::glyph_scene::{FrameData, RegionParams};

use crate::g3_passes::{G3AtlasTables, G3EmojiPayload, G3EmojiSheet, G3FrameInputs, G3GroupTable, G3Renderer, G3Selection, G3StaticTables};

/// Every scene a process builds stages against the same immutable atlas
/// (`assets/atlas/` + the `--emoji-sheet` path), so `G3AtlasTables` is built
/// and handed to `G3Renderer` once (`ATLAS_GENERATION = 1`), keeping neither
/// `FloatTexture` copies nor the emoji payload buffer in `FrameFeed`.
const ATLAS_GENERATION: u64 = 1;

fn emoji_sheet_of(atlas: &Atlas, use_bc7: bool) -> G3EmojiSheet {
    let payload = if use_bc7 {
        let mut bc7_len = 0usize;
        atlas.emoji.for_each_level(|_, _, w, h, _| {
            bc7_len += makepad_texcomp::bc7::encoded_len(w, h);
        });
        let mut bc7 = Vec::with_capacity(bc7_len);
        let t0 = std::time::Instant::now();
        atlas.emoji.for_each_level(|_, _, w, h, rgba| {
            makepad_texcomp::bc7::encode_image_into(rgba, w, h, 0, &mut bc7)
                .expect("emoji sheet level dimensions match RGBA8 slice");
        });
        makepad_widgets::log!(
            "g3: encoded emoji sheet to BC7 ({} layers, {}x{}, {} mips): {:.2} MiB -> {:.2} MiB in {:.1} ms",
            atlas.emoji.sheet.layers,
            atlas.emoji.sheet.layer_w,
            atlas.emoji.sheet.layer_h,
            atlas.emoji.mip_levels,
            atlas.emoji.byte_len() as f64 / (1024.0 * 1024.0),
            bc7.len() as f64 / (1024.0 * 1024.0),
            t0.elapsed().as_secs_f64() * 1000.0,
        );
        G3EmojiPayload::Bc7(bc7)
    } else {
        let byte_len = atlas.emoji.byte_len();
        assert_eq!(byte_len % 4, 0, "emoji sheet byte_len is 4-byte RGBA8 texels");
        let mut bgra = Vec::with_capacity(byte_len / 4);
        atlas.emoji.for_each_level(|_, _, _, _, rgba| {
            bgra.extend(
                rgba.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&[r, g, b, a]| (b as u32) | ((g as u32) << 8) | ((r as u32) << 16) | ((a as u32) << 24)),
            );
        });
        G3EmojiPayload::Bgra(bgra)
    };
    G3EmojiSheet {
        width: atlas.emoji.sheet.layer_w as usize,
        height: atlas.emoji.sheet.layer_h as usize,
        layers: atlas.emoji.sheet.layers as usize,
        max_level: atlas.emoji.mip_levels as usize - 1,
        payload,
    }
}

/// The tables of the scene being drawn.
struct SceneTables {
    /// `EngineHost::scene_generation` they were built for.
    scene: u64,
    items: FloatTexture,
    overrides: FloatTexture,
    /// The override words `overrides` was packed from.
    override_words: Vec<u32>,
    groups: FloatTexture,
    /// Group rows the texture holds.
    group_count: u32,
    /// Rects written since the renderer last consumed the group generation.
    dirty: Vec<TexelRect>,
    /// Bumped with every change of the tables or the groups above; folded
    /// with the scene's into the renderer's generations.
    tables_version: u64,
    groups_version: u64,
}

#[derive(Default)]
pub struct FrameFeed {
    scene: Option<SceneTables>,
    /// The blocks as makepad takes them, refreshed per frame (a refcount
    /// each), kept so the inputs can borrow them.
    blocks: Vec<Arc<[f32]>>,
    /// Which blocks the last frame's selection drew into, logged when it
    /// changes (a selection across a block edge draws into two).
    selection_blocks: Vec<usize>,
    /// The last frame's mesh and region counts and picked region, logged
    /// when they change.
    shape: (usize, usize, usize, usize, Option<usize>),
    /// `GLYPH_FRAME_TRACE=1`: log every frame's changed group, face and
    /// region runs (a drag or a page turn moves them each frame).
    trace: Option<bool>,
}

/// A block's `u32` words as the `f32`-carried words makepad's instance
/// publications take, without a copy: the same allocation, so its `Arc`
/// identity (the renderer's generation for it) is the block's own.
fn words_as_f32(block: &Arc<[u32]>) -> Arc<[f32]> {
    let raw = Arc::into_raw(block.clone()) as *const [f32];
    // SAFETY: `u32` and `f32` have the same size and alignment and every bit
    // pattern is a valid `f32`, so `[u32]` and `[f32]` of one length share a
    // layout and `Arc`'s header; the pointer came from `Arc::into_raw` of an
    // `Arc<[u32]>` and is turned back exactly once. The f32 lanes are a byte
    // carrier: the glyph shader reads them as `instance(u32)`, never as float.
    unsafe { Arc::from_raw(raw) }
}

/// A frame's generation for one group: the scene's in the high half, the
/// group's own version in the low.
fn generation(scene: u64, version: u64) -> u64 {
    (scene << 32) ^ version
}

impl FrameFeed {
    /// Draw `frame` (scene `scene_generation`) into `rect` through `renderer`.
    pub fn draw(
        &mut self,
        cx: &mut makepad_widgets::Cx2d,
        renderer: &mut G3Renderer,
        draws: &mut crate::g3_passes::G3Draws,
        rect: makepad_widgets::Rect,
        scene_generation: u64,
        frame: &FrameData<'_>,
    ) {
        let atlas_generation = ATLAS_GENERATION;
        let use_bc7 = cx.gpu_info().texture_bc7;
        let atlas = (renderer.seen().atlas != Some(atlas_generation)).then(|| {
            let tables = float_tables::atlas_tables_of(frame.atlas);
            G3AtlasTables {
                glyphmap: tables.glyphmap,
                curves: tables.curves,
                advances: tables.glyph_advances,
                emoji: emoji_sheet_of(frame.atlas, use_bc7),
            }
        });

        let overrides: &[u32] = &frame.group_overrides;
        let rows = &frame.group_rows[..];
        let tables = match &mut self.scene {
            Some(t) if t.scene == scene_generation => {
                if t.override_words != overrides {
                    t.override_words = overrides.to_vec();
                    t.overrides = float_tables::override_texels(overrides);
                    t.tables_version += 1;
                }
                if rows.len() as u32 != t.group_count {
                    // Rows appended (a verb's new group): a fresh, whole table.
                    t.groups = float_tables::group_texels(rows);
                    t.group_count = rows.len() as u32;
                    t.dirty.clear();
                    t.groups_version += 1;
                } else if !frame.group_runs.is_empty() {
                    for run in &frame.group_runs {
                        let written = &rows[run.start as usize..run.end as usize];
                        t.dirty.extend(float_tables::write_group_rows(&mut t.groups, run.start, written));
                    }
                    t.groups_version += 1;
                }
                t
            }
            _ => self.scene.insert(SceneTables {
                scene: scene_generation,
                items: float_tables::item_texels(frame.item_table),
                overrides: float_tables::override_texels(overrides),
                override_words: overrides.to_vec(),
                groups: float_tables::group_texels(rows),
                group_count: rows.len() as u32,
                dirty: Vec::new(),
                tables_version: 0,
                groups_version: 0,
            }),
        };

        self.blocks.clear();
        self.blocks.extend(frame.slots.blocks().iter().map(words_as_f32));
        let statics_view = G3StaticTables {
            items: &tables.items,
            overrides: &tables.overrides,
        };
        let chunk_ranges = frame.glyph_ranges.per_chunk();
        let selection_ranges = frame.selection.as_ref().map(|s| s.ranges.per_chunk());
        let selection_blocks: Vec<usize> = selection_ranges
            .iter()
            .flat_map(|per| per.iter().enumerate().filter(|(_, r)| !r.is_empty()).map(|(k, _)| k))
            .collect();
        if selection_blocks != self.selection_blocks {
            let slots = frame.selection.as_ref().map_or(0, |s| s.ranges.slot_count());
            makepad_widgets::log!("g3: selection draws {slots} slots into blocks {selection_blocks:?}");
            self.selection_blocks = selection_blocks;
        }
        let selection = frame
            .selection
            .as_ref()
            .zip(selection_ranges.as_deref())
            .map(|(s, ranges)| G3Selection { chunk_ranges: ranges, tint: s.tint });
        let faces = frame.faces.as_deref().map_or(&[][..], Vec::as_slice);
        let regions = frame.regions.as_deref().map_or(&[][..], Vec::as_slice);
        let shape = (faces.len(), frame.quads.len(), frame.cubes.len(), regions.len(), frame.picked_region);
        if shape != self.shape {
            let (f, q, c, r, picked) = shape;
            makepad_widgets::log!("g3: meshes {f} faces, {q} quads, {c} cubes; {r} regions, picked {picked:?}");
            self.shape = shape;
        }
        if *self.trace.get_or_insert_with(|| std::env::var_os("GLYPH_FRAME_TRACE").is_some())
            && !(frame.group_runs.is_empty() && frame.face_runs.is_empty() && frame.region_runs.is_empty())
        {
            makepad_widgets::log!(
                "g3: frame runs: groups {:?}, faces {:?}, regions {:?}",
                frame.group_runs,
                frame.face_runs,
                frame.region_runs
            );
        }
        let [r, g, b, a] = frame.clear_color;
        let groups_generation = generation(scene_generation, tables.groups_version);
        let mut inputs = G3FrameInputs {
            atlas_generation,
            atlas,
            tables_generation: generation(scene_generation, tables.tables_version),
            tables: &statics_view,
            blocks: &self.blocks,
            meshes_generation: generation(scene_generation, frame.faces_generation ^ frame.meshes_generation.rotate_left(16)),
            faces,
            quads: frame.quads,
            cubes: frame.cubes,
            group_faces: &[],
            groups_generation,
            groups: G3GroupTable { table: &tables.groups, group_count: tables.group_count, dirty: &tables.dirty },
            view_proj: frame.view_proj,
            reverse_z: true,
            clear_color: [r as f32, g as f32, b as f32, a as f32],
            env: frame.env,
            glyph_params: frame.params,
            regions,
            region_params: frame.region_params.unwrap_or(RegionParams {
                edge_opacity: 0.0,
                edge_px: 0.0,
                picked_opacity: 0.0,
                picked_edge_opacity: 0.0,
            }),
            picked_region: frame.picked_region,
            backdrops: &frame.backdrops,
            chunk_ranges: &chunk_ranges,
            selection,
        };
        renderer.draw_frame(cx, draws, rect, &mut inputs);
        // The renderer consumed this generation: its rects are uploaded.
        if renderer.seen().groups == Some(groups_generation) {
            tables.dirty.clear();
        }
    }
}
