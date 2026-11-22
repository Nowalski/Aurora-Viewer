// The depth the contact shadows trace at half resolution (contact_shadows.rs,
// `Half`): one texel per 2 x 2 block of the window's depth, the nearest of
// the four (reversed depth: the largest), drawn into a depth target so that
// the pass reads it as it reads the prepass. The nearest keeps the thin
// casters (a strap, a heel, a blade of grass) that a fixed pixel of the
// block would miss one time in two; the filter then gives a block's shadow
// only to the pixels on its plane (contact_shadows_filter.wgsl, `HALF`).

@group(0) @binding(0) var depth_tex: texture_depth_2d;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @builtin(frag_depth) f32 {
    // an odd width or height: the last block repeats its edge pixels
    let last = vec2<i32>(textureDimensions(depth_tex)) - 1;
    let p = vec2<i32>(floor(pos.xy)) * 2;
    let a = textureLoad(depth_tex, min(p, last), 0);
    let b = textureLoad(depth_tex, min(p + vec2<i32>(1, 0), last), 0);
    let c = textureLoad(depth_tex, min(p + vec2<i32>(0, 1), last), 0);
    let d = textureLoad(depth_tex, min(p + vec2<i32>(1, 1), last), 0);
    return max(max(a, b), max(c, d));
}
