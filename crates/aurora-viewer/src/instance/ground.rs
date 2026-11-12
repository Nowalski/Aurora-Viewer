//! The host's place made solid for a visiting guest. The simulator only
//! knows our own region, so its floors and walls are not there for it:
//! while we visit, our avatar flies, and its controls are steered so it
//! stands on the host's ground and linksets and stops at their walls. An
//! approximation by boxes (each prim's bounding box), good enough to walk
//! about a place; slopes and rotated prims are coarse.

use aurora_net::control;
use glam::{Quat, Vec3};

use super::wire::Worn;

/// What a step climbs (as an avatar walks up a stair).
const STEP: f32 = 0.5;
/// Half the height of an avatar, centre to feet.
pub(super) const HALF_HEIGHT: f32 = 0.95;
/// How far ahead a wall stops us.
const REACH: f32 = 0.45;
/// Closer than this to the floor, no correction.
const DEADBAND: f32 = 0.12;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: Vec3,
    pub max: Vec3,
}

impl Bounds {
    fn under(&self, x: f32, y: f32) -> bool {
        (self.min.x..=self.max.x).contains(&x) && (self.min.y..=self.max.y).contains(&y)
    }
}

/// The boxes of a linkset standing in the region, raised by `dz`.
pub fn linkset_bounds(thing: &Worn, dz: f32) -> Vec<Bounds> {
    let Some(root) = thing.prims.first() else {
        return Vec::new();
    };
    let root_pos = Vec3::from_array(root.pos) + Vec3::Z * dz;
    let root_rot = rotation(root.rot);
    thing
        .prims
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let (pos, rot) = if i == 0 {
                (root_pos, root_rot)
            } else {
                (root_pos + root_rot * Vec3::from_array(p.pos), root_rot * rotation(p.rot))
            };
            let half = Vec3::from_array(p.scale) * 0.5;
            let m = glam::Mat3::from_quat(rot);
            let extent = Vec3::new(
                m.x_axis.x.abs() * half.x + m.y_axis.x.abs() * half.y + m.z_axis.x.abs() * half.z,
                m.x_axis.y.abs() * half.x + m.y_axis.y.abs() * half.y + m.z_axis.y.abs() * half.z,
                m.x_axis.z.abs() * half.x + m.y_axis.z.abs() * half.y + m.z_axis.z.abs() * half.z,
            );
            (pos.is_finite() && extent.is_finite()).then_some(Bounds {
                min: pos - extent,
                max: pos + extent,
            })
        })
        .collect()
}

fn rotation(a: [f32; 4]) -> Quat {
    let q = Quat::from_array(a);
    if q.is_finite() && q.length_squared() > 1e-6 {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// Sunk into a floor by less than this, we are lifted back on top (our own
/// region let us sink through it, or we arrived a little low); deeper, it is
/// a room around us.
const SUNK: f32 = 1.5;

/// The floor under (`x`, `y`) for feet at `feet`: the land, or the highest
/// top a step can reach or we sank a little into. A thin platform whose top
/// is above our feet but within `SUNK` counts too, wherever its bottom is:
/// we stand inside it, and it would otherwise be neither a floor nor
/// passable, so we fell to the land under it.
pub fn floor<'a>(land: f32, boxes: impl Iterator<Item = &'a Bounds>, x: f32, y: f32, feet: f32) -> f32 {
    boxes
        .filter(|b| b.under(x, y) && b.max.z <= feet + SUNK)
        .map(|b| b.max.z)
        .fold(land, f32::max)
}

/// Whether something too high to step on stands at `at` for a body whose
/// centre is at `centre`.
fn blocked<'a>(boxes: impl Iterator<Item = &'a Bounds>, at: Vec3, centre: f32) -> bool {
    let feet = centre - HALF_HEIGHT;
    let head = centre + HALF_HEIGHT;
    boxes
        .into_iter()
        .any(|b| b.under(at.x, at.y) && b.max.z > feet + STEP && b.min.z < head)
}

/// Our controls while visiting: flying, held on the host's floor, stopped at
/// its walls. `pos`: where the simulator has us; `yaw`: our heading.
pub fn steer(flags: u32, pos: Vec3, yaw: f32, land: impl Fn(f32, f32) -> f32, boxes: &[Bounds]) -> u32 {
    let mut flags = flags | control::FLY;
    let at = axis(
        flags,
        control::AT_POS | control::NUDGE_AT_POS,
        control::AT_NEG | control::NUDGE_AT_NEG,
    );
    let left = axis(
        flags,
        control::LEFT_POS | control::NUDGE_LEFT_POS,
        control::LEFT_NEG | control::NUDGE_LEFT_NEG,
    );
    let (sin, cos) = yaw.sin_cos();
    let dir = Vec3::new(cos, sin, 0.0) * at + Vec3::new(-sin, cos, 0.0) * left;
    if dir.length_squared() > 1e-4 {
        let ahead = pos + dir.normalize() * REACH;
        if blocked(boxes.iter(), ahead, pos.z) {
            flags &= !(control::AT_POS
                | control::AT_NEG
                | control::LEFT_POS
                | control::LEFT_NEG
                | control::FAST_AT
                | control::FAST_LEFT
                | control::NUDGE_AT_POS
                | control::NUDGE_AT_NEG
                | control::NUDGE_LEFT_POS
                | control::NUDGE_LEFT_NEG);
        }
    }
    // going up stays ours (a jump, flying up): the floor brings us back
    if flags & (control::UP_POS | control::NUDGE_UP_POS) != 0 {
        return flags;
    }
    let feet = pos.z - HALF_HEIGHT;
    let ground = floor(land(pos.x, pos.y), boxes.iter(), pos.x, pos.y, feet);
    let target = ground + HALF_HEIGHT;
    flags &= !(control::UP_NEG | control::NUDGE_UP_NEG);
    if pos.z > target + DEADBAND {
        flags |= control::UP_NEG;
    } else if pos.z < target - DEADBAND {
        flags |= control::UP_POS;
    }
    flags
}

fn axis(flags: u32, pos: u32, neg: u32) -> f32 {
    f32::from(u8::from(flags & pos != 0)) - f32::from(u8::from(flags & neg != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slab(min: [f32; 3], max: [f32; 3]) -> Bounds {
        Bounds {
            min: Vec3::from_array(min),
            max: Vec3::from_array(max),
        }
    }

    #[test]
    fn stands_on_a_floor_and_stops_at_a_wall() {
        let floor = slab([0.0, 0.0, 29.5], [20.0, 20.0, 30.0]);
        let wall = slab([10.0, 0.0, 30.0], [10.5, 20.0, 34.0]);
        let boxes = [floor, wall];
        let land = |_: f32, _: f32| 20.0;
        // standing on the floor: no vertical correction, flying
        let f = steer(0, Vec3::new(5.0, 5.0, 30.95), 0.0, land, &boxes);
        assert_eq!(f & (control::UP_POS | control::UP_NEG), 0);
        assert_ne!(f & control::FLY, 0);
        // above it: down
        assert_ne!(steer(0, Vec3::new(5.0, 5.0, 33.0), 0.0, land, &boxes) & control::UP_NEG, 0);
        // sunk into it: up; fallen through it: the land below
        assert_ne!(steer(0, Vec3::new(5.0, 5.0, 30.6), 0.0, land, &boxes) & control::UP_POS, 0);
        assert_ne!(steer(0, Vec3::new(5.0, 5.0, 29.0), 0.0, land, &boxes) & control::UP_NEG, 0);
        // walking east into the wall stops; west is free
        let at = control::AT_POS | control::FAST_AT;
        assert_eq!(steer(at, Vec3::new(9.8, 5.0, 30.95), 0.0, land, &boxes) & control::AT_POS, 0);
        assert_ne!(
            steer(at, Vec3::new(9.8, 5.0, 30.95), std::f32::consts::PI, land, &boxes) & control::AT_POS,
            0
        );
        // off the floor, the land
        let f = steer(0, Vec3::new(30.0, 5.0, 20.95), 0.0, land, &boxes);
        assert_eq!(f & (control::UP_POS | control::UP_NEG), 0);
    }

    #[test]
    fn a_thin_platform_a_little_above_our_feet_lifts_us() {
        // a 0.2 m platform whose bottom is 0.7 m above our feet: we stand
        // inside it, so it is our floor, not the land below
        let platform = slab([0.0, 0.0, 20.7], [10.0, 10.0, 20.9]);
        let land = |_: f32, _: f32| 20.0;
        assert!((floor(20.0, [platform].iter(), 5.0, 5.0, 20.0) - 20.9).abs() < 1e-4);
        assert_ne!(steer(0, Vec3::new(5.0, 5.0, 20.95), 0.0, land, &[platform]) & control::UP_POS, 0);
        // far above our feet it is a ceiling
        let ceiling = slab([0.0, 0.0, 22.5], [10.0, 10.0, 22.7]);
        assert!((floor(20.0, [ceiling].iter(), 5.0, 5.0, 20.0) - 20.0).abs() < 1e-4);
    }

    #[test]
    fn a_rotated_child_has_its_box() {
        let w = Worn {
            id: uuid::Uuid::nil(),
            root: 1,
            prims: vec![
                prim(1, [10.0, 10.0, 20.0], [4.0, 4.0, 0.5]),
                prim(2, [0.0, 0.0, 2.0], [1.0, 1.0, 1.0]),
            ],
        };
        let b = linkset_bounds(&w, 1.0);
        assert_eq!(b.len(), 2);
        assert!((b[0].max.z - 21.25).abs() < 1e-4);
        assert!((b[1].min.z - 22.5).abs() < 1e-4);
    }

    fn prim(key: u32, pos: [f32; 3], scale: [f32; 3]) -> super::super::wire::WornPrim {
        super::super::wire::WornPrim {
            key,
            state: 0,
            pcode: aurora_prim::params::LL_PCODE_VOLUME,
            material: 3,
            pos,
            rot: [0.0, 0.0, 0.0, 1.0],
            scale,
            shape: [0; 18],
            te: String::new(),
            extra: "AA==".into(),
        }
    }
}
