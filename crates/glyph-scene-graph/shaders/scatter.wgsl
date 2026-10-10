// scatter.wgsl — apply a sparse upload: rows staged contiguously in `src`,
// each copied to the row `indices` names in `dest`. One thread per 32-bit
// WORD, not per row, so a 32 B row is eight threads and nothing loops.
//
// Adapted from bevy's `sparse_buffer_update.wesl`
// (bevy_render/src/render_resource/, bevy 0.16+, MIT OR Apache-2.0), which
// scatters `AtomicSparseBufferVec` updates the same way; the host-side
// policy (runs below a few, full upload above 15 % of the table) is
// `src/upload.rs`. Changes from bevy: plain WGSL instead of WESL, a 2-D
// dispatch so a large batch is not capped at 65,535 workgroups, and the
// metadata names.

struct ScatterParams {
    row_words: u32, // words per row
    rows: u32,      // rows staged in src
    _p0: u32,
    _p1: u32,
}

@group(0) @binding(0) var<storage, read_write> dest: array<u32>;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read> indices: array<u32>;
@group(0) @binding(3) var<uniform> params: ScatterParams;

const WG: u32 = 256u;

@compute @workgroup_size(256)
fn scatter(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let w = gid.x + gid.y * nwg.x * WG;
    if w >= params.rows * params.row_words {
        return;
    }
    let row = w / params.row_words;
    let word = w % params.row_words;
    let d = indices[row] * params.row_words + word;
    if d >= arrayLength(&dest) {
        return;
    }
    dest[d] = src[w];
}
