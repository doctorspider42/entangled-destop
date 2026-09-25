// The guest's hardware-cursor plane (MVP-812), drawn over a scanout the CPU
// mirror does not hold — a frame presented through the display's GPU
// (ADR-0004, zero-copy presentation), where there are no pixels on the CPU to
// composite the cursor into.
//
// One quad, placed by a uniform in the clip space of the scanout viewport; the
// image is premultiplied BGRA (the DRM cursor-plane convention), blended
// source-over by the pipeline (ONE, ONE_MINUS_SRC_ALPHA).

@group(0) @binding(0) var cursor_texture: texture_2d<f32>;
@group(0) @binding(1) var cursor_sampler: sampler;

struct Placement {
    // Left, top, right, bottom in clip space (y up).
    rect: vec4<f32>,
};
@group(0) @binding(2) var<uniform> placement: Placement;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_cursor(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Two triangles: (0,0) (1,0) (0,1) and (1,0) (1,1) (0,1).
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];
    var out: VertexOutput;
    let x = mix(placement.rect.x, placement.rect.z, corner.x);
    let y = mix(placement.rect.y, placement.rect.w, corner.y);
    out.clip_position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_cursor(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(cursor_texture, cursor_sampler, in.uv);
}
