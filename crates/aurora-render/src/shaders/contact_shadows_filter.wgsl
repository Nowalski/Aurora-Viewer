// Softens the contact shadows' mask (contact_shadows.wgsl) before the
// shading reads it: each pixel's shadow is decided on its own along one ray,
// so the mask's borders are hard steps of whole pixels, and the gap under a
// hovering foot seen from above is a dotted line one pixel wide (issue 988).
// A 5 x 5 tent over the pixels of the same surface: a neighbour counts as
// far as its depth lies on the plane the pixel's own neighbours describe, so
// the shadow never spreads from the ground onto the caster nor from the
// caster onto the ground behind it (no halo).
//
// With `HALF` the pass traced the mask at half resolution, over the nearest
// depth of each 2 x 2 block (`half_depth_tex`, contact_shadows_half.wgsl),
// and this filter brings it back to the window's pixels: a neighbour takes
// its block's mask, and counts as far as its block's depth lies on the
// pixel's plane, so a block that holds the caster gives nothing to the
// ground beside it, and the reverse (a joint, plane-aware upsample).

@group(0) @binding(0) var depth_tex: texture_depth_2d;
@group(0) @binding(1) var mask_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;
// The depth the mask was traced over: `depth_tex` itself at full
// resolution, the nearest of each 2 x 2 block with `HALF`.
@group(0) @binding(3) var half_depth_tex: texture_depth_2d;
override HALF: bool = false;

// Reversed depth: the far plane (the sky) at 0.
const FAR_DEPTH: f32 = 0.0;
// How far off the pixel's plane a neighbour may lie, as a share of the
// pixel's depth (relative depth is relative distance), before it stops
// counting: 1 %, 3 cm at 3 m.
const PLANE_TOLERANCE: f32 = 0.01;
const RADIUS: i32 = 2;
// The workgroup's 8 x 8 pixels and RADIUS around them, read once into
// workgroup memory: depth, and the mask (-1 outside the screen).
const TILE: i32 = 8 + 2 * RADIUS;
var<workgroup> tile_depth: array<f32, 144>;
var<workgroup> tile_mask: array<f32, 144>;
// With `HALF`: the depth of the block each tile pixel takes its mask from.
var<workgroup> tile_block: array<f32, 144>;
// Whether any pixel of the tile is in contact shadow. Most of the screen has
// none: there the result is lit (a weighted sum of zeros) and the 25 taps are
// skipped, 0.05 ms of the pass's 0.25 at 1920 x 1080 (demo plaza, RTX 4090).
// Off (`Filter::new`, contact_shadows.rs) on the AMD RX 9000 cards, whose
// pass runs without workgroup memory: no atomic there either.
override SKIP_LIT: bool = true;
var<workgroup> any_shadow: atomic<u32>;

fn tile_at(t: vec2<i32>) -> i32 {
    return (t.y + RADIUS) * TILE + t.x + RADIUS;
}

// The smaller of the one-sided slopes: across an edge one side jumps, the
// other follows the surface.
fn slope(before: f32, centre: f32, after: f32) -> f32 {
    let a = centre - before;
    let b = after - centre;
    return select(b, a, abs(a) < abs(b));
}

@compute @workgroup_size(8, 8, 1)
fn main(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_id) local: vec3<u32>,
    @builtin(local_invocation_index) index: u32,
) {
    let size = vec2<i32>(textureDimensions(out_tex));
    let origin = vec2<i32>(group.xy) * 8 - RADIUS;
    for (var k = i32(index); k < TILE * TILE; k += 64) {
        let q = origin + vec2<i32>(k % TILE, k / TILE);
        let inside = all(q >= vec2<i32>(0)) && all(q < size);
        let c = clamp(q, vec2<i32>(0), size - 1);
        tile_depth[k] = textureLoad(depth_tex, c, 0);
        if (HALF) {
            let b = c >> vec2<u32>(1u);
            tile_block[k] = textureLoad(half_depth_tex, b, 0);
            tile_mask[k] = select(-1.0, textureLoad(mask_tex, b, 0).r, inside);
        } else {
            tile_mask[k] = select(-1.0, textureLoad(mask_tex, c, 0).r, inside);
        }
        if (SKIP_LIT && tile_mask[k] > 0.0) {
            atomicOr(&any_shadow, 1u);
        }
    }
    workgroupBarrier();
    let t = vec2<i32>(local.xy);
    let p = origin + RADIUS + t;
    if (any(p >= size)) {
        return;
    }
    if (SKIP_LIT && atomicLoad(&any_shadow) == 0u) {
        textureStore(out_tex, p, vec4<f32>(0.0));
        return;
    }
    let d0 = tile_depth[tile_at(t)];
    if (d0 <= FAR_DEPTH) {
        textureStore(out_tex, p, vec4<f32>(0.0));
        return;
    }
    let gx = slope(tile_depth[tile_at(t - vec2<i32>(1, 0))], d0, tile_depth[tile_at(t + vec2<i32>(1, 0))]);
    let gy = slope(tile_depth[tile_at(t - vec2<i32>(0, 1))], d0, tile_depth[tile_at(t + vec2<i32>(0, 1))]);
    // a block's depth is the nearest of its four pixels: on the plane up
    // to one pixel's slope away along each axis
    let tolerance = PLANE_TOLERANCE * d0 + select(0.0, abs(gx) + abs(gy), HALF);
    var sum = 0.0;
    var weight = 0.0;
    for (var j = -RADIUS; j <= RADIUS; j++) {
        for (var i = -RADIUS; i <= RADIUS; i++) {
            let at = tile_at(t + vec2<i32>(i, j));
            let mask = tile_mask[at];
            let expected = d0 + gx * f32(i) + gy * f32(j);
            let off_plane = abs(select(tile_depth[at], tile_block[at], HALF) - expected);
            let tent = f32((RADIUS + 1 - abs(i)) * (RADIUS + 1 - abs(j)));
            let w = select(0.0, tent * saturate(1.0 - off_plane / tolerance), mask >= 0.0);
            sum += w * mask;
            weight += w;
        }
    }
    // at full resolution the pixel itself always counts (weight 9 at
    // least); at half, when no block around lies on its plane (a pixel of
    // one surface among blocks of another), its own block's mask
    let own = tile_mask[tile_at(t)];
    textureStore(out_tex, p, vec4<f32>(select(own, sum / weight, weight > 0.0), 0.0, 0.0, 0.0));
}

// The mask as it is (`AURORA_CONTACT_SOFT=0`).
@compute @workgroup_size(8, 8, 1)
fn copy(@builtin(global_invocation_id) id: vec3<u32>) {
    let p = vec2<i32>(id.xy);
    if (all(p < vec2<i32>(textureDimensions(out_tex)))) {
        textureStore(out_tex, p, textureLoad(mask_tex, select(p, p >> vec2<u32>(1u), HALF), 0));
    }
}
