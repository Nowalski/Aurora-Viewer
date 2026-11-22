// The depth the contact shadows trace at half resolution (contact_shadows.rs,
// `Resolution`): one texel per 2 x 2 block of the window's depth, drawn into
// a depth target so that the pass reads it as it reads the prepass. In a
// checkerboard, the nearest of the four (reversed depth: the largest) and
// the farthest: the nearest keeps the thin casters (a strap, a heel, a
// blade of grass) that a fixed pixel of the block would miss one time in
// two, and the farthest keeps the surface behind them, so that a pixel of
// either finds a block of its own plane next to it, which the filter takes
// (contact_shadows_filter.wgsl, `HALF`). With the nearest alone, the dress
// beside a hand resting on it found only the hand's blocks around it, and
// took their shadow: a stepped outline and a streak below the hand (demo
// `AURORA_DEMO_FEET`, `AURORA_DEMO_SUN=90,35`).

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
    let near = max(max(a, b), max(c, d));
    let far = min(min(a, b), min(c, d));
    // the sky (0) is no surface: a block on a silhouette keeps the object
    let far_solid = select(far, near, far <= 0.0);
    let blocks = vec2<i32>(floor(pos.xy));
    return select(far_solid, near, ((blocks.x + blocks.y) & 1) == 0);
}
