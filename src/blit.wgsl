struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Standard fullscreen-triangle trick: 3 vertices, no vertex buffer needed.
// vertex_index 0,1,2 -> covers the whole clip-space area with one triangle.
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);
    out.uv = vec2<f32>(x, y);
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var src_texture: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;

// wgpu presents sampled texel data in logical R,G,B,A order regardless of the
// source texture's memory byte layout (that's what the format name like
// Bgra8UnormSrgb vs Rgba8Unorm actually describes: how bytes sit in memory,
// not what a shader sees when it samples). So this fragment shader needs no
// manual channel swizzle - sampling CEF's Bgra8UnormSrgb frame here already
// yields correct (r, g, b, a) values, and writing them straight into an
// Rgba8Unorm render target is the entire "conversion".
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(src_texture, src_sampler, in.uv);
}
