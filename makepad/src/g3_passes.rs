//! glyph3d-rs's six draw passes as native Makepad Splash `DrawVars` (Option A:
//! no wgpu, no compute). One shader per glyph3d pipeline, in glyph3d's phase
//! order:
//!
//! | glyph3d                          | here                                 |
//! |----------------------------------|--------------------------------------|
//! | environment.wgsl                 | `DrawG3Sky` + `DrawG3Ground`         |
//! | cull.wgsl (backdrop half)        | `DrawG3Backdrop`                     |
//! | mesh.wgsl                        | `DrawG3Mesh` (+ `DrawG3GroupFace`)   |
//! | library_regions.wgsl (render)    | `DrawG3Region`                       |
//! | glyph_field_derived.wgsl         | `DrawG3Glyph` + `DrawG3GlyphMask`    |
//! | composite.wgsl (fs_tint)         | `DrawG3Composite`                    |
//!
//! Every scene shader renders linear, premultiplied colour into a
//! `RenderRGBAf16` target (`color_format: @Rgba16F`), so blending happens in
//! linear light as it does in glyph3d's `Rgba8UnormSrgb` pool; the composite
//! encodes linear → sRGB into the UI. The selection mask renders into a
//! `RenderBGRAu8` target (`@Bgra8NoBlend`, coverage in alpha).
//!
//! Depth: glyph3d is Reverse-Z (`GreaterEqual`, clear 0.0). The scene pass
//! sets `DrawPass::set_reverse_z(cx, true)` and clears depth with 0.0; the
//! camera is glyph3d's `view_proj` in `pass_uniforms.camera_projection` with
//! `camera_view` = identity (both are applied, so a standard camera works too).
//! There is no hardware depth bias: backdrops and meshes bias in the vertex
//! stage, multiplicatively (`clip.z *= 1 - k`, `k = bias · 2⁻²³`), which tracks
//! Depth32Float's ULP across the whole near/far range and never pushes a
//! positive z past 0.0.
//!
//! glyph3d's GPU tables become `sample_nearest` lookup textures addressed at
//! texel centres (`G3Tables`). The engine packs them (glyph3d-native
//! `float_tables.rs`: integers as exact f32 below 2²⁴, never bitcast u32
//! bits; NO_CELL → -1.0); this file only uploads its `FloatTexture`s, and a
//! partial group upload names its `TexelRect`s.
//!
//! A host drives it all through `G3Renderer::draw_frame(cx, &mut draws, rect,
//! &G3FrameInputs)` from its widget's draw_walk, with one `#[live] draws:
//! G3Draws` field (`draws: mod.draw.G3Draws{}`). Inputs are engine-neutral
//! slices with a generation per slow group, so nothing re-uploads unless it
//! changed. `reverse_z` needs `DrawPass::set_reverse_z` (the per-pass HAL
//! reverse-Z option).
//!
//! Draw order: every phase item takes its own draw call (`new_draw_call`), so
//! `add_instance` can never merge an item into an earlier call of the same
//! shader and break glyph3d's phase order.

use makepad_widgets::makepad_draw::makepad_platform::retained_instances::RetainedInstances;
use makepad_widgets::*;
use glyph3d_native::float_tables::{FloatTexture, TexelRect, GROUP_TABLE_WIDTH, TABLE_WIDTH};
use glyph3d_native::glyph_scene::mesh::MeshInstance;
use glyph3d_native::glyph_scene::{BackdropInst, EnvUniform, Params, RegionInstance, RegionParams};
use std::ops::Range;

use crate::residency::{BlockResidency, UPLOAD_PER_FRAME};
use std::sync::Arc;

script_mod! {
    use mod.prelude.widgets.*
    use mod.math.*
    use mod.shader.*
    use mod.draw
    use mod.geom

    // ── Pass 1: environment.wgsl ─────────────────────────────────────────
    // glyph3d draws one full-screen triangle writing frag_depth (ground depth
    // or 0.0 for sky). Splash has no frag_depth, so it splits in two: the sky
    // as a quad AT the far plane (drawn first, depth untouched), then the
    // ground as real camera-relative geometry with hardware depth.

    mod.draw.DrawG3Sky = mod.std.set_type_default() do #(DrawG3Sky::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: false
        depth_write: false
        backface_culling: false
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.QuadVertex, geom.QuadGeom)
        // Env.inv_vp_rel: the inverse of proj × view-rotation (camera-relative).
        u_inv_vp_rel: uniform(mat4x4f(1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0))
        // rgb, w = gradient height (in sin(elevation))
        u_sky_horizon: uniform(vec4(0.62, 0.68, 0.74, 0.35))
        u_sky_zenith: uniform(vec4(0.24, 0.36, 0.56, 1.0))
        v_ndc: varying(vec2f)

        vertex: fn() {
            let ndc = self.geom.pos * 2.0 - vec2(1.0, 1.0)
            self.v_ndc = ndc
            self.vertex_pos = vec4(ndc.x, ndc.y, self.far_z, 1.0)
        }

        pixel: fn() {
            // Ray direction, camera-relative. NDC z = 1 is the NEAR plane
            // under reverse-Z; the eye is the origin, so any point on the
            // ray gives the direction under either convention.
            let h = self.u_inv_vp_rel * vec4(self.v_ndc.x, self.v_ndc.y, 1.0, 1.0)
            let dir = normalize(h.xyz / h.w)
            let elevation = smoothstep(0.0, max(self.u_sky_horizon.w, 0.000001), max(dir.y, 0.0))
            let sky = mix(self.u_sky_horizon.xyz, self.u_sky_zenith.xyz, elevation)
            return vec4(sky.x, sky.y, sky.z, 1.0)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    mod.draw.DrawG3Ground = mod.std.set_type_default() do #(DrawG3Ground::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: false
        depth_write: true
        backface_culling: false
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.QuadVertex, geom.QuadGeom)
        // struct Env, field for field (inv_vp_rel lives on the sky).
        u_vp_rel: uniform(mat4x4f(1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0))
        u_eye: uniform(vec4(0.0, 10.0, 0.0, 0.0))          // xyz = eye (world), w = ground_y
        u_grid_origin: uniform(vec4(0.0, 0.0, 0.0, 0.0))   // xy = eye.xz mod minor, zw = eye.xz mod major
        u_spacing: uniform(vec4(1.0, 10.0, 1.0, 2.0))      // minor, major, line px, axis px
        u_fade: uniform(vec4(50.0, 400.0, 6.0, 2.0))       // fog start, fog end, line fade start px, end px
        u_ground: uniform(vec4(0.10, 0.11, 0.12, 1.0))
        u_minor_line: uniform(vec4(0.20, 0.22, 0.24, 0.5))
        u_major_line: uniform(vec4(0.30, 0.32, 0.36, 0.8))
        u_axis_x: uniform(vec4(0.70, 0.25, 0.25, 1.0))     // the z = 0 line, running along x
        u_axis_z: uniform(vec4(0.25, 0.35, 0.75, 1.0))     // the x = 0 line, running along z
        u_sky_horizon: uniform(vec4(0.62, 0.68, 0.74, 0.35))
        v_rel: varying(vec3f)

        vertex: fn() {
            // One camera-centred quad at y = ground_y, ±R with R = fog end ×
            // `extent` (2.0: glyph3d's ray clamp). Positions stay
            // camera-relative, as in glyph3d, so a far eye costs no precision.
            let r = self.u_fade.y * self.extent
            let height = self.u_eye.y - self.u_eye.w
            let rel = vec3(mix(-r, r, self.geom.pos.x), -height, mix(-r, r, self.geom.pos.y))
            self.v_rel = rel
            // glyph3d's ground exists only seen from above.
            if height <= 0.0 {
                self.vertex_pos = vec4(2.0, 2.0, 2.0, 1.0)
                return
            }
            self.vertex_pos = self.u_vp_rel * vec4(rel.x, rel.y, rel.z, 1.0)
        }

        // Antialiased line coverage of the integer lines of `g` (grid
        // units) for a line `width_px` wide, and the on-screen size of one
        // cell in px. fwidth(g) is abs(dFdx(g)) + abs(dFdy(g)).
        grid_lines: fn(g: vec2, width_px: float) -> vec2 {
            let fw = max(abs(dFdx(g)) + abs(dFdy(g)), vec2(0.000001, 0.000001))
            let d = abs(fract(g - vec2(0.5, 0.5)) - vec2(0.5, 0.5)) / fw
            let coverage = 1.0 - clamp(min(d.x, d.y) - (width_px * 0.5 - 0.5), 0.0, 1.0)
            let cell_px = 1.0 / max(fw.x, fw.y)
            return vec2(coverage, cell_px)
        }

        axis_line: fn(v: float, width_px: float) -> float {
            let d = abs(v) / max(abs(dFdx(v)) + abs(dFdy(v)), 0.000001)
            return 1.0 - clamp(d - (width_px * 0.5 - 0.5), 0.0, 1.0)
        }

        pixel: fn() {
            // Straight-line code: every derivative is taken unconditionally.
            let rel = self.v_rel
            let t = length(rel)

            let minor = self.grid_lines((self.u_grid_origin.xy + rel.xz) / self.u_spacing.x, self.u_spacing.z)
            let major = self.grid_lines((self.u_grid_origin.zw + rel.xz) / self.u_spacing.y, self.u_spacing.z)
            let lf0 = self.u_fade.w
            let lf1 = self.u_fade.z
            let minor_a = minor.x * smoothstep(lf0, lf1, minor.y) * self.u_minor_line.w
            let major_a = major.x * smoothstep(lf0, lf1, major.y) * self.u_major_line.w

            let world_xz = self.u_eye.xz + rel.xz
            let ax = self.axis_line(world_xz.y, self.u_spacing.w) * self.u_axis_x.w
            let az = self.axis_line(world_xz.x, self.u_spacing.w) * self.u_axis_z.w

            let mut ground = self.u_ground.xyz
            ground = mix(ground, self.u_minor_line.xyz, minor_a)
            ground = mix(ground, self.u_major_line.xyz, major_a)
            ground = mix(ground, self.u_axis_x.xyz, ax)
            ground = mix(ground, self.u_axis_z.xyz, az)

            // Past the fog end the ground IS the horizon colour, so the quad's
            // edge and the sky behind it meet seamlessly.
            let fog = smoothstep(self.u_fade.x, self.u_fade.y, t)
            ground = mix(ground, self.u_sky_horizon.xyz, fog)
            return vec4(ground.x, ground.y, ground.z, 1.0)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    // ── Pass 2: cull.wgsl backdrops ──────────────────────────────────────
    // One flat tinted quad per far-LOD segment; the CPU cull compacts the
    // survivors and streams them per frame (`BackdropInst`, 9 lanes).

    mod.draw.DrawG3Backdrop = mod.std.set_type_default() do #(DrawG3Backdrop::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: true
        depth_write: true
        backface_culling: false
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.QuadVertex, geom.QuadGeom)
        // wgpu bias constant -100: 100 · 2⁻²³, applied multiplicatively.
        u_bias_k: uniform(0.000011920929)

        vertex: fn() {
            // y-up world: geom.pos.y = 0 is the rect's bottom (min_y).
            let c = self.geom.pos
            let p = vec2(
                mix(self.min_max.x, self.min_max.z, c.x),
                mix(self.min_max.y, self.min_max.w, c.y)
            )
            let clip = self.draw_pass.camera_projection * (self.draw_pass.camera_view * vec4(p.x, p.y, self.depth, 1.0))
            self.vertex_pos = vec4(clip.x, clip.y, clip.z * (1.0 - self.u_bias_k), clip.w)
        }

        pixel: fn() {
            // Premultiplied: mean linear ink colour attenuated by coverage E.
            return vec4(self.rgba.x * self.rgba.w, self.rgba.y * self.rgba.w, self.rgba.z * self.rgba.w, self.rgba.w)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    // ── Pass 3: mesh.wgsl ────────────────────────────────────────────────
    // MeshInstance (80 B) as five vec4 instance lanes; the geometry is
    // glyph3d's unit quad or cube (`G3MeshGeometries`) in CubeVertex layout.

    mod.draw.DrawG3Mesh = mod.std.set_type_default() do #(DrawG3Mesh::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: true
        depth_write: true
        backface_culling: true
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.CubeVertex, geom.CubeGeom)
        // wgpu bias constant -50: 50 · 2⁻²³, applied multiplicatively.
        u_bias_k: uniform(0.0000059604645)
        v_normal: varying(vec3f)

        vertex: fn() {
            let p = self.geom.geom_pos
            let n = self.geom.geom_normal
            let world = self.col0.xyz * p.x + self.col1.xyz * p.y + self.col2.xyz * p.z
                + vec3(self.col0.w, self.col1.w, self.col2.w)
            let clip = self.draw_pass.camera_projection * (self.draw_pass.camera_view * vec4(world.x, world.y, world.z, 1.0))
            self.vertex_pos = vec4(clip.x, clip.y, clip.z * (1.0 - self.u_bias_k), clip.w)
            // mat3(col0, col1, col2) * n
            self.v_normal = normalize(self.col0.xyz * n.x + self.col1.xyz * n.y + self.col2.xyz * n.z)
        }

        pixel: fn() {
            if self.params.w == 0.0 {
                return self.color
            }
            // Simple directional diffuse light, 0.2 ambient.
            let light_dir = normalize(vec3(1.0, 1.0, 1.0))
            let ndotl = max(dot(self.v_normal, light_dir), 0.0)
            let diffuse = ndotl * 0.8 + 0.2
            return vec4(self.color.x * diffuse, self.color.y * diffuse, self.color.z * diffuse, self.color.w)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    // A library page face composed in the vertex stage from its sheet's
    // group row (`t_groups`, the same table the glyphs read), so a static
    // face spec follows its sheet through drags, eases and whole-library
    // moves with zero per-frame face upload. The arithmetic is
    // library_faces.wgsl's / faces.rs `face_instance`: the face sits at
    // (0, 0, -gap) in the sheet's frame, a unit quad scaled to the page.
    // PRECONDITION (not met yet, 2026-10-26): the spec's group row must BE
    // its sheet's world (offset = translation, quat = rotation, scale =
    // uniform scale). Today sheets have no rows of their own; a file's row is
    // sheet ∘ mount (contain-fit scale + offset), so leave `group_faces`
    // empty and draw faces as MeshInstances until sheets route to rows.
    // Not bit-exact to `face_instance` either (different operation order).
    mod.draw.DrawG3GroupFace = mod.std.set_type_default() do #(DrawG3GroupFace::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: true
        depth_write: true
        backface_culling: true
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.CubeVertex, geom.CubeGeom)
        t_groups: texture_2d(float)
        u_groups_inv_size: uniform(vec2(1.0, 1.0))
        u_max_groups: uniform(1.0)
        u_bias_k: uniform(0.0000059604645)

        group_texel: fn(idx: u32) -> vec4 {
            let row = idx / u32(1020)
            let col = idx - row * u32(1020)
            let uv = vec2((float(col) + 0.5) * self.u_groups_inv_size.x, (float(row) + 0.5) * self.u_groups_inv_size.y)
            return self.t_groups.sample_nearest(uv, 0.0)
        }

        vertex: fn() {
            let group_id = uint(self.spec.w)
            let max_groups = uint(self.u_max_groups)
            let gbase = min(group_id, max(max_groups, u32(1)) - u32(1)) * u32(6)
            let gpos = self.group_texel(gbase)
            let gquat = self.group_texel(gbase + u32(1))
            let gscale = self.group_texel(gbase + u32(3))
            // faces.rs skips a face whose sheet has (almost) no scale.
            if group_id >= max_groups || dot(gscale.xyz, gscale.xyz) < 0.000001 {
                self.vertex_pos = vec4(2.0, 2.0, 2.0, 1.0)
                return
            }
            let p = self.geom.geom_pos
            let local = vec3(p.x * self.spec.x, p.y * self.spec.y, -self.spec.z) * gscale.xyz
            let qc = cross(gquat.xyz, local) + local * gquat.w
            let posed = local + cross(gquat.xyz, qc) * 2.0
            let world = posed + gpos.xyz
            let clip = self.draw_pass.camera_projection * (self.draw_pass.camera_view * vec4(world.x, world.y, world.z, 1.0))
            self.vertex_pos = vec4(clip.x, clip.y, clip.z * (1.0 - self.u_bias_k), clip.w)
        }

        pixel: fn() {
            // Faces are unlit (face_instance writes material_params = 0).
            return self.color
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    // ── Pass 4: library_regions.wgsl (render half) ───────────────────────
    // One translucent double-sided box per directory, depth tested, not
    // written. CubeGeom carries the face uv, so no vertex_index is needed.

    mod.draw.DrawG3Region = mod.std.set_type_default() do #(DrawG3Region::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: true
        depth_write: false
        backface_culling: false
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.CubeVertex, geom.CubeGeom)
        // edge_opacity, edge_px, picked_opacity, picked_edge_opacity
        u_region_params: uniform(vec4(0.6, 1.0, 0.25, 1.0))
        v_uv: varying(vec2f)

        vertex: fn() {
            let c = self.geom.geom_pos
            let world = self.col0.xyz * c.x + self.col1.xyz * c.y + self.col2.xyz * c.z
                + vec3(self.col0.w, self.col1.w, self.col2.w)
            self.vertex_pos = self.draw_pass.camera_projection * (self.draw_pass.camera_view * vec4(world.x, world.y, world.z, 1.0))
            self.v_uv = self.geom.geom_uv
        }

        pixel: fn() {
            // Pixels to the nearest edge of this face: each uv coordinate's
            // distance to 0 or 1 over its screen-space rate.
            let fw = max(abs(dFdx(self.v_uv)) + abs(dFdy(self.v_uv)), vec2(0.000001, 0.000001))
            let d = min(self.v_uv, vec2(1.0, 1.0) - self.v_uv) / fw
            let px = min(d.x, d.y)
            let edge = clamp(self.u_region_params.y + 0.5 - px, 0.0, 1.0)
            let mut fill = self.color.w
            let mut rim = self.u_region_params.x
            // Instance lanes are exact in the fragment stage (flat instance id).
            if self.picked_flag > 0.5 {
                fill = self.u_region_params.z
                rim = self.u_region_params.w
            }
            let a = mix(fill, rim, edge)
            return vec4(self.color.x * a, self.color.y * a, self.color.z * a, a)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    // ── Pass 5: glyph_field_derived.wgsl ─────────────────────────────────
    // A 1:1 port. The instance record is DerivedSlot (20 B = 5 words),
    // byte-identical: script-declared lanes enter the record in first-read
    // order, so `vertex` reads all five, in DerivedSlot order, first.

    mod.draw.DrawG3Glyph = mod.std.set_type_default() do #(DrawG3Glyph::script_shader(vm)){
        color_format: @Rgba16F
        alpha_blend: true
        depth_write: true
        backface_culling: false
        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)
        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.QuadVertex, geom.QuadGeom)

        // DerivedSlot, word for word.
        x: instance(float)               // w0: world x (pen origin)
        row: instance(u32)               // w1: row:24 | x_page:8
        glyph_and_wrap: instance(u32)    // w2: glyph_id:16 | wrap_segment:16
        color: instance(u32)             // w3: packed sRGB RGBA8 (r in bits 0..7)
        item_and_group: instance(u32)    // w4: item:20 | override:12

        t_groups: texture_2d(float)          // GroupRow: 6 texels/group, 1020 wide
        t_items: texture_2d(float)           // ItemParamsGpu: 4 texels/item, 1024 wide
        t_overrides: texture_2d(float)       // group_overrides (R), 1024 wide
        t_advances: texture_2d(float)        // glyph_advances (R), 1024 wide
        t_glyphmap: texture_2d(float)        // [curve_start, curve_count, mode, emoji_cell | -1], 1024 wide
        t_curves: texture_2d(float)          // 2 texels/curve, u16 coords as exact floats, 1024 wide
        t_emoji: texture_2d_array(float)     // emoji sheet layers (sRGB-encoded BGRA8, mips)

        u_groups_inv_size: uniform(vec2(1.0, 1.0))
        u_items_inv_size: uniform(vec2(1.0, 1.0))
        u_overrides_inv_size: uniform(vec2(1.0, 1.0))
        u_advances_inv_size: uniform(vec2(1.0, 1.0))
        u_glyphmap_inv_size: uniform(vec2(1.0, 1.0))
        u_curves_inv_size: uniform(vec2(1.0, 1.0))

        // struct Params (integers as exact floats).
        u_max_groups: uniform(1.0)
        u_greek_mode: uniform(1.0)           // 0 off, 1 smooth blend, 2 hard bypass
        u_dilate_px: uniform(0.75)
        u_soften: uniform(0.45)
        u_min_lo: uniform(0.06)
        u_min_hi: uniform(0.20)
        u_emoji_cell: uniform(vec2(64.0, 64.0))
        u_emoji_cols: uniform(128.0)
        u_emoji_rows: uniform(68.0)
        u_emoji_layer: uniform(vec2(8192.0, 4352.0))
        u_greek_onset_px: uniform(10.0)
        // 1.0 in the selection-mask pass: the output is coverage, as data.
        u_mask_mode: uniform(0.0)

        v_color: varying(vec4f)      // rgb = blended ink colour, a = group alpha
        v_uv: varying(vec4f)         // xy = glyph uv, zw = emoji uv
        // curve_start, curve_count, mode, emoji_layer: exact integers,
        // constant per primitive (Splash has no flat user varyings), rounded
        // back in the pixel stage.
        v_glyph: varying(vec4f)
        v_group_rgb: varying(vec4f)
        v_bg: varying(vec4f)

        table_uv: fn(idx: u32, width: u32, inv_size: vec2) -> vec2 {
            let row = idx / width
            let col = idx - row * width
            return vec2((float(col) + 0.5) * inv_size.x, (float(row) + 0.5) * inv_size.y)
        }
        group_texel: fn(idx: u32) -> vec4 {
            return self.t_groups.sample_nearest(self.table_uv(idx, u32(1020), self.u_groups_inv_size), 0.0)
        }
        item_texel: fn(idx: u32) -> vec4 {
            return self.t_items.sample_nearest(self.table_uv(idx, u32(1024), self.u_items_inv_size), 0.0)
        }
        override_at: fn(idx: u32) -> float {
            return self.t_overrides.sample_nearest(self.table_uv(idx, u32(1024), self.u_overrides_inv_size), 0.0).x
        }
        advance_at: fn(idx: u32) -> float {
            return self.t_advances.sample_nearest(self.table_uv(idx, u32(1024), self.u_advances_inv_size), 0.0).x
        }
        glyphmap_at: fn(idx: u32) -> vec4 {
            return self.t_glyphmap.sample_nearest(self.table_uv(idx, u32(1024), self.u_glyphmap_inv_size), 0.0)
        }
        // A curve's two texels: the index is an exact float (< 2²⁴).
        curve_texel: fn(idx: float) -> vec4 {
            let row = floor(idx / 1024.0)
            let col = idx - row * 1024.0
            let uv = vec2((col + 0.5) * self.u_curves_inv_size.x, (row + 0.5) * self.u_curves_inv_size.y)
            return self.t_curves.sample_nearest(uv, 0.0)
        }

        // derive.rs's derive_yz. The item rows are ItemParamsGpu as texels:
        // it0 = line_height, origin_y, origin_z, z_step
        // it1 = z_step_lo, band_stride_y, depth_per_band, depth_per_col
        // it2 = page_rows, pages_wide, page_cols, scroll_rows (exact integers)
        // it3 = has_page, line_height_lo, group, 0
        derive_yz: fn(row_lane: u32, wrap_segment: u32, it0: vec4, it1: vec4, it2: vec4, it3: vec4) -> vec2 {
            let row = row_lane & u32(16777215)
            let x_page = row_lane >> u32(24)

            let depth_steps = -(float(wrap_segment))
            let z_tail = fma(depth_steps, it1.x, it0.z)
            let z_stepped = fma(depth_steps, it0.w, z_tail)

            let mut derived_y = 0.0
            let mut derived_z = 0.0
            if it3.x != 0.0 {
                let page_rows = int(it2.x)
                let screen_row = int(row) - int(it2.w)
                let mut y_page = int(0)
                if page_rows > int(0) && screen_row >= page_rows {
                    y_page = screen_row / page_rows
                }
                let pages_wide = max(int(it2.y), int(1))
                let band = y_page / pages_wide
                let row_in_page = float(screen_row - y_page * page_rows)
                let y_tail = fma(-row_in_page, it3.y, it0.y)
                let y_row_folded = fma(-row_in_page, it0.x, y_tail)
                derived_y = fma(-(float(band)), it1.y, y_row_folded)

                let z_banded = fma(float(band), it1.z, z_stepped)
                derived_z = fma(float(x_page), it1.w, z_banded)
            } else {
                let y_tail = fma(-(float(row)), it3.y, it0.y)
                derived_y = fma(-(float(row)), it0.x, y_tail)
                derived_z = z_stepped
            }
            return vec2(derived_y, derived_z)
        }

        vertex: fn() {
            // DerivedSlot lanes, in record order, before anything else.
            let inst_x = self.x
            let inst_row = self.row
            let inst_glyph_and_wrap = self.glyph_and_wrap
            let inst_color = self.color
            let inst_item_and_group = self.item_and_group

            // The lane is item:20 | override:12.
            let item_idx = inst_item_and_group & u32(1048575)
            let override_idx = inst_item_and_group >> u32(20)
            let ibase = item_idx * u32(4)
            let it0 = self.item_texel(ibase)
            let it1 = self.item_texel(ibase + u32(1))
            let it2 = self.item_texel(ibase + u32(2))
            let it3 = self.item_texel(ibase + u32(3))
            let mut group_id = uint(it3.z)
            if override_idx != u32(0) {
                group_id = uint(self.override_at(override_idx))
            }
            let wrap_segment = inst_glyph_and_wrap >> u32(16)
            let glyph_id = inst_glyph_and_wrap & u32(65535)
            let yz = self.derive_yz(inst_row, wrap_segment, it0, it1, it2, it3)
            let inst_pos = vec3(inst_x, yz.x, yz.y)

            // Glyph-map lookup: slot → curve range + mode.
            let info = self.glyphmap_at(glyph_id)
            let mode = uint(info.z) // 0 = outline, 1 = bitmap emoji

            let mut quad_w = self.advance_at(glyph_id)
            if mode == u32(1) {
                quad_w = 1.0
            }

            let c = self.geom.pos
            let aligned = vec3(c.x * quad_w, (c.y - 0.5) * 1.0, 0.0) + inst_pos

            // Group table row: offset / quat / color+alpha / scale+colorBlend / clip / bg_color.
            let max_groups = uint(self.u_max_groups)
            let grow = min(group_id, max(max_groups, u32(1)) - u32(1))
            let gbase = grow * u32(6)
            let gpos = self.group_texel(gbase)
            let gquat = self.group_texel(gbase + u32(1))
            let gcolor = self.group_texel(gbase + u32(2))
            let gscale = self.group_texel(gbase + u32(3))
            let gclip = self.group_texel(gbase + u32(4))
            let gbg = self.group_texel(gbase + u32(5))

            // World = rotate(quat, aligned * groupScale) + groupOffset (T·R·S).
            let local = aligned * gscale.xyz
            let qc = cross(gquat.xyz, local) + local * gquat.w
            let posed = local + cross(gquat.xyz, qc) * 2.0
            let world = posed + gpos.xyz
            let mut clip = self.draw_pass.camera_projection * (self.draw_pass.camera_view * vec4(world.x, world.y, world.z, 1.0))

            // Vertex culls → degenerate to outside-NDC.
            if group_id >= max_groups || gcolor.w <= 0.01 {
                clip = vec4(2.0, 2.0, 2.0, 1.0)
            }
            if gclip.z > 0.5 && (inst_pos.y > gclip.x || inst_pos.y < gclip.y) {
                clip = vec4(2.0, 2.0, 2.0, 1.0)
            }

            let icolor = vec3(
                float(inst_color & u32(255)),
                float((inst_color >> u32(8)) & u32(255)),
                float((inst_color >> u32(16)) & u32(255))
            ) / 255.0
            let base_color = icolor * gcolor.xyz
            let blended = base_color + (gcolor.xyz - base_color) * gscale.w

            let mut emoji_uv = vec2(0.0, 0.0)
            let mut emoji_layer = 0.0
            // info.w is the emoji cell, or -1.0 for NO_CELL.
            if mode == u32(1) && info.w >= 0.0 {
                let cell = uint(info.w)
                let cols = uint(self.u_emoji_cols)
                let per_layer = cols * uint(self.u_emoji_rows)
                emoji_layer = float(cell / per_layer)
                let r = cell % per_layer
                let cell_xy = vec2(float(r % cols), float(r / cols)) * self.u_emoji_cell
                let inset = vec2(0.5, 0.5)
                let span = self.u_emoji_cell - vec2(1.0, 1.0)
                let t = vec2(c.x, 1.0 - c.y)
                emoji_uv = (cell_xy + inset + t * span) / self.u_emoji_layer
            }
            let mut out_mode = mode
            if mode == u32(1) && info.w < 0.0 {
                out_mode = u32(2)
            }

            self.vertex_pos = clip
            self.v_color = vec4(blended.x, blended.y, blended.z, gcolor.w)
            self.v_uv = vec4(c.x, c.y, emoji_uv.x, emoji_uv.y)
            self.v_glyph = vec4(info.x, info.y, float(out_mode), emoji_layer)
            self.v_group_rgb = gcolor
            self.v_bg = gbg
        }

        rot90: fn(v: vec2) -> vec2 {
            return vec2(v.y, -v.x)
        }

        compute_coverage: fn(inv_diameter: float, dilate: float, p0: vec2, p1: vec2, p2: vec2) -> float {
            let mut result = 0.0

            let all_above = p0.y > 0.0 && p1.y > 0.0 && p2.y > 0.0
            let all_below = p0.y < 0.0 && p1.y < 0.0 && p2.y < 0.0

            if !(all_above || all_below) {
                let a = p0 - p1 * 2.0 + p2
                let b = p0 - p1
                let c = p0

                let mut t0 = -1.0
                let mut t1 = -1.0
                let mut solvable = true

                if abs(a.y) >= 0.00001 {
                    let radicand = b.y * b.y - a.y * c.y
                    if radicand > 0.0 {
                        let s = sqrt(radicand)
                        let mut q = b.y - s
                        if b.y >= 0.0 {
                            q = b.y + s
                        }
                        if b.y >= 0.0 {
                            t0 = c.y / q
                            t1 = q / a.y
                        } else {
                            t0 = q / a.y
                            t1 = c.y / q
                        }
                    } else {
                        solvable = false
                    }
                } else {
                    let denom = p0.y - p2.y
                    if abs(denom) >= 0.000001 {
                        let t = p0.y / denom
                        if p0.y < p2.y {
                            t0 = -1.0
                            t1 = t
                        } else {
                            t0 = t
                            t1 = -1.0
                        }
                    } else {
                        solvable = false
                    }
                }

                if solvable {
                    if t0 >= 0.0 && t0 < 1.0 {
                        let x = (a.x * t0 - b.x * 2.0) * t0 + c.x
                        result = result + clamp(x * inv_diameter + 0.5 + dilate, 0.0, 1.0)
                    }
                    if t1 >= 0.0 && t1 < 1.0 {
                        let x = (a.x * t1 - b.x * 2.0) * t1 + c.x
                        result = result - clamp(x * inv_diameter + 0.5 - dilate, 0.0, 1.0)
                    }
                }
            }

            return result
        }

        glyph_out: fn(rgb: vec3, alpha: float) -> vec4 {
            if self.u_mask_mode > 0.5 {
                return vec4(alpha, alpha, alpha, alpha)
            }
            return vec4(rgb.x, rgb.y, rgb.z, alpha)
        }

        pixel: fn() {
            // Every derivative first, in uniform control flow: before any
            // branch or discard.
            let fw = abs(dFdx(self.v_uv.xy)) + abs(dFdy(self.v_uv.xy))
            let emoji_dx = dFdx(self.v_uv.zw)
            let emoji_dy = dFdy(self.v_uv.zw)

            let glyph_uv = self.v_uv.xy
            let group_alpha = self.v_color.w
            let curve_start = floor(self.v_glyph.x + 0.5)
            let curve_count = floor(self.v_glyph.y + 0.5)
            let mode = floor(self.v_glyph.z + 0.5)
            let emoji_layer = floor(self.v_glyph.w + 0.5)

            if mode == 1.0 {
                let s = self.t_emoji.sample_grad(vec3(self.v_uv.z, self.v_uv.w, emoji_layer), emoji_dx, emoji_dy)
                let alpha = s.w * group_alpha
                if alpha <= 0.0 {
                    discard()
                }
                // The sheet is sRGB-encoded (no sRGB sampling format): decode here.
                let rgb = pow(s.xyz, vec3(2.2, 2.2, 2.2)) * pow(self.v_group_rgb.xyz, vec3(2.2, 2.2, 2.2)) * alpha
                return self.glyph_out(rgb, alpha)
            }
            if mode == 2.0 {
                discard()
            }

            // Empty glyph (space / .notdef): no ink.
            if curve_count == 0.0 && self.v_bg.w <= 0.0 {
                discard()
            }

            let em_px = 1.0 / max(fw.y, 0.0001)
            let mut greek = 0.0
            if curve_count > 0.0 {
                if self.u_greek_mode == 1.0 {
                    let onset = self.u_greek_onset_px
                    let full = onset * 0.45
                    let t = clamp((onset - em_px) / max(onset - full, 0.01), 0.0, 1.0)
                    greek = t * t * (3.0 - 2.0 * t)
                } else if self.u_greek_mode == 2.0 {
                    if em_px <= self.u_greek_onset_px {
                        greek = 1.0
                    }
                }
            }

            let mut cov = 0.0
            if greek < 1.0 && curve_count > 0.0 {
                let fw_max = max(fw.x, fw.y)
                let mut m = clamp((fw_max - self.u_min_lo) / (self.u_min_hi - self.u_min_lo), 0.0, 1.0)
                m = m * m * (3.0 - 2.0 * m)

                let dilate = m * self.u_dilate_px
                let inv_d = vec2(1.0 / fw.x, 1.0 / fw.y) * (1.0 - m * self.u_soften)

                let mut coverage = 0.0
                // MAX_CURVES = 512: a safety bound, not a budget.
                let n = min(curve_count, 512.0)
                let mut i = 0.0
                loop {
                    if i >= n {
                        break
                    }
                    let ci = (curve_start + i) * 2.0
                    let t0 = self.curve_texel(ci)
                    let t1 = self.curve_texel(ci + 1.0)

                    let p0 = vec2(t0.x, t0.y) / 65535.0 - glyph_uv
                    let p1 = vec2(t0.z, t0.w) / 65535.0 - glyph_uv
                    let p2 = vec2(t1.x, t1.y) / 65535.0 - glyph_uv

                    coverage = coverage + self.compute_coverage(inv_d.x, dilate, p0, p1, p2)
                    coverage = coverage + self.compute_coverage(inv_d.y, dilate, self.rot90(p0), self.rot90(p1), self.rot90(p2))
                    i = i + 1.0
                }
                cov = clamp(coverage * 0.5, 0.0, 1.0)
            }

            if greek > 0.0 && curve_count > 0.0 {
                let dy = max(fw.y, 0.0001)
                let y_cov = clamp((glyph_uv.y - 0.20) / dy + 0.5, 0.0, 1.0)
                    - clamp((glyph_uv.y - 0.75) / dy + 0.5, 0.0, 1.0)
                let dx = max(fw.x, 0.0001)
                let x_cov = clamp((glyph_uv.x - 0.0) / dx + 0.5, 0.0, 1.0)
                    - clamp((glyph_uv.x - 1.0) / dx + 0.5, 0.0, 1.0)
                let bar_cov = clamp(x_cov * y_cov, 0.0, 1.0)
                cov = mix(cov, bar_cov, greek)
            }

            let fg_alpha = cov * group_alpha
            let bg_alpha = self.v_bg.w * group_alpha

            if fg_alpha <= 0.0 && bg_alpha <= 0.0 {
                discard()
            }

            let fg_rgb = pow(self.v_color.xyz, vec3(2.2, 2.2, 2.2)) * fg_alpha
            let bg_rgb = pow(self.v_bg.xyz, vec3(2.2, 2.2, 2.2)) * bg_alpha

            let out_alpha = fg_alpha + bg_alpha * (1.0 - fg_alpha)
            let out_rgb = fg_rgb + bg_rgb * (1.0 - fg_alpha)

            return self.glyph_out(out_rgb, out_alpha)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    // The selection mask: the same glyphs (the same chunk publications,
    // ranges = the selection) into a BGRA8 data target, blending off, no
    // depth, coverage in alpha.
    mod.draw.DrawG3GlyphMask = mod.std.set_type_default() do #(DrawG3GlyphMask::script_shader(vm)){
        ..mod.draw.DrawG3Glyph
        color_format: @Bgra8NoBlend
        alpha_blend: false
        depth_write: false
        u_mask_mode: uniform(1.0)
    }

    // ── Pass 6: composite.wgsl (fs_tint) ─────────────────────────────────
    // The linear f16 scene (+ the selection tint) encoded to sRGB into the
    // UI. An exact overwrite, not a blend.

    mod.draw.DrawG3Composite = mod.std.set_type_default() do #(DrawG3Composite::script_shader(vm)){
        ..mod.draw.DrawQuad
        alpha_blend: false
        t_scene: texture_2d(float)
        t_mask: texture_2d(float)
        // rgb = tint, a = strength
        u_tint: uniform(vec4(0.35, 0.6, 1.0, 0.45))
        u_has_selection: uniform(0.0)

        linear_to_srgb: fn(c: vec3) -> vec3 {
            let l = clamp(c, vec3(0.0, 0.0, 0.0), vec3(1.0, 1.0, 1.0))
            let lo = l * 12.92
            let hi = pow(l, vec3(0.41666666, 0.41666666, 0.41666666)) * 1.055 - vec3(0.055, 0.055, 0.055)
            return mix(lo, hi, step(vec3(0.0031308, 0.0031308, 0.0031308), l))
        }

        pixel: fn() {
            // Both samples first (implicit derivatives, uniform flow).
            let scene = self.t_scene.sample(self.pos)
            let m = self.t_mask.sample(self.pos)
            let mut rgb = scene.xyz
            if self.u_has_selection > 0.5 {
                rgb = rgb + self.u_tint.xyz * m.w * self.u_tint.w
            }
            let srgb = self.linear_to_srgb(rgb)
            return vec4(srgb.x, srgb.y, srgb.z, 1.0)
        }
    }

    // Every shader a viewport needs, as one `#[live] draws: G3Draws` field.
    mod.draw.G3Draws = mod.std.set_type_default() do #(G3Draws::script_component(vm)){
        sky: mod.draw.DrawG3Sky{}
        ground: mod.draw.DrawG3Ground{}
        backdrop: mod.draw.DrawG3Backdrop{}
        mesh: mod.draw.DrawG3Mesh{}
        group_face: mod.draw.DrawG3GroupFace{}
        region: mod.draw.DrawG3Region{}
        glyph: mod.draw.DrawG3Glyph{}
        glyph_mask: mod.draw.DrawG3GlyphMask{}
        composite: mod.draw.DrawG3Composite{}
    }
}

// ---------------------------------------------------------------------------
// Constants shared with the shaders above.
// ---------------------------------------------------------------------------

/// 2⁻²³: one Depth32Float ULP at z ∈ [0.5, 1), the unit of wgpu's constant bias.
pub const G3_DEPTH_ULP: f32 = 1.1920929e-7;
/// cull.wgsl's backdrop pipeline: `DepthBiasState { constant: -100, .. }`.
pub const G3_BACKDROP_BIAS_K: f32 = 100.0 * G3_DEPTH_ULP;
/// mesh.wgsl's pipeline: `DepthBiasState { constant: -50, .. }`.
pub const G3_MESH_BIAS_K: f32 = 50.0 * G3_DEPTH_ULP;
/// Words in one DerivedSlot (`x, row, glyph_and_wrap, color, item_and_group`).
pub const G3_DERIVED_SLOT_WORDS: usize = 5;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Binds `texture` to the shader texture named `name`. Slots follow the
/// shader's IO order, which is not necessarily declaration order, so
/// textures are always bound by name.
pub fn g3_set_texture(cx: &Cx, draw_vars: &mut DrawVars, name: LiveId, texture: &Texture) -> bool {
    let Some(shader_id) = draw_vars.draw_shader_id else {
        return false;
    };
    let Some(slot) = cx.draw_shaders[shader_id.index]
        .mapping
        .textures
        .iter()
        .position(|t| t.id == name)
    else {
        return false;
    };
    draw_vars.set_texture(slot, texture);
    true
}

/// One instance as its own draw call (a phase item never merges into an
/// earlier call of the same shader).
fn g3_draw_single(cx: &mut CxDraw, draw_vars: &mut DrawVars) {
    if !draw_vars.can_instance() {
        return;
    }
    cx.new_draw_call(draw_vars);
    let area = cx.add_instance(draw_vars);
    draw_vars.area = cx.update_area_refs(draw_vars.area, area);
}

/// A stream of whole instance records, re-recorded this frame, as one fresh
/// draw call.
fn g3_draw_stream(cx: &mut CxDraw, draw_vars: &mut DrawVars, records: &[f32]) {
    if !draw_vars.can_instance() || records.is_empty() {
        return;
    }
    cx.new_draw_call(draw_vars);
    if let Some(mut many) = cx.begin_many_instances(draw_vars) {
        many.instances.extend_from_slice(records);
        let area = cx.end_many_instances(many);
        draw_vars.area = cx.update_area_refs(draw_vars.area, area);
    }
}

/// A retained publication (uploaded once, re-attached every frame without a
/// copy) as one fresh draw call.
fn g3_draw_retained(cx: &mut CxDraw, draw_vars: &mut DrawVars, publication: &RetainedInstances) {
    if !draw_vars.can_instance() || publication.count() == 0 {
        return;
    }
    let area = cx.add_retained_instances(draw_vars, publication);
    draw_vars.area = cx.update_area_refs(draw_vars.area, area);
}

fn g3_inv_size(texture: &Texture, cx: &Cx) -> [f32; 2] {
    let (w, h) = texture.allocated_size(cx).unwrap_or((1, 1));
    [1.0 / w.max(1) as f32, 1.0 / h.max(1) as f32]
}

// ---------------------------------------------------------------------------
// Pass 1: environment
// ---------------------------------------------------------------------------

/// Whether the ground draws: glyph3d's plane exists only seen from above
/// (`EnvUniform::eye` is the eye in xyz and the ground height in w; the
/// engine builds it camera-relative, its grid phase reduced in f64).
fn g3_ground_visible(env: &EnvUniform) -> bool {
    env.eye[1] > env.eye[3]
}

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Sky {
    #[deref]
    pub draw_vars: DrawVars,
    /// The far plane's NDC depth: 0.0 under reverse-Z (the cleared value),
    /// 1.0 in a standard-Z pass.
    #[live(0.0)]
    pub far_z: f32,
}

impl DrawG3Sky {
    pub fn set_env(&mut self, cx: &Cx, env: &EnvUniform) {
        self.draw_vars.set_uniform(cx, live_id!(u_inv_vp_rel), &env.inv_vp_rel);
        self.draw_vars.set_uniform(cx, live_id!(u_sky_horizon), &env.sky_horizon);
        self.draw_vars.set_uniform(cx, live_id!(u_sky_zenith), &env.sky_zenith);
    }

    /// Draw first in the scene pass, right after the clear.
    pub fn draw(&mut self, cx: &mut CxDraw) {
        g3_draw_single(cx, &mut self.draw_vars);
    }
}

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Ground {
    #[deref]
    pub draw_vars: DrawVars,
    /// The quad's half-extent in fog-end units (glyph3d clamps the ground
    /// ray at `fade.y * 2`).
    #[live(2.0)]
    pub extent: f32,
}

impl DrawG3Ground {
    pub fn set_env(&mut self, cx: &Cx, env: &EnvUniform) {
        let dv = &mut self.draw_vars;
        dv.set_uniform(cx, live_id!(u_vp_rel), &env.vp_rel);
        dv.set_uniform(cx, live_id!(u_eye), &env.eye);
        dv.set_uniform(cx, live_id!(u_grid_origin), &env.grid_origin);
        dv.set_uniform(cx, live_id!(u_spacing), &env.spacing);
        dv.set_uniform(cx, live_id!(u_fade), &env.fade);
        dv.set_uniform(cx, live_id!(u_ground), &env.ground);
        dv.set_uniform(cx, live_id!(u_minor_line), &env.minor_line);
        dv.set_uniform(cx, live_id!(u_major_line), &env.major_line);
        dv.set_uniform(cx, live_id!(u_axis_x), &env.axis_x);
        dv.set_uniform(cx, live_id!(u_axis_z), &env.axis_z);
        dv.set_uniform(cx, live_id!(u_sky_horizon), &env.sky_horizon);
    }

    /// Draw right after the sky; skipped when the eye is at or below the plane.
    pub fn draw(&mut self, cx: &mut CxDraw, env: &EnvUniform) {
        if g3_ground_visible(env) {
            g3_draw_single(cx, &mut self.draw_vars);
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 2: backdrops
// ---------------------------------------------------------------------------

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Backdrop {
    #[deref]
    pub draw_vars: DrawVars,
    /// min.xy, max.xy
    #[live]
    pub min_max: Vec4f,
    /// rgb linear, a = effective ink coverage E
    #[live]
    pub rgba: Vec4f,
    #[live]
    pub depth: f32,
}

/// Lanes per backdrop record: `min_max` (4) + `rgba` (4) + `depth` (1).
pub const G3_BACKDROP_LANES: usize = 9;

impl DrawG3Backdrop {
    /// The frame's compacted backdrop survivors as ONE instanced draw call.
    /// `scratch` is reused across frames.
    pub fn draw_all(&mut self, cx: &mut CxDraw, backdrops: &[BackdropInst], scratch: &mut Vec<f32>) {
        scratch.clear();
        scratch.reserve(backdrops.len() * G3_BACKDROP_LANES);
        for b in backdrops {
            scratch.extend_from_slice(&[b.min[0], b.min[1], b.max[0], b.max[1]]);
            scratch.extend_from_slice(&b.rgba);
            scratch.push(b.depth[0]);
        }
        self.draw_vars.set_uniform(cx, live_id!(u_bias_k), &[G3_BACKDROP_BIAS_K]);
        g3_draw_stream(cx, &mut self.draw_vars, scratch);
    }
}

// ---------------------------------------------------------------------------
// Pass 3: meshes
// ---------------------------------------------------------------------------

/// Lanes per mesh record: `col0..col2`, `color`, `params`.
pub const G3_MESH_LANES: usize = 20;

/// Publish the engine's mesh instances (mesh.rs's `MeshInstance`, 80 B =
/// 20 lanes, the exact `DrawG3Mesh` record; `material_params.w == 0.0` is
/// unlit, otherwise Lambert 0.8 + 0.2 ambient) once; draw them every frame
/// with `DrawG3Mesh::draw_retained`.
pub fn g3_publish_meshes(instances: &[MeshInstance]) -> RetainedInstances {
    let mut lanes = Vec::with_capacity(instances.len() * G3_MESH_LANES);
    for m in instances {
        lanes.extend_from_slice(&m.world_col0);
        lanes.extend_from_slice(&m.world_col1);
        lanes.extend_from_slice(&m.world_col2);
        lanes.extend_from_slice(&m.color);
        lanes.extend_from_slice(&m.material_params);
    }
    RetainedInstances::new(G3_MESH_LANES, lanes.into()).expect("whole mesh records")
}

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Mesh {
    #[deref]
    pub draw_vars: DrawVars,
    /// matrix x axis, translation.x
    #[live]
    pub col0: Vec4f,
    /// matrix y axis, translation.y
    #[live]
    pub col1: Vec4f,
    /// matrix z axis, translation.z
    #[live]
    pub col2: Vec4f,
    #[live]
    pub color: Vec4f,
    #[live]
    pub params: Vec4f,
}

impl DrawG3Mesh {
    /// One unit mesh's published instances (faces, quads or cubes) as one
    /// draw call. Nothing is uploaded unless the publication is new.
    pub fn draw_retained(&mut self, cx: &mut CxDraw, geometry_id: GeometryId, instances: &RetainedInstances) {
        self.draw_vars.geometry_id = Some(geometry_id);
        self.draw_vars.set_uniform(cx, live_id!(u_bias_k), &[G3_MESH_BIAS_K]);
        g3_draw_retained(cx, &mut self.draw_vars, instances);
    }
}

/// glyph3d's unit meshes (mesh.rs `QUAD_VERTS`/`CUBE_VERTS`), packed into
/// the 12-float `geom.CubeVertex` layout
/// `[px, py, pz, id, nx, ny, nz, 0, u, v, 0, 0]` (`id` = the cube face).
pub struct G3MeshGeometries {
    pub quad: Geometry,
    pub cube: Geometry,
}

/// (position, normal, uv), as mesh.rs declares them.
type G3MeshVertex = ([f32; 3], [f32; 3], [f32; 2]);

const G3_QUAD_VERTS: [G3MeshVertex; 4] = [
    ([-0.5, 0.5, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
    ([-0.5, -0.5, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
    ([0.5, -0.5, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
    ([0.5, 0.5, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
];
const G3_QUAD_INDICES: [u32; 6] = [0, 1, 2, 0, 2, 3];

const G3_CUBE_VERTS: [G3MeshVertex; 24] = [
    // +Z Front
    ([-0.5, 0.5, 0.5], [0.0, 0.0, 1.0], [0.0, 0.0]),
    ([-0.5, -0.5, 0.5], [0.0, 0.0, 1.0], [0.0, 1.0]),
    ([0.5, -0.5, 0.5], [0.0, 0.0, 1.0], [1.0, 1.0]),
    ([0.5, 0.5, 0.5], [0.0, 0.0, 1.0], [1.0, 0.0]),
    // -Z Back
    ([0.5, 0.5, -0.5], [0.0, 0.0, -1.0], [0.0, 0.0]),
    ([0.5, -0.5, -0.5], [0.0, 0.0, -1.0], [0.0, 1.0]),
    ([-0.5, -0.5, -0.5], [0.0, 0.0, -1.0], [1.0, 1.0]),
    ([-0.5, 0.5, -0.5], [0.0, 0.0, -1.0], [1.0, 0.0]),
    // -X Left
    ([-0.5, 0.5, -0.5], [-1.0, 0.0, 0.0], [0.0, 0.0]),
    ([-0.5, -0.5, -0.5], [-1.0, 0.0, 0.0], [0.0, 1.0]),
    ([-0.5, -0.5, 0.5], [-1.0, 0.0, 0.0], [1.0, 1.0]),
    ([-0.5, 0.5, 0.5], [-1.0, 0.0, 0.0], [1.0, 0.0]),
    // +X Right
    ([0.5, 0.5, 0.5], [1.0, 0.0, 0.0], [0.0, 0.0]),
    ([0.5, -0.5, 0.5], [1.0, 0.0, 0.0], [0.0, 1.0]),
    ([0.5, -0.5, -0.5], [1.0, 0.0, 0.0], [1.0, 1.0]),
    ([0.5, 0.5, -0.5], [1.0, 0.0, 0.0], [1.0, 0.0]),
    // +Y Top
    ([-0.5, 0.5, -0.5], [0.0, 1.0, 0.0], [0.0, 0.0]),
    ([-0.5, 0.5, 0.5], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ([0.5, 0.5, 0.5], [0.0, 1.0, 0.0], [1.0, 1.0]),
    ([0.5, 0.5, -0.5], [0.0, 1.0, 0.0], [1.0, 0.0]),
    // -Y Bottom
    ([-0.5, -0.5, 0.5], [0.0, -1.0, 0.0], [0.0, 0.0]),
    ([-0.5, -0.5, -0.5], [0.0, -1.0, 0.0], [0.0, 1.0]),
    ([0.5, -0.5, -0.5], [0.0, -1.0, 0.0], [1.0, 1.0]),
    ([0.5, -0.5, 0.5], [0.0, -1.0, 0.0], [1.0, 0.0]),
];
const G3_CUBE_INDICES: [u32; 36] = [
    0, 1, 2, 0, 2, 3, // Front
    4, 5, 6, 4, 6, 7, // Back
    8, 9, 10, 8, 10, 11, // Left
    12, 13, 14, 12, 14, 15, // Right
    16, 17, 18, 16, 18, 19, // Top
    20, 21, 22, 20, 22, 23, // Bottom
];

fn g3_pack_cube_vertices(verts: &[G3MeshVertex]) -> Vec<f32> {
    let mut out = Vec::with_capacity(verts.len() * 12);
    for (i, (p, n, uv)) in verts.iter().enumerate() {
        let face = (i / 4) as f32;
        out.extend_from_slice(&[p[0], p[1], p[2], face, n[0], n[1], n[2], 0.0, uv[0], uv[1], 0.0, 0.0]);
    }
    out
}

impl G3MeshGeometries {
    pub fn new(cx: &mut Cx) -> Self {
        let quad = Geometry::new(cx);
        quad.update(cx, G3_QUAD_INDICES.to_vec(), g3_pack_cube_vertices(&G3_QUAD_VERTS));
        let cube = Geometry::new(cx);
        cube.update(cx, G3_CUBE_INDICES.to_vec(), g3_pack_cube_vertices(&G3_CUBE_VERTS));
        Self { quad, cube }
    }
}

/// One library page face, static: its size and gap in its sheet's frame and
/// the sheet's group row. 8 lanes. Only valid once sheets have group rows
/// of their own (see `DrawG3GroupFace`); until then leave `group_faces`
/// empty and use `faces`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct G3GroupFaceSpec {
    pub size: [f32; 2],
    pub gap: f32,
    /// The sheet's group row (an exact integer).
    pub group: f32,
    pub color: [f32; 4],
}

/// Lanes per group-face record: `spec` (size.xy, gap, group) + `color`.
pub const G3_GROUP_FACE_LANES: usize = 8;

impl G3GroupFaceSpec {
    /// Publish specs once; they follow their sheets with no further upload.
    pub fn publish(specs: &[G3GroupFaceSpec]) -> RetainedInstances {
        let mut lanes = Vec::with_capacity(specs.len() * G3_GROUP_FACE_LANES);
        for s in specs {
            lanes.extend_from_slice(&[s.size[0], s.size[1], s.gap, s.group]);
            lanes.extend_from_slice(&s.color);
        }
        RetainedInstances::new(G3_GROUP_FACE_LANES, lanes.into()).expect("whole face records")
    }
}

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3GroupFace {
    #[deref]
    pub draw_vars: DrawVars,
    /// size.x, size.y, gap, group
    #[live]
    pub spec: Vec4f,
    #[live]
    pub color: Vec4f,
}

impl DrawG3GroupFace {
    /// Faces drawn from static specs; the sheet transforms come from the
    /// group table on the GPU.
    pub fn draw_retained(&mut self, cx: &mut CxDraw, geometry_id: GeometryId, tables: &G3Tables, specs: &RetainedInstances) {
        self.draw_vars.geometry_id = Some(geometry_id);
        g3_set_texture(cx, &mut self.draw_vars, live_id!(t_groups), &tables.groups);
        let inv = g3_inv_size(&tables.groups, cx);
        self.draw_vars.set_uniform(cx, live_id!(u_groups_inv_size), &inv);
        self.draw_vars.set_uniform(cx, live_id!(u_max_groups), &[tables.group_count as f32]);
        self.draw_vars.set_uniform(cx, live_id!(u_bias_k), &[G3_MESH_BIAS_K]);
        g3_draw_retained(cx, &mut self.draw_vars, specs);
    }
}

// ---------------------------------------------------------------------------
// Pass 4: regions
// ---------------------------------------------------------------------------

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Region {
    #[deref]
    pub draw_vars: DrawVars,
    #[live]
    pub col0: Vec4f,
    #[live]
    pub col1: Vec4f,
    #[live]
    pub col2: Vec4f,
    #[live]
    pub color: Vec4f,
    /// 1.0 on the picked region (instance lanes stay exact in the fragment
    /// stage, so no instance_index compare is needed).
    #[live]
    pub picked_flag: f32,
}

/// Lanes per region record: 4 × vec4 + `picked_flag`.
pub const G3_REGION_LANES: usize = 17;

impl DrawG3Region {
    /// Every region box as one draw call; `picked` is a region index.
    pub fn draw_all(
        &mut self,
        cx: &mut CxDraw,
        params: &RegionParams,
        regions: &[RegionInstance],
        picked: Option<usize>,
        scratch: &mut Vec<f32>,
    ) {
        scratch.clear();
        scratch.reserve(regions.len() * G3_REGION_LANES);
        for (k, r) in regions.iter().enumerate() {
            scratch.extend_from_slice(&r.col0);
            scratch.extend_from_slice(&r.col1);
            scratch.extend_from_slice(&r.col2);
            scratch.extend_from_slice(&r.color);
            scratch.push(if picked == Some(k) { 1.0 } else { 0.0 });
        }
        self.draw_vars.set_uniform(
            cx,
            live_id!(u_region_params),
            &[params.edge_opacity, params.edge_px, params.picked_opacity, params.picked_edge_opacity],
        );
        g3_draw_stream(cx, &mut self.draw_vars, scratch);
    }
}

// ---------------------------------------------------------------------------
// Pass 5: glyphs — tables, params and the retained blocks
// ---------------------------------------------------------------------------

/// The emoji sheet: `layers` BGRA8 images (`b | g << 8 | r << 16 | a << 24`),
/// each with its mip chain (levels 0..=max_level concatenated), layer-major.
pub struct G3EmojiSheet {
    pub width: usize,
    pub height: usize,
    pub layers: usize,
    pub max_level: usize,
    pub bgra: Vec<u32>,
}

/// Process-static atlas tables (`FrameData::atlas`), uploaded once per
/// `atlas_generation` and kept across scene relayouts and override edits.
pub struct G3AtlasTables {
    pub glyphmap: FloatTexture,
    pub curves: FloatTexture,
    pub advances: FloatTexture,
    pub emoji: G3EmojiSheet,
}

fn g3_float_texture_vec(cx: &mut Cx, name: &str, width: u32, w: u32, h: u32, channels: u32, vec: Vec<f32>) -> Texture {
    assert_eq!(w, width, "{name}: the shader addresses a {width}-wide table");
    let (w, h) = (w as usize, h as usize);
    assert_eq!(vec.len(), w * h * channels as usize, "{name}: payload is not {w}x{h}x{channels}");
    let data = Some(vec);
    let updated = TextureUpdated::Full;
    let format = match channels {
        4 => TextureFormat::VecRGBAf32 { width: w, height: h, data, updated },
        1 => TextureFormat::VecRf32 { width: w, height: h, data, updated },
        c => panic!("{name}: {c}-channel float tables have no texture format"),
    };
    Texture::new_with_format(cx, format)
}

fn g3_float_texture(cx: &mut Cx, name: &str, width: u32, table: &FloatTexture) -> Texture {
    g3_float_texture_vec(cx, name, width, table.width, table.height, table.channels, table.data.to_vec())
}

fn g3_float_texture_owned(cx: &mut Cx, name: &str, width: u32, table: FloatTexture) -> Texture {
    g3_float_texture_vec(cx, name, width, table.width, table.height, table.channels, table.data)
}

fn g3_update_float_texture(cx: &mut Cx, tex: &Texture, name: &str, expected_width: u32, table: &FloatTexture) {
    assert_eq!(table.width, expected_width, "{name}: the shader addresses a {expected_width}-wide table");
    let (w, h) = (table.width as usize, table.height as usize);
    assert_eq!(table.data.len(), w * h * table.channels as usize, "{name}: payload is not {w}x{h}x{}", table.channels);
    match (table.channels, tex.get_format(cx)) {
        (4, TextureFormat::VecRGBAf32 { width, height, data, updated })
        | (1, TextureFormat::VecRf32 { width, height, data, updated }) => {
            *width = w;
            *height = h;
            let buf = data.get_or_insert_with(Vec::new);
            buf.clear();
            buf.extend_from_slice(&table.data);
            *updated = TextureUpdated::Full;
        }
        (c, _) => panic!("{name}: {c}-channel float table does not match existing texture format"),
    }
}

/// glyph3d's GPU tables as Makepad lookup textures, read with
/// `sample_nearest` at texel centres. One set per field; shared by the
/// glyph, mask and group-face shaders.
pub struct G3Tables {
    /// GroupRow (96 B): 6 RGBA texels per group, 1020 wide.
    pub groups: Texture,
    groups_size: (u32, u32),
    pub group_count: u32,
    /// ItemParamsGpu (64 B): 4 RGBA texels per item, 1024 wide.
    pub items: Texture,
    /// group_overrides (R), 1024 wide.
    pub overrides: Texture,
    /// glyph_advances (R), 1024 wide.
    pub advances: Texture,
    /// The glyph map: one texel per slot, NO_CELL → -1.0.
    pub glyphmap: Texture,
    /// Curves: two texels per quadratic, u16 coords as exact floats.
    pub curves: Texture,
    /// The emoji sheet (sRGB-encoded BGRA8 layers, mips).
    pub emoji: Texture,
}

/// The scene tables that change on scene load, relayout or override edit.
#[derive(Clone, Copy)]
pub struct G3StaticTables<'a> {
    pub items: &'a FloatTexture,
    pub overrides: &'a FloatTexture,
}

/// The group table and what changed in it since the renderer last saw it.
#[derive(Clone, Copy)]
pub struct G3GroupTable<'a> {
    pub table: &'a FloatTexture,
    /// Rows the field has (`u_max_groups`; an out-of-range group culls).
    pub group_count: u32,
    /// Rectangles written since the generation the renderer last saw
    /// (`write_group_rows`' results, accumulated). Empty: upload it all.
    pub dirty: &'a [TexelRect],
}

impl G3Tables {
    pub fn new(cx: &mut Cx, atlas: G3AtlasTables, statics: &G3StaticTables, groups: &G3GroupTable) -> Self {
        let g = groups.table;
        Self {
            groups: g3_float_texture(cx, "groups", GROUP_TABLE_WIDTH, g),
            groups_size: (g.width, g.height),
            group_count: groups.group_count,
            items: g3_float_texture(cx, "item_table", TABLE_WIDTH, statics.items),
            overrides: g3_float_texture(cx, "group_overrides", TABLE_WIDTH, statics.overrides),
            advances: g3_float_texture_owned(cx, "glyph_advances", TABLE_WIDTH, atlas.advances),
            glyphmap: g3_float_texture_owned(cx, "glyphmap", TABLE_WIDTH, atlas.glyphmap),
            curves: g3_float_texture_owned(cx, "curves", TABLE_WIDTH, atlas.curves),
            emoji: Self::emoji_texture(cx, atlas.emoji),
        }
    }

    pub fn update_scene(&mut self, cx: &mut Cx, statics: &G3StaticTables, groups: &G3GroupTable) {
        let g = groups.table;
        g3_update_float_texture(cx, &self.groups, "groups", GROUP_TABLE_WIDTH, g);
        self.groups_size = (g.width, g.height);
        self.group_count = groups.group_count;
        g3_update_float_texture(cx, &self.items, "item_table", TABLE_WIDTH, statics.items);
        g3_update_float_texture(cx, &self.overrides, "group_overrides", TABLE_WIDTH, statics.overrides);
    }

    fn emoji_texture(cx: &mut Cx, sheet: G3EmojiSheet) -> Texture {
        Texture::new_with_format(
            cx,
            TextureFormat::VecArrayMipBGRAu8_32 {
                width: sheet.width,
                height: sheet.height,
                layers: sheet.layers,
                data: Some(sheet.bgra),
                max_level: Some(sheet.max_level),
                wrap: TextureWrap::ClampToEdge,
                updated: TextureUpdated::Full,
            },
        )
    }

    /// Bring the group texture up to `groups`: a resized table updates the
    /// handle's dimensions in place; otherwise only the dirty rectangles are
    /// copied and uploaded (all of it when none are named).
    pub fn update_groups(&mut self, cx: &mut Cx, groups: &G3GroupTable) {
        let g = groups.table;
        self.group_count = groups.group_count;
        if (g.width, g.height) != self.groups_size {
            g3_update_float_texture(cx, &self.groups, "groups", GROUP_TABLE_WIDTH, g);
            self.groups_size = (g.width, g.height);
            return;
        }
        let mut data = self.groups.take_vec_f32(cx);
        let row = g.width as usize * 4;
        if groups.dirty.is_empty() {
            data.copy_from_slice(&g.data);
            self.groups.put_back_vec_f32(cx, data, None);
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for r in groups.dirty {
            for y in r.y..r.y + r.height {
                let span = y as usize * row + r.x as usize * 4..y as usize * row + (r.x + r.width) as usize * 4;
                data[span.clone()].copy_from_slice(&g.data[span]);
            }
            (x0, y0) = (x0.min(r.x), y0.min(r.y));
            (x1, y1) = (x1.max(r.x + r.width), y1.max(r.y + r.height));
        }
        let dirty = RectUsize::new(
            PointUsize::new(x0 as usize, y0 as usize),
            SizeUsize::new((x1 - x0) as usize, (y1 - y0) as usize),
        );
        self.groups.put_back_vec_f32(cx, data, Some(dirty));
    }

    /// Bind every table to a glyph-family shader and set its layout uniforms.
    fn bind(&self, cx: &Cx, dv: &mut DrawVars) {
        g3_set_texture(cx, dv, live_id!(t_groups), &self.groups);
        g3_set_texture(cx, dv, live_id!(t_items), &self.items);
        g3_set_texture(cx, dv, live_id!(t_overrides), &self.overrides);
        g3_set_texture(cx, dv, live_id!(t_advances), &self.advances);
        g3_set_texture(cx, dv, live_id!(t_glyphmap), &self.glyphmap);
        g3_set_texture(cx, dv, live_id!(t_curves), &self.curves);
        g3_set_texture(cx, dv, live_id!(t_emoji), &self.emoji);
        dv.set_uniform(cx, live_id!(u_groups_inv_size), &g3_inv_size(&self.groups, cx));
        dv.set_uniform(cx, live_id!(u_items_inv_size), &g3_inv_size(&self.items, cx));
        dv.set_uniform(cx, live_id!(u_overrides_inv_size), &g3_inv_size(&self.overrides, cx));
        dv.set_uniform(cx, live_id!(u_advances_inv_size), &g3_inv_size(&self.advances, cx));
        dv.set_uniform(cx, live_id!(u_glyphmap_inv_size), &g3_inv_size(&self.glyphmap, cx));
        dv.set_uniform(cx, live_id!(u_curves_inv_size), &g3_inv_size(&self.curves, cx));
        dv.set_uniform(cx, live_id!(u_max_groups), &[self.group_count as f32]);
    }
}

fn g3_set_glyph_params(cx: &Cx, dv: &mut DrawVars, p: &Params) {
    dv.set_uniform(cx, live_id!(u_greek_mode), &[p.greek_mode as f32]);
    dv.set_uniform(cx, live_id!(u_dilate_px), &[p.dilate_px]);
    dv.set_uniform(cx, live_id!(u_soften), &[p.soften]);
    dv.set_uniform(cx, live_id!(u_min_lo), &[p.min_lo]);
    dv.set_uniform(cx, live_id!(u_min_hi), &[p.min_hi]);
    dv.set_uniform(cx, live_id!(u_emoji_cell), &[p.emoji_cell[0] as f32, p.emoji_cell[1] as f32]);
    dv.set_uniform(cx, live_id!(u_emoji_cols), &[p.emoji_cols as f32]);
    dv.set_uniform(cx, live_id!(u_emoji_rows), &[p.emoji_rows as f32]);
    dv.set_uniform(cx, live_id!(u_emoji_layer), &p.emoji_layer);
    dv.set_uniform(cx, live_id!(u_greek_onset_px), &[p.greek_onset_px]);
}

/// The glyph shader. Its record is DerivedSlot, script-declared, so the
/// Rust struct carries no instance lanes.
#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Glyph {
    #[deref]
    pub draw_vars: DrawVars,
}

impl DrawG3Glyph {
    /// Tables + params, once per frame before the chunks draw.
    pub fn prepare(&mut self, cx: &Cx, tables: &G3Tables, params: &Params) {
        tables.bind(cx, &mut self.draw_vars);
        g3_set_glyph_params(cx, &mut self.draw_vars, params);
    }
}

/// The selection-mask variant (`@Bgra8NoBlend`, no blend, no depth,
/// `u_mask_mode = 1`). Draws the same chunk publications.
#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3GlyphMask {
    #[deref]
    pub draw_super: DrawG3Glyph,
}

/// One slot block: a DerivedSlot chunk of the Derived field (a fixed slot
/// count, ~256K slots = 5.2 MB), published once as a retained instance
/// resource exactly as the field holds it (no copy, byte-identical), drawn
/// per frame with the CPU cull's segment ranges. One draw item per block,
/// in arena order, so blend order is glyph3d's.
///
/// The native HAL keeps a retained publication's GPU buffer per draw ITEM
/// POSITION, `(draw list, item index)`, and re-uploads it whenever the
/// publication id at that position changes (frame_build.rs `fb_instances`).
/// So a block keeps ONE publication for as long as the host hands in the same
/// `Arc` (`Arc::ptr_eq`: identity is the generation), and every block draws
/// at a stable index in a list of its own (`G3Renderer`).
pub struct G3GlyphBlock {
    source: Arc<[f32]>,
    retained: RetainedInstances,
}

impl G3GlyphBlock {
    /// Wrap the field's block: DerivedSlot words carried as f32 bits
    /// (`f32::from_bits`), 5 a slot. f32 is only the byte carrier: the GPU
    /// reads these lanes as u32 (`instance(u32)`), never as float.
    pub fn new(words_as_f32: &Arc<[f32]>) -> Result<Self, &'static str> {
        Ok(Self {
            source: words_as_f32.clone(),
            retained: RetainedInstances::new(G3_DERIVED_SLOT_WORDS, words_as_f32.clone())?,
        })
    }

    /// Whether this is still the host's block (same allocation).
    pub fn is(&self, words_as_f32: &Arc<[f32]>) -> bool {
        Arc::ptr_eq(&self.source, words_as_f32)
    }

    /// Draw this block's visible `ranges` (slot ranges, ascending) with
    /// `draw_vars` (a `DrawG3Glyph`'s, or a `DrawG3GlyphMask`'s for the
    /// selection). An empty `ranges` draws nothing while the publication
    /// stays resident: it is submitted as `[0..0]`, because an empty range
    /// list means "the whole buffer".
    pub fn draw(&self, cx: &mut CxDraw, draw_vars: &mut DrawVars, ranges: &[Range<u32>]) -> Area {
        let area = cx.add_retained_instances(draw_vars, &self.retained);
        if let Area::Instance(ia) = &area {
            let ranges: &[Range<u32>] = if ranges.is_empty() { std::slice::from_ref(&(0..0)) } else { ranges };
            cx.draw_lists[ia.draw_list_id].draw_items.set_instance_ranges(ia.draw_item_id, ranges);
        }
        draw_vars.area = cx.update_area_refs(draw_vars.area, area);
        area
    }
}

// ---------------------------------------------------------------------------
// Pass 6: composite
// ---------------------------------------------------------------------------

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawG3Composite {
    #[deref]
    pub draw_super: DrawQuad,
}

impl DrawG3Composite {
    /// `scene`: the RenderRGBAf16 scene target; `mask`: the RenderBGRAu8
    /// selection mask, read only when `tint` is Some (rgb, strength).
    pub fn set_inputs(&mut self, cx: &Cx, scene: &Texture, mask: &Texture, tint: Option<[f32; 4]>) {
        let dv = &mut self.draw_super.draw_vars;
        g3_set_texture(cx, dv, live_id!(t_scene), scene);
        g3_set_texture(cx, dv, live_id!(t_mask), mask);
        dv.set_uniform(cx, live_id!(u_has_selection), &[if tint.is_some() { 1.0 } else { 0.0 }]);
        if let Some(tint) = tint {
            dv.set_uniform(cx, live_id!(u_tint), &tint);
        }
    }
}

// ---------------------------------------------------------------------------
// The viewport renderer: one frame from engine-neutral inputs
// ---------------------------------------------------------------------------

/// Every DrawG3* shader a viewport needs, as ONE `#[live]` field
/// (`draws: mod.draw.G3Draws{}` in the widget's DSL).
#[derive(Script, ScriptHook)]
pub struct G3Draws {
    #[live]
    pub sky: DrawG3Sky,
    #[live]
    pub ground: DrawG3Ground,
    #[live]
    pub backdrop: DrawG3Backdrop,
    #[live]
    pub mesh: DrawG3Mesh,
    #[live]
    pub group_face: DrawG3GroupFace,
    #[live]
    pub region: DrawG3Region,
    #[live]
    pub glyph: DrawG3Glyph,
    #[live]
    pub glyph_mask: DrawG3GlyphMask,
    #[live]
    pub composite: DrawG3Composite,
}

/// The selection: per-block slot ranges into the mask pass, and the tint
/// (rgb, strength) the composite adds where the mask covers.
#[derive(Clone, Copy)]
pub struct G3Selection<'a> {
    pub chunk_ranges: &'a [&'a [Range<u32>]],
    pub tint: [f32; 4],
}

/// One frame's inputs, split by rate. Each slow group carries a generation:
/// the renderer re-uploads that group only when its generation differs from
/// the one it last consumed (`G3Renderer::seen`), so a host may pass the
/// same borrowed data every frame at no cost. Glyph blocks need no
/// generation: a block's identity (`Arc::ptr_eq`) is its generation.
pub struct G3FrameInputs<'a> {
    // ── once per atlas ──
    pub atlas_generation: u64,
    pub atlas: Option<G3AtlasTables>,
    // ── on load / on relayout / on override edit ──
    pub tables_generation: u64,
    pub tables: &'a G3StaticTables<'a>,
    /// The Derived field's chunks, in arena order: DerivedSlot words as f32
    /// bits, 5 a slot, a fixed slot count each. A block whose `Arc` is the
    /// one already held is not re-published; a new `Arc` at index k
    /// re-uploads block k only. Appending blocks never moves earlier ones.
    pub blocks: &'a [Arc<[f32]>],
    /// Library faces, scene quads and cubes (MeshInstance), and the static
    /// group-composed face specs (empty until sheets have group rows of
    /// their own; see `DrawG3GroupFace`).
    pub meshes_generation: u64,
    pub faces: &'a [MeshInstance],
    pub quads: &'a [MeshInstance],
    pub cubes: &'a [MeshInstance],
    pub group_faces: &'a [G3GroupFaceSpec],
    // ── on change ──
    pub groups_generation: u64,
    pub groups: G3GroupTable<'a>,
    // ── every frame ──
    /// glyph3d's camera: view_proj, column-major (reverse-Z when `reverse_z`).
    pub view_proj: [f32; 16],
    /// Reverse-Z: depth clears to 0.0 and tests GREATER_OR_EQUAL.
    pub reverse_z: bool,
    pub clear_color: [f32; 4],
    /// The environment; None = off (draws nothing, as in glyph3d).
    pub env: Option<EnvUniform>,
    pub glyph_params: Params,
    pub regions: &'a [RegionInstance],
    pub region_params: RegionParams,
    pub picked_region: Option<usize>,
    /// The CPU cull's compacted backdrop survivors.
    pub backdrops: &'a [BackdropInst],
    /// The CPU cull's visible slot ranges per block (chunk); a block past
    /// the end of this list draws nothing.
    pub chunk_ranges: &'a [&'a [Range<u32>]],
    pub selection: Option<G3Selection<'a>>,
}

/// The generations a renderer has consumed; a host may drop its pending
/// group dirty rects once `groups` reaches the generation they belong to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct G3Seen {
    pub atlas: Option<u64>,
    pub tables: Option<u64>,
    pub groups: Option<u64>,
    pub meshes: Option<u64>,
}

struct G3MeshPublications {
    faces: RetainedInstances,
    quads: RetainedInstances,
    cubes: RetainedInstances,
    group_faces: RetainedInstances,
}

/// One retained phase's own draw list. A retained publication's GPU buffer
/// is keyed by its draw item's position (see `G3GlyphBlock`), so every
/// retained draw sits in a list where nothing optional precedes it: an env
/// toggling or a backdrop list going empty can never shift it and re-upload.
struct G3PhaseList(DrawList);

impl G3PhaseList {
    fn new(cx: &mut Cx) -> Self {
        Self(DrawList::new(cx))
    }

    /// Record `f`'s draws as a child of the current list, at this point in
    /// draw order (a child list sits where it is begun).
    fn draw(&mut self, cx: &mut CxDraw, f: impl FnOnce(&mut CxDraw)) {
        self.0.begin_always(cx);
        f(cx);
        self.0.end(cx);
    }
}

/// Owns the scene pass (RGBA16F + Depth32), the selection mask pass
/// (BGRA8), the tables, the block and mesh publications; draws one frame in
/// glyph3d's phase order and composites it into a rect.
///
/// RESIDENCY (2026-10-26): a block is published only while the cull draws
/// it (or the selection covers it), within makepad's publication envelope; which
/// blocks are published and evicted each frame is
/// [`crate::residency::BlockResidency`]'s decision. A needed block not yet
/// published draws nothing until it is. Evicting a block drops its lists,
/// which makepad retires with their buffers. A
/// tree past the device (1.29B slots, 24 GiB, against a 7 GiB envelope) is
/// drawn from the host blocks a view at a time. Selecting glyphs costs one
/// more upload per selected block (its mask copy, in the block's own mask
/// list).
pub struct G3Renderer {
    pass: DrawPass,
    mask_pass: DrawPass,
    draw_list: DrawList,
    mask_list: DrawList,
    faces_list: G3PhaseList,
    group_faces_list: G3PhaseList,
    quads_list: G3PhaseList,
    cubes_list: G3PhaseList,
    glyph_list: G3PhaseList,
    color: Texture,
    depth: Texture,
    mask: Texture,
    geoms: G3MeshGeometries,
    tables: Option<G3Tables>,
    /// Per block index, the block while it is resident.
    blocks: Vec<Option<G3ResidentBlock>>,
    residency: BlockResidency,
    /// The device bytes the resident blocks may total: makepad's
    /// publication envelope, as the host read it at startup.
    envelope: usize,
    /// The last frame's needed blocks that did not fit the envelope, logged
    /// when it changes.
    unfit: usize,
    meshes: Option<G3MeshPublications>,
    scratch: Vec<f32>,
    seen: G3Seen,
}

/// A resident block: its publication, drawn at index 0 of a list of its
/// own (so it stays put whichever other blocks are drawn or evicted), and
/// its selection mask's list.
struct G3ResidentBlock {
    block: G3GlyphBlock,
    list: G3PhaseList,
    mask: Option<G3PhaseList>,
}

fn g3_set_pass_camera(cx: &mut Cx, pass: &DrawPass, view_proj: &[f32; 16]) {
    let proj = Mat4f { v: *view_proj };
    let identity = Mat4f::identity();
    let u = &mut cx.passes[pass.draw_pass_id()].pass_uniforms;
    u.camera_projection = proj;
    u.camera_projection_r = proj;
    u.depth_projection = proj;
    u.depth_projection_r = proj;
    u.camera_view = identity;
    u.camera_view_r = identity;
    u.depth_view = identity;
    u.depth_view_r = identity;
    u.camera_inv = identity;
    u.camera_inv_r = identity;
}

impl G3Renderer {
    /// A renderer whose glyph blocks are resident within `envelope`
    /// device bytes (makepad's publication envelope; makepad leaves keeping
    /// under it to the producer).
    pub fn new(cx: &mut Cx, envelope: usize) -> Self {
        log!("g3: glyph block envelope {} MiB (makepad's), {} MiB published a frame", envelope >> 20, UPLOAD_PER_FRAME >> 20);
        Self {
            pass: DrawPass::new(cx),
            mask_pass: DrawPass::new(cx),
            draw_list: DrawList::new(cx),
            mask_list: DrawList::new(cx),
            faces_list: G3PhaseList::new(cx),
            group_faces_list: G3PhaseList::new(cx),
            quads_list: G3PhaseList::new(cx),
            cubes_list: G3PhaseList::new(cx),
            glyph_list: G3PhaseList::new(cx),
            color: Texture::new_with_format(cx, TextureFormat::RenderRGBAf16 { size: TextureSize::Auto, initial: true }),
            depth: Texture::new_with_format(cx, TextureFormat::DepthD32 { size: TextureSize::Auto, initial: true }),
            mask: Texture::new_with_format(cx, TextureFormat::RenderBGRAu8 { size: TextureSize::Auto, initial: true }),
            geoms: G3MeshGeometries::new(cx),
            tables: None,
            blocks: Vec::new(),
            residency: BlockResidency::new(UPLOAD_PER_FRAME),
            envelope,
            unfit: 0,
            meshes: None,
            scratch: Vec::new(),
            seen: G3Seen::default(),
        }
    }

    /// The generations consumed so far.
    pub fn seen(&self) -> G3Seen {
        self.seen
    }

    fn sync(&mut self, cx: &mut Cx, inputs: &mut G3FrameInputs) {
        if self.seen.atlas != Some(inputs.atlas_generation) || self.tables.is_none() {
            let atlas = inputs.atlas.take().expect("new atlas_generation requires G3FrameInputs::atlas");
            self.tables = Some(G3Tables::new(cx, atlas, inputs.tables, &inputs.groups));
            self.seen.atlas = Some(inputs.atlas_generation);
            self.seen.tables = Some(inputs.tables_generation);
            self.seen.groups = Some(inputs.groups_generation);
        } else if self.seen.tables != Some(inputs.tables_generation) {
            if let Some(tables) = &mut self.tables {
                tables.update_scene(cx, inputs.tables, &inputs.groups);
            }
            self.seen.tables = Some(inputs.tables_generation);
            self.seen.groups = Some(inputs.groups_generation);
        } else if self.seen.groups != Some(inputs.groups_generation) {
            if let Some(tables) = &mut self.tables {
                tables.update_groups(cx, &inputs.groups);
            }
            self.seen.groups = Some(inputs.groups_generation);
        }
        self.sync_blocks(cx, inputs);
        if self.seen.meshes != Some(inputs.meshes_generation) || self.meshes.is_none() {
            self.meshes = Some(G3MeshPublications {
                faces: g3_publish_meshes(inputs.faces),
                quads: g3_publish_meshes(inputs.quads),
                cubes: g3_publish_meshes(inputs.cubes),
                group_faces: G3GroupFaceSpec::publish(inputs.group_faces),
            });
            self.seen.meshes = Some(inputs.meshes_generation);
        }
    }

    /// Publish what the frame needs of the blocks, by identity and within
    /// the residency (see [`G3Renderer`]). A held block whose `Arc` is
    /// still the host's is not re-published; a new `Arc` (an edit)
    /// re-publishes that block only. One log line per sync that published
    /// or evicted any, so an edit's upload is visible: it should name one
    /// block, not all.
    fn sync_blocks(&mut self, cx: &mut Cx, inputs: &G3FrameInputs) {
        let selected = |k: usize| inputs.selection.and_then(|s| s.chunk_ranges.get(k)).is_some_and(|r| !r.is_empty());
        let needed: Vec<usize> = (0..inputs.blocks.len())
            .filter(|&k| inputs.chunk_ranges.get(k).is_some_and(|r| !r.is_empty()) || selected(k))
            .collect();
        let envelope = self.envelope;
        let plan = self.residency.plan(envelope, inputs.blocks.len(), |k| inputs.blocks[k].len() * 4, &needed);
        for &k in &plan.evict {
            self.blocks[k] = None;
        }
        self.blocks.resize_with(inputs.blocks.len(), || None);
        let mut republished = Vec::new();
        for &k in &needed {
            if let Some(held) = &mut self.blocks[k] {
                if !held.block.is(&inputs.blocks[k]) {
                    held.block = G3GlyphBlock::new(&inputs.blocks[k]).expect("a DerivedSlot block is 5 words a slot");
                    republished.push(k);
                }
            }
        }
        for &k in &plan.publish {
            self.blocks[k] = Some(G3ResidentBlock {
                block: G3GlyphBlock::new(&inputs.blocks[k]).expect("a DerivedSlot block is 5 words a slot"),
                list: G3PhaseList::new(cx),
                mask: None,
            });
        }
        let (published, evicted, unfit) = (&plan.publish, plan.evict.len(), plan.unfit);
        if !(republished.is_empty() && published.is_empty() && evicted == 0) {
            let shown: Vec<usize> = republished.iter().chain(published).copied().take(8).collect();
            let (resident, bytes) = self.residency.resident();
            log!(
                "g3: republished {} and published {} glyph blocks {:?}{}, evicted {}; {} of {} resident ({} MiB), {} needed",
                republished.len(),
                published.len(),
                shown,
                if republished.len() + published.len() > shown.len() { " ..." } else { "" },
                evicted,
                resident,
                self.blocks.len(),
                bytes >> 20,
                needed.len(),
            );
        }
        if unfit != self.unfit {
            if unfit > 0 {
                log!(
                    "g3: the view needs {} glyph blocks, {} past the {} MiB envelope: they draw nothing",
                    needed.len(),
                    unfit,
                    envelope >> 20
                );
            }
            self.unfit = unfit;
        }
    }

    /// Render one frame into `rect` (call from the widget's draw_walk). The
    /// pass list follows the inputs: sky and ground only with an env (ground
    /// only from above), the mask pass only with a selection, empty lists
    /// draw nothing. Returns the composite's area.
    pub fn draw_frame(&mut self, cx: &mut Cx2d, draws: &mut G3Draws, rect: Rect, inputs: &mut G3FrameInputs) -> Area {
        self.sync(cx, inputs);
        let (Some(tables), Some(meshes)) = (&self.tables, &self.meshes) else {
            return Area::Empty;
        };
        let reverse_z = inputs.reverse_z;
        let c = inputs.clear_color;

        // ── the scene pass, in glyph3d's phase order ──
        self.pass.set_size(cx, rect.size);
        self.pass.set_color_texture(cx, &self.color, DrawPassClearColor::ClearWith(vec4(c[0], c[1], c[2], c[3])));
        self.pass
            .set_depth_texture(cx, &self.depth, DrawPassClearDepth::ClearWith(if reverse_z { 0.0 } else { 1.0 }));
        self.pass.set_reverse_z(cx, reverse_z);
        self.pass.set_keep_camera_matrix(cx, true);
        cx.make_child_pass(&self.pass);
        cx.begin_pass(&self.pass, None);
        g3_set_pass_camera(cx, &self.pass, &inputs.view_proj);
        {
            let cxd: &mut CxDraw = cx;
            self.draw_list.begin_always(cxd);

            // Streamed phases (re-recorded each frame, position-free) draw
            // straight into the pass list; each retained phase draws into
            // its own child list at its place in the order.
            if let Some(env) = &inputs.env {
                draws.sky.far_z = if reverse_z { 0.0 } else { 1.0 };
                draws.sky.set_env(cxd, env);
                draws.sky.draw(cxd);
                draws.ground.set_env(cxd, env);
                draws.ground.draw(cxd, env);
            }
            draws.backdrop.draw_all(cxd, inputs.backdrops, &mut self.scratch);
            let (quad, cube) = (self.geoms.quad.geometry_id(), self.geoms.cube.geometry_id());
            self.faces_list.draw(cxd, |cx| draws.mesh.draw_retained(cx, quad, &meshes.faces));
            self.group_faces_list.draw(cxd, |cx| draws.group_face.draw_retained(cx, quad, tables, &meshes.group_faces));
            draws.region.draw_all(cxd, &inputs.region_params, inputs.regions, inputs.picked_region, &mut self.scratch);
            self.quads_list.draw(cxd, |cx| draws.mesh.draw_retained(cx, quad, &meshes.quads));
            self.cubes_list.draw(cxd, |cx| draws.mesh.draw_retained(cx, cube, &meshes.cubes));

            // The resident blocks the cull draws, each in its own list, in
            // arena order (glyph3d's blend order).
            draws.glyph.prepare(cxd, tables, &inputs.glyph_params);
            let blocks = &mut self.blocks;
            self.glyph_list.draw(cxd, |cx| {
                for (k, held) in blocks.iter_mut().enumerate() {
                    let Some(held) = held else { continue };
                    let Some(ranges) = inputs.chunk_ranges.get(k).filter(|r| !r.is_empty()) else { continue };
                    let block = &held.block;
                    held.list.draw(cx, |cx| {
                        block.draw(cx, &mut draws.glyph.draw_vars, ranges);
                    });
                }
            });

            self.draw_list.end(cxd);
        }
        cx.end_pass(&self.pass);

        // ── the selection mask: coverage only, no depth ──
        let tint = inputs.selection.map(|s| s.tint);
        if let Some(selection) = &inputs.selection {
            self.mask_pass.set_size(cx, rect.size);
            self.mask_pass.set_color_texture(cx, &self.mask, DrawPassClearColor::ClearWith(vec4(0.0, 0.0, 0.0, 0.0)));
            self.mask_pass.set_keep_camera_matrix(cx, true);
            cx.make_child_pass(&self.mask_pass);
            cx.begin_pass(&self.mask_pass, None);
            g3_set_pass_camera(cx, &self.mask_pass, &inputs.view_proj);
            {
                let cxd: &mut CxDraw = cx;
                self.mask_list.begin_always(cxd);
                let mask = &mut draws.glyph_mask.draw_super;
                mask.prepare(cxd, tables, &inputs.glyph_params);
                for (k, held) in self.blocks.iter_mut().enumerate() {
                    let Some(held) = held else { continue };
                    let Some(ranges) = selection.chunk_ranges.get(k).filter(|r| !r.is_empty()) else {
                        continue;
                    };
                    let block = &held.block;
                    let list = held.mask.get_or_insert_with(|| G3PhaseList::new(cxd));
                    list.draw(cxd, |cx| {
                        block.draw(cx, &mut mask.draw_vars, ranges);
                    });
                }
                self.mask_list.end(cxd);
            }
            cx.end_pass(&self.mask_pass);
        }

        // ── composite into the UI ──
        draws.composite.set_inputs(cx, &self.color, &self.mask, tint);
        draws.composite.draw_super.draw_abs(cx, rect);
        let area = draws.composite.draw_super.area();
        cx.set_pass_area(&self.pass, area);
        if inputs.selection.is_some() {
            cx.set_pass_area(&self.mask_pass, area);
        }
        area
    }
}
