//! The people of the host's place, for its guests: every avatar the hosting
//! viewer sees in its region (not its own, not the instance's peers) goes
//! out as `Present` + `State`, and a `Bye` when it leaves. Like our own
//! state, only ids and small state: each guest loads the looks itself.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use glam::{Quat, Vec3};
use uuid::Uuid;

use super::capture::encode_appearance;
use super::wire::{self, Msg, PeerState};
use crate::world::World;
use crate::world::objects::{ObjKey, Object};

/// A moving avatar is sent at most this often: half our own avatar's rate,
/// as the crowd can be large.
const MOVING: Duration = Duration::from_millis(200);
/// A still avatar is sent again this often, so a guest's copy cannot drift.
const HEARTBEAT: Duration = Duration::from_secs(2);
/// Looks go again this often for guests who arrived since.
const APPEARANCE_AGAIN: Duration = Duration::from_secs(10);

struct Sent {
    at: Instant,
    pos: Vec3,
    rot: Quat,
    anims: Vec<(Uuid, i32)>,
    appearance_at: Option<Instant>,
    appearance_cof: i32,
    appearance_len: usize,
}

#[derive(Default)]
pub struct Crowd {
    sent: HashMap<Uuid, Sent>,
}

impl Crowd {
    pub fn new() -> Self {
        Self::default()
    }

    /// What to send this frame. `skip`: avatars the instance itself shows
    /// (its peers), which must not come back as people of the place.
    pub fn messages(&mut self, world: &World, skip: impl Fn(&Uuid) -> bool, now: Instant) -> Vec<Msg> {
        let Some(main) = world.main().map(|r| r.handle) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (_, avatar) in world.objects.avatars() {
            let id = avatar.full_id;
            if id == world.agent_id || id.is_nil() || skip(&id) || avatar.key.region != main {
                continue;
            }
            let Some((pos, rot)) = place(world, avatar) else {
                continue;
            };
            seen.insert(id);
            let anims: Vec<(Uuid, i32)> = world
                .animations
                .get(&id)
                .map(|a| a.iter().take(wire::MAX_ANIMS).map(|p| (p.id, p.sequence)).collect())
                .unwrap_or_default();
            let appearance = world.avatar_appearance(&id);
            let fresh = !self.sent.contains_key(&id);
            if fresh {
                // RLVa may hide names: World::avatar_name applies it
                let name = world.avatar_name(&id).unwrap_or_else(|| "Resident".into());
                let name: String = name.chars().filter(|c| !c.is_control()).take(wire::MAX_NAME).collect();
                out.push(Msg::Present {
                    id,
                    name: if name.trim().is_empty() { "Resident".into() } else { name },
                });
            }
            let due = self.sent.get(&id).is_none_or(|s| {
                let changed = s.pos.distance(pos) > 0.01 || s.rot.angle_between(rot) > 0.01 || s.anims != anims;
                let since = now.duration_since(s.at);
                (changed && since >= MOVING) || since >= HEARTBEAT
            });
            if !due {
                continue;
            }
            let looks_due = appearance.is_some_and(|a| {
                self.sent.get(&id).is_none_or(|s| {
                    s.appearance_cof != a.cof_version
                        || s.appearance_len != a.visual_params.len()
                        || s.appearance_at.is_none_or(|t| now.duration_since(t) >= APPEARANCE_AGAIN)
                })
            });
            let encoded = if looks_due { appearance.and_then(encode_appearance) } else { None };
            let entry = self.sent.entry(id).or_insert(Sent {
                at: now,
                pos,
                rot,
                anims: Vec::new(),
                appearance_at: None,
                appearance_cof: i32::MIN,
                appearance_len: 0,
            });
            entry.at = now;
            entry.pos = pos;
            entry.rot = rot;
            entry.anims.clone_from(&anims);
            if encoded.is_some()
                && let Some(a) = appearance
            {
                entry.appearance_at = Some(now);
                entry.appearance_cof = a.cof_version;
                entry.appearance_len = a.visual_params.len();
            }
            out.push(Msg::State(PeerState {
                id,
                pos: pos.to_array(),
                rot: rot.to_array(),
                vel: avatar.velocity.to_array(),
                flying: false,
                anims,
                appearance: encoded,
            }));
        }
        // those who left the place (or the view)
        let gone: Vec<Uuid> = self.sent.keys().filter(|id| !seen.contains(id)).copied().collect();
        for id in gone {
            self.sent.remove(&id);
            out.push(Msg::Bye { id });
        }
        out
    }
}

/// An avatar's position and rotation in its region: its own, or through the
/// object it sits on (a seat's root; deeper chains are skipped).
fn place(world: &World, avatar: &Object) -> Option<(Vec3, Quat)> {
    let (pos, rot) = if avatar.parent_id == 0 {
        (avatar.position, avatar.rotation)
    } else {
        let key = ObjKey {
            region: avatar.parent_region,
            local_id: avatar.parent_id,
        };
        let seat = world.objects.get(world.objects.index_of(&key)?)?;
        if seat.parent_id != 0 {
            return None;
        }
        (seat.position + seat.rotation * avatar.position, seat.rotation * avatar.rotation)
    };
    let valid = pos.is_finite() && (0.0..=8192.0).contains(&pos.x) && (0.0..=8192.0).contains(&pos.y) && rot.is_finite();
    valid.then_some((pos, rot.normalize()))
}
