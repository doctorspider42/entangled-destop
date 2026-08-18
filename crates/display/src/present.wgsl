// Presents the guest scanout texture into the letterboxed viewport.
//
// No vertex buffers: one oversized triangle covers the whole clip space, and
// the render pass viewport (set from `letterbox()`) crops it to the image area.
// The letterbox bars are whatever the pass clears the attachment to (black).

@group(0) @binding(0) var scanout_texture: texture_2d<f32>;
@group(0) @binding(1) var scanout_sampler: sampler;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let index = i32(vertex_index);
    let x = f32(index / 2) * 4.0 - 1.0;
    let y = f32(index & 1) * 4.0 - 1.0;
    var out: VertexOutput;
    out.clip_position = vec4<f32>(x, y, 0.0, 1.0);
    // Clip space is y-up, texture space is y-down.
    out.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(scanout_texture, scanout_sampler, in.uv);
    // The guest's alpha is meaningless (XRGB framebuffers leave it zero) and a
    // transparent window is not what the user asked for.
    return vec4<f32>(color.rgb, 1.0);
}
