struct Camera {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
}

struct InstanceInput {
    @location(3) world_col0: vec4<f32>,
    @location(4) world_col1: vec4<f32>,
    @location(5) world_col2: vec4<f32>,
    @location(6) color: vec4<f32>,
    @location(7) material_params: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) material_params: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput, inst: InstanceInput) -> VertexOutput {
    var out: VertexOutput;

    let world_pos = inst.world_col0.xyz * in.position.x +
                    inst.world_col1.xyz * in.position.y +
                    inst.world_col2.xyz * in.position.z +
                    vec3<f32>(inst.world_col0.w, inst.world_col1.w, inst.world_col2.w);

    out.position = camera.view_proj * vec4<f32>(world_pos, 1.0);

    let world_mat = mat3x3<f32>(inst.world_col0.xyz, inst.world_col1.xyz, inst.world_col2.xyz);
    out.normal = normalize(world_mat * in.normal);

    out.color = inst.color;
    out.uv = in.uv;
    out.material_params = inst.material_params;

    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let unlit = in.material_params.w == 0.0;
    if unlit {
        return in.color;
    } else {
        // Simple directional diffuse light
        let light_dir = normalize(vec3<f32>(1.0, 1.0, 1.0));
        let ndotl = max(dot(in.normal, light_dir), 0.0);
        let diffuse = ndotl * 0.8 + 0.2; // 0.2 ambient
        return vec4<f32>(in.color.rgb * diffuse, in.color.a);
    }
}
