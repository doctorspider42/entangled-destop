// Checks 6 and 7: one triangle, split into three flat-coloured regions.
//
// Each vertex carries a one-hot barycentric (1,0,0) / (0,1,0) / (0,0,1); the
// fragment shader colours a pixel after the vertex whose weight is largest.
// That gives three regions of *exact* colour (only 0.0 and 1.0 are ever
// written, so UNORM conversion has nothing to round) whose boundaries run from
// the edge midpoints to the centroid. src/raster.rs computes the same image on
// the CPU, with the same tie order, and the geometry keeps every pixel centre
// well away from every edge and boundary, so the whole image can be compared
// exactly.
//
// Positions are raw Vulkan clip space (build.rs turns naga's y-flip off):
// y = -1 is the top row of the image.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) bary: vec3<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) bary: vec3<f32>) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(pos, 0.0, 1.0);
    o.bary = bary;
    return o;
}

@fragment
fn fs_main(@location(0) bary: vec3<f32>) -> @location(0) vec4<f32> {
    if (bary.x >= bary.y && bary.x >= bary.z) {
        return vec4<f32>(1.0, 0.0, 0.0, 1.0);
    }
    if (bary.y >= bary.z) {
        return vec4<f32>(0.0, 1.0, 0.0, 1.0);
    }
    return vec4<f32>(0.0, 0.0, 1.0, 1.0);
}
