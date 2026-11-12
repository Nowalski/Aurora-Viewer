//! Our own avatar as wire state: where it is, which way it faces, what it
//! plays, and its appearance when that changed. The rates keep the link
//! light: a state at most every 100 ms while the avatar moves, a heartbeat
//! every second otherwise, and the appearance again every 5 s so a viewer
//! that joins late still gets it.

use super::wire::{MAX_ANIMS, PeerAppearance, PeerState};
use crate::world::World;
use aurora_net::AvatarAppearance;
use aurora_net::local_bakes::AVATAR_FACES;
use aurora_prim::te::{pack_texture_entry, parse_texture_entry};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use glam::Vec3;
use std::collections::hash_map::DefaultHasher;
use std::f32::consts::TAU;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Shortest gap between two states while the avatar moves.
const MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Longest gap between two states, moving or not.
const HEARTBEAT: Duration = Duration::from_secs(1);
/// The appearance is sent again this often, for late arrivals.
const APPEARANCE_REFRESH: Duration = Duration::from_secs(5);
/// Smaller moves are not worth a message (metres, radians).
const POSITION_EPSILON: f32 = 0.01;
const YAW_EPSILON: f32 = 0.01;
/// Visual parameters kept from a peer: the viewer's own list is far shorter.
const MAX_VISUAL_PARAMS: usize = 255;

/// What a state carries besides the appearance, as last sent or read now.
#[derive(Clone)]
struct Pose {
    pos: Vec3,
    yaw: f32,
    anims: Vec<(Uuid, i32)>,
}

#[derive(Default)]
pub struct Capture {
    /// When the last state went out, and the pose it carried.
    last_state: Option<(Instant, Pose)>,
    /// Key of the last appearance sent, and when it went out.
    last_appearance: Option<(u64, Instant)>,
}

impl Capture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a first state went out.
    pub fn started(&self) -> bool {
        self.last_state.is_some()
    }

    /// The state to send now, if one is due. `peer_id` is the instance id
    /// of this viewer, not the avatar's agent id.
    pub fn state(&mut self, world: &World, peer_id: Uuid, now: Instant) -> Option<PeerState> {
        let pose = pose_of(world);
        if !state_due(self.last_state.as_ref().map(|(at, p)| (*at, p)), now, &pose) {
            return None;
        }
        let appearance = self.appearance_to_send(world.server_appearance(), now);
        let state = PeerState {
            id: peer_id,
            pos: pose.pos.to_array(),
            rot: world.agent.body_rotation().to_array(),
            vel: world.agent.velocity.to_array(),
            flying: world.agent.flying,
            anims: pose.anims.clone(),
            appearance,
            body_te: None,
        };
        self.last_state = Some((now, pose));
        Some(state)
    }

    fn appearance_to_send(&mut self, appearance: Option<&AvatarAppearance>, now: Instant) -> Option<PeerAppearance> {
        let appearance = appearance?;
        let key = appearance_key(appearance);
        if !appearance_due(self.last_appearance, key, now) {
            return None;
        }
        let encoded = encode_appearance(appearance)?;
        self.last_appearance = Some((key, now));
        Some(encoded)
    }
}

/// The wire form of an appearance. `None` without a texture entry, or when
/// the entry packs to nothing (a peer could not parse it back).
pub fn encode_appearance(a: &AvatarAppearance) -> Option<PeerAppearance> {
    let te = a.texture_entry.as_ref()?;
    let packed = pack_texture_entry(te, AVATAR_FACES);
    if packed.is_empty() {
        return None;
    }
    Some(PeerAppearance {
        te: STANDARD.encode(packed),
        visual_params: STANDARD.encode(&a.visual_params),
        hover: finite_or_zero(a.hover_height),
        cof_version: a.cof_version,
    })
}

/// Parse a peer's appearance back. `None` on bad base64 or an unreadable
/// texture entry; the caller drops the peer's appearance then.
pub fn decode_appearance(avatar_id: Uuid, a: &PeerAppearance) -> Option<AvatarAppearance> {
    let packed = decode_base64(&a.te)?;
    let Some(texture_entry) = parse_texture_entry(&packed) else {
        log::warn!("instance: unreadable texture entry from a peer");
        return None;
    };
    let mut visual_params = decode_base64(&a.visual_params)?;
    visual_params.truncate(MAX_VISUAL_PARAMS);
    Some(AvatarAppearance {
        avatar_id,
        texture_entry: Some(texture_entry.into()),
        visual_params,
        cof_version: a.cof_version,
        appearance_version: 1,
        hover_height: finite_or_zero(a.hover),
        attachments: Vec::new(),
    })
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(s)
        .map_err(|e| log::warn!("instance: bad base64 in a peer's appearance: {e}"))
        .ok()
}

fn finite_or_zero(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

fn pose_of(world: &World) -> Pose {
    let anims = world
        .animations
        .get(&world.agent_id)
        .map(|list| list.iter().take(MAX_ANIMS).map(|a| (a.id, a.sequence)).collect())
        .unwrap_or_default();
    Pose {
        pos: world.agent.position,
        yaw: world.agent.yaw,
        anims,
    }
}

/// Smallest angle between two yaws, wrapped around the circle.
fn yaw_delta(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(TAU);
    d.min(TAU - d)
}

/// Whether the avatar moved, turned or changed animations noticeably.
fn moved(prev: &Pose, now: &Pose) -> bool {
    prev.pos.distance(now.pos) > POSITION_EPSILON || yaw_delta(prev.yaw, now.yaw) > YAW_EPSILON || prev.anims != now.anims
}

/// Whether a state is due: at most every `MIN_INTERVAL` while the pose
/// changed since the last one sent, and every `HEARTBEAT` regardless.
fn state_due(last: Option<(Instant, &Pose)>, now: Instant, pose: &Pose) -> bool {
    let Some((at, prev)) = last else {
        return true;
    };
    let elapsed = now.saturating_duration_since(at);
    elapsed >= HEARTBEAT || (elapsed >= MIN_INTERVAL && moved(prev, pose))
}

/// Whether the appearance goes out with this state: when its content
/// changed since the last one sent, or `APPEARANCE_REFRESH` passed.
fn appearance_due(last: Option<(u64, Instant)>, key: u64, now: Instant) -> bool {
    match last {
        None => true,
        Some((sent_key, at)) => sent_key != key || now.saturating_duration_since(at) >= APPEARANCE_REFRESH,
    }
}

/// Hash of what the peer sees of the appearance: cof version, visual
/// parameters, hover and the packed texture entry. Not for security.
fn appearance_key(a: &AvatarAppearance) -> u64 {
    let mut h = DefaultHasher::new();
    a.cof_version.hash(&mut h);
    a.visual_params.hash(&mut h);
    a.hover_height.to_bits().hash(&mut h);
    if let Some(te) = &a.texture_entry {
        pack_texture_entry(te, AVATAR_FACES).hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_prim::te::{MAX_TES, TextureEntry, TextureFace};
    use std::sync::Arc;

    fn pose(x: f32, yaw: f32) -> Pose {
        Pose {
            pos: Vec3::new(x, 0.0, 0.0),
            yaw,
            anims: Vec::new(),
        }
    }

    #[test]
    fn appearance_round_trip() {
        let mut te = TextureEntry {
            faces: vec![TextureFace::default(); MAX_TES],
        };
        let texture = Uuid::from_u128(0x1234_5678);
        te.faces[3].texture = texture;
        te.faces[3].scale_s = 2.0;
        let original = AvatarAppearance {
            avatar_id: Uuid::from_u128(7),
            texture_entry: Some(Arc::new(te)),
            visual_params: vec![0, 1, 2, 128, 255],
            cof_version: 42,
            appearance_version: 1,
            hover_height: 0.25,
            attachments: Vec::new(),
        };
        let wire = encode_appearance(&original).expect("encodes");
        let back = decode_appearance(original.avatar_id, &wire).expect("decodes");
        assert_eq!(back.avatar_id, original.avatar_id);
        assert_eq!(back.visual_params, original.visual_params);
        assert_eq!(back.cof_version, 42);
        assert_eq!(back.appearance_version, 1);
        assert!(back.attachments.is_empty());
        assert_eq!(back.hover_height, 0.25);
        let faces = &back.texture_entry.expect("texture entry").faces;
        assert_eq!(faces[3].texture, texture);
        assert_eq!(faces[3].scale_s, 2.0);
    }

    #[test]
    fn encode_without_texture_entry_is_none() {
        let a = AvatarAppearance {
            avatar_id: Uuid::nil(),
            texture_entry: None,
            visual_params: vec![1],
            cof_version: 1,
            appearance_version: 1,
            hover_height: 0.0,
            attachments: Vec::new(),
        };
        assert!(encode_appearance(&a).is_none());
    }

    #[test]
    fn decode_rejects_bad_base64_and_bad_entry() {
        let good = PeerAppearance {
            te: "!!!not base64".into(),
            visual_params: STANDARD.encode([1u8]),
            hover: 0.0,
            cof_version: 1,
        };
        assert!(decode_appearance(Uuid::nil(), &good).is_none());
        let empty_entry = PeerAppearance {
            te: String::new(),
            visual_params: STANDARD.encode([1u8]),
            hover: 0.0,
            cof_version: 1,
        };
        assert!(decode_appearance(Uuid::nil(), &empty_entry).is_none());
    }

    #[test]
    fn decode_caps_visual_params_and_fixes_hover() {
        let mut te = TextureEntry {
            faces: vec![TextureFace::default(); MAX_TES],
        };
        te.faces[0].scale_s = 1.0;
        let wire = PeerAppearance {
            te: STANDARD.encode(pack_texture_entry(&te, AVATAR_FACES)),
            visual_params: STANDARD.encode(vec![9u8; 400]),
            hover: f32::NAN,
            cof_version: 3,
        };
        let back = decode_appearance(Uuid::nil(), &wire).expect("decodes");
        assert_eq!(back.visual_params.len(), MAX_VISUAL_PARAMS);
        assert_eq!(back.hover_height, 0.0);
    }

    #[test]
    fn yaw_delta_wraps_around() {
        let near_pi = std::f32::consts::PI - 0.004;
        // just either side of the half turn: 0.008 apart, not almost a full turn
        assert!((yaw_delta(near_pi, -near_pi) - 0.008).abs() < 1e-4);
        assert!((yaw_delta(0.0, 1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn small_moves_do_not_count() {
        let base = pose(0.0, 0.0);
        assert!(!moved(&base, &pose(0.005, 0.0)));
        assert!(!moved(&base, &pose(0.0, 0.005)));
        assert!(moved(&base, &pose(0.02, 0.0)));
        assert!(moved(&base, &pose(0.0, 0.02)));
        let mut animated = pose(0.0, 0.0);
        animated.anims.push((Uuid::from_u128(1), 1));
        assert!(moved(&base, &animated));
    }

    #[test]
    fn state_rates() {
        let t0 = Instant::now();
        let still = pose(0.0, 0.0);
        let walking = pose(1.0, 0.0);
        // first state always goes out
        assert!(state_due(None, t0, &still));
        let last = Some((t0, &still));
        // moving: at most every 100 ms
        assert!(!state_due(last, t0 + Duration::from_millis(50), &walking));
        assert!(state_due(last, t0 + Duration::from_millis(120), &walking));
        // not moving: heartbeat every second only
        assert!(!state_due(last, t0 + Duration::from_millis(500), &still));
        assert!(state_due(last, t0 + Duration::from_secs(1), &still));
    }

    #[test]
    fn appearance_rates() {
        let t0 = Instant::now();
        assert!(appearance_due(None, 1, t0));
        let last = Some((1, t0));
        assert!(!appearance_due(last, 1, t0 + Duration::from_secs(4)));
        assert!(appearance_due(last, 1, t0 + Duration::from_secs(5)));
        assert!(appearance_due(last, 2, t0 + Duration::from_secs(1)));
    }
}
