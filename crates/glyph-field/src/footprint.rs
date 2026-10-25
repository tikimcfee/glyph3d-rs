//! Where the glyph shaders' antialiasing footprint comes from (C35,
//! 2026-10-24).
//!
//! Coverage, dilation and Greeking all read `fw`: how much of the glyph's
//! `[0,1]²` cell one pixel spans (`let fw = fwidth(in.glyph_uv);` in both
//! glyph shaders). A hardware derivative is a difference between the lanes of
//! a 2x2 quad. On Apple's TBDR GPUs that difference is NOT a pure function of
//! the fragment: the glyph pass blends, writes depth and may discard, and a
//! quad-mate lane hidden behind a nearer glyph is sometimes rejected before
//! shading and sometimes after, depending on GPU timing. The winning
//! fragment's `fw` moves with it, so its coverage does too. Measured on an
//! M2: the set of fragments that pass the depth test is identical every
//! frame, and the colour flickers at delta 1. The "early state" the golden
//! captured at frame 0 is gone under the analytic footprint too, which late-Z
//! did not remove; what switches the M2 out of it over wall-clock time is
//! untraced (`out/APPLE-TBDR-FOOTPRINT-2026-10-24.md`).
//!
//! [`Footprint::Analytic`] removes the quad from the arithmetic. The vertex
//! stage already knows the glyph quad's projective map: corner `(u, v)` lands
//! at `clip = C0 + u·Cu + v·Cv`. It hands the fragment stage those three
//! vectors in pixel units, as flat varyings. The fragment rebuilds its own
//! `clip` from its interpolated `glyph_uv` and inverts the 2x2 Jacobian of
//! pixel position with respect to `(u, v)`. That gives `fw`, and the emoji
//! sheet's sampling gradients, from the fragment's own inputs alone.
//!
//! Hardware stays the default. Every adapter that is not Apple-on-Metal
//! (`GpuProfile`) compiles the shader source byte for byte as it always was:
//! no extra varyings, no extra arithmetic, no moved pixel. That is why the
//! analytic form is DERIVED from that source here, by anchored substitution,
//! and not written as a second copy of the shader. There is one coverage body
//! and two derivative sources, and the hardware source is untouched. Every
//! anchor must match exactly once, or this panics. The per-mode `wgsl` tests
//! compile both forms, so an edit that moves an anchor fails in cargo-test
//! rather than at pipeline creation.

/// The source of the glyph shaders' AA footprint (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Footprint {
    /// `fwidth(glyph_uv)` and implicit-gradient `textureSample`: the shader
    /// source as written.
    #[default]
    Hardware,
    /// The Jacobian of the quad's projective map, from flat varyings.
    Analytic,
}

/// One substitution: `from` must occur exactly once in the source.
struct Edit {
    what: &'static str,
    from: &'static str,
    to: String,
}

/// The analytic-footprint form of a glyph shader (module docs). The only
/// per-mode input is `quad_height`, the WGSL expression for the quad's local
/// height that the vertex stage multiplies `(c.y - 0.5)` by: `"1.0"` in
/// Derived, `"inst.height"` in Instanced.
pub fn analytic_source(src: &str, quad_height: &str) -> String {
    let edits = [
        Edit {
            what: "the camera block reads the viewport (FrameUniform's layout up to it)",
            from: "struct Camera {\n    view_proj: mat4x4<f32>,\n};",
            to: "struct Camera {\n    view_proj: mat4x4<f32>,\n    eye: vec3<f32>,\n    _pad0: f32,\n    viewport: vec2<f32>,\n};"
                .to_string(),
        },
        Edit {
            what: "three flat varyings: the quad's projective map in pixel units",
            from: "    @location(9) bg_color: vec4<f32>,\n};",
            to: "    @location(9) bg_color: vec4<f32>,\n    @location(10) @interpolate(flat) foot_p0: vec3<f32>,\n    \
                 @location(11) @interpolate(flat) foot_pu: vec3<f32>,\n    @location(12) @interpolate(flat) foot_pv: vec3<f32>,\n};"
                .to_string(),
        },
        Edit {
            what: "the corner's clip position, kept before the vertex culls overwrite it",
            from: "    var clip = camera.view_proj * vec4<f32>(posed + gpos.xyz, 1.0);\n",
            to: "    var clip = camera.view_proj * vec4<f32>(posed + gpos.xyz, 1.0);\n    let foot_clip = clip;\n".to_string(),
        },
        Edit {
            what: "the vertex stage writes the map",
            from: "    out.bg_color = gbg;\n    return out;\n}",
            to: format!(
                "    out.bg_color = gbg;\n\
                 \x20   // Analytic footprint (glyph-field footprint.rs): d(world)/du and d(world)/dv are\n\
                 \x20   // the quad's edges through the group's scale and rotation; C0 is corner (0,0).\n\
                 \x20   let foot_eu = vec3<f32>(quad_w, 0.0, 0.0) * gscale.xyz;\n\
                 \x20   let foot_ev = vec3<f32>(0.0, {quad_height}, 0.0) * gscale.xyz;\n\
                 \x20   let foot_qu = cross(gquat.xyz, foot_eu) + foot_eu * gquat.w;\n\
                 \x20   let foot_qv = cross(gquat.xyz, foot_ev) + foot_ev * gquat.w;\n\
                 \x20   let foot_cu = camera.view_proj * vec4<f32>(foot_eu + 2.0 * cross(gquat.xyz, foot_qu), 0.0);\n\
                 \x20   let foot_cv = camera.view_proj * vec4<f32>(foot_ev + 2.0 * cross(gquat.xyz, foot_qv), 0.0);\n\
                 \x20   let foot_c0 = foot_clip - c.x * foot_cu - c.y * foot_cv;\n\
                 \x20   // NDC to pixels: x' = x * w/2, y' = -y * h/2 (offsets drop out of a derivative).\n\
                 \x20   let foot_s = vec3<f32>(camera.viewport.x * 0.5, -camera.viewport.y * 0.5, 1.0);\n\
                 \x20   out.foot_p0 = foot_c0.xyw * foot_s;\n\
                 \x20   out.foot_pu = foot_cu.xyw * foot_s;\n\
                 \x20   out.foot_pv = foot_cv.xyw * foot_s;\n\
                 \x20   return out;\n}}"
            ),
        },
        Edit {
            what: "the coverage footprint",
            from: "    let fw = fwidth(in.glyph_uv);",
            to: "    let fw = foot_fwidth(in);".to_string(),
        },
        Edit {
            what: "the emoji sheet's sampling gradients",
            from: "textureSample(emoji_tex, emoji_samp, in.emoji_uv, in.emoji_layer)",
            to: "textureSampleGrad(emoji_tex, emoji_samp, in.emoji_uv, in.emoji_layer, foot_emoji_ddx(in), foot_emoji_ddy(in))"
                .to_string(),
        },
    ];
    let mut out = src.to_string();
    for e in &edits {
        let n = out.matches(e.from).count();
        assert_eq!(n, 1, "analytic footprint: anchor for {} matched {n} times, not once:\n{}", e.what, e.from);
        out = out.replacen(e.from, &e.to, 1);
    }
    out.push_str(FUNCTIONS);
    out
}

const FUNCTIONS: &str = r#"
// ── Analytic footprint (glyph-field footprint.rs, C35 2026-10-24) ──────────
// The fragment's pixel position as a function of its glyph_uv is the
// projective map p(u, v) = P0 + u·Pu + v·Pv, with (x, y, w) components in
// pixel units. Its Jacobian, inverted, is d(glyph_uv)/d(pixel), the quantity
// fwidth estimates from quad-mate lanes. Here it is read from the fragment's
// own inputs alone. Columns: d(uv)/dx, d(uv)/dy.
fn foot_jacobian(in: VsOut) -> mat2x2<f32> {
    let p = in.foot_p0 + in.glyph_uv.x * in.foot_pu + in.glyph_uv.y * in.foot_pv;
    let w2 = p.z * p.z;
    let xu = (in.foot_pu.x * p.z - p.x * in.foot_pu.z) / w2;
    let xv = (in.foot_pv.x * p.z - p.x * in.foot_pv.z) / w2;
    let yu = (in.foot_pu.y * p.z - p.y * in.foot_pu.z) / w2;
    let yv = (in.foot_pv.y * p.z - p.y * in.foot_pv.z) / w2;
    let det = xu * yv - xv * yu;
    // An edge-on quad (det -> 0) gets a huge footprint, as fwidth would; never inf/NaN.
    let inv = 1.0 / select(det, 1e-30, abs(det) < 1e-30);
    return mat2x2<f32>(vec2<f32>(yv, -yu) * inv, vec2<f32>(-xv, xu) * inv);
}

// fwidth(glyph_uv) = |d(uv)/dx| + |d(uv)/dy|.
fn foot_fwidth(in: VsOut) -> vec2<f32> {
    let j = foot_jacobian(in);
    return abs(j[0]) + abs(j[1]);
}

// emoji_uv = (cell + inset + (u, 1 - v) · span) / layer (vs_main), so its
// gradients are glyph_uv's, scaled per axis and flipped in v.
fn foot_emoji_scale() -> vec2<f32> {
    return (vec2<f32>(params.emoji_cell) - vec2<f32>(1.0)) / params.emoji_layer * vec2<f32>(1.0, -1.0);
}

fn foot_emoji_ddx(in: VsOut) -> vec2<f32> {
    return foot_jacobian(in)[0] * foot_emoji_scale();
}

fn foot_emoji_ddy(in: VsOut) -> vec2<f32> {
    return foot_jacobian(in)[1] * foot_emoji_scale();
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "struct Camera {\n    view_proj: mat4x4<f32>,\n};\n    @location(9) bg_color: vec4<f32>,\n};\n    var clip = camera.view_proj * vec4<f32>(posed + gpos.xyz, 1.0);\n    out.bg_color = gbg;\n    return out;\n}\n    let fw = fwidth(in.glyph_uv);\ntextureSample(emoji_tex, emoji_samp, in.emoji_uv, in.emoji_layer)";

    #[test]
    fn every_anchor_is_replaced_and_the_helpers_appended() {
        let out = analytic_source(SRC, "inst.height");
        assert!(!out.contains("= fwidth("), "the hardware derivative is gone");
        assert!(!out.contains("textureSample(emoji_tex"), "the implicit-gradient sample is gone");
        assert!(out.contains("viewport: vec2<f32>"));
        assert!(out.contains("vec3<f32>(0.0, inst.height, 0.0)"), "the per-mode quad height is spliced in");
        assert!(out.contains("fn foot_jacobian("));
    }

    #[test]
    #[should_panic(expected = "matched 0 times")]
    fn a_missing_anchor_refuses() {
        analytic_source(&SRC.replace("let fw = fwidth", "let fw = fwidthFine"), "1.0");
    }

    #[test]
    #[should_panic(expected = "matched 2 times")]
    fn a_doubled_anchor_refuses() {
        analytic_source(&format!("{SRC}\n    let fw = fwidth(in.glyph_uv);"), "1.0");
    }
}
