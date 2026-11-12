//! The host's place, for guests who visit it: every linkset standing in the
//! region (not avatars, not what they wear, not what the instance itself
//! shows) as `Thing` messages, again when one changes and a `Gone` when it
//! leaves, and the land as `Terrain` rows when it changes. Ids and shapes
//! only, like the rest of the instance: each guest loads the textures and
//! meshes itself, under its own viewer's permission rules.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use aurora_net::NetEvent;
use aurora_net::RegionHandle;
use aurora_net::terrain::TerrainPatch;
use aurora_prim::params::LL_PCODE_VOLUME;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use uuid::Uuid;

use super::mirror::FIRST_LOCAL_ID;
use super::wire::{self, MAX_REGION, Msg, Worn};
use super::worn::linkset;
use crate::world::World;

/// How often the place is looked at again.
const SCAN: Duration = Duration::from_secs(2);
/// Linksets sent at most per scan, so a large place goes out over a few
/// scans instead of filling the guests' queues at once.
const PER_SCAN: usize = 400;

#[derive(Default)]
pub struct PlaceSender {
    scanned: Option<Instant>,
    /// Root key -> hash of the message last sent.
    sent: HashMap<u32, u64>,
    /// The land's revision last sent.
    terrain: Option<u64>,
}

impl PlaceSender {
    /// What changed in our place since the last scan, sent under `id`.
    pub fn messages(&mut self, world: &World, id: Uuid, now: Instant) -> Vec<Msg> {
        if self.scanned.is_some_and(|t| now.duration_since(t) < SCAN) {
            return Vec::new();
        }
        self.scanned = Some(now);
        let Some(region) = world.main() else {
            return Vec::new();
        };
        let handle = region.handle;
        let mut out = Vec::new();
        if self.terrain != Some(region.terrain_revision) {
            self.terrain = Some(region.terrain_revision);
            out.extend(terrain_rows(&region.heightmap, id));
        }
        let mut seen = HashSet::new();
        let mut sent_now = 0;
        for (_, o) in world.objects.iter() {
            if o.key.region != handle
                || o.parent_id != 0
                || o.pcode != LL_PCODE_VOLUME
                || o.key.local_id >= FIRST_LOCAL_ID
                || o.is_client_preview()
            {
                continue;
            }
            let key = o.key.local_id;
            seen.insert(key);
            if sent_now >= PER_SCAN {
                continue;
            }
            let msg = Msg::Thing(Worn {
                id,
                root: key,
                prims: linkset(world, o),
            });
            let line = wire::encode(&msg);
            if line.len() > wire::MAX_LINE {
                continue;
            }
            let mut h = DefaultHasher::new();
            line.hash(&mut h);
            let hash = h.finish();
            if self.sent.get(&key) != Some(&hash) {
                self.sent.insert(key, hash);
                sent_now += 1;
                out.push(msg);
            }
        }
        let gone: Vec<u32> = self.sent.keys().filter(|k| !seen.contains(k)).copied().collect();
        for root in gone {
            self.sent.remove(&root);
            out.push(Msg::Gone { id, root });
        }
        out
    }
}

/// The land in rows of 16 m.
fn terrain_rows(map: &crate::world::terrain::Heightmap, id: Uuid) -> Vec<Msg> {
    let (w, h) = (map.size_x.min(MAX_REGION), map.size_y.min(MAX_REGION));
    if w < 256 || map.heights.len() < (map.size_x * map.size_y) as usize {
        return Vec::new();
    }
    (0..h / 16)
        .map(|row| {
            let mut bytes = Vec::with_capacity((w * 16 * 4) as usize);
            for y in row * 16..row * 16 + 16 {
                let start = (y * map.size_x) as usize;
                for v in &map.heights[start..start + w as usize] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
            }
            Msg::Terrain {
                id,
                row,
                size_x: w,
                heights: STANDARD.encode(bytes),
            }
        })
        .collect()
}

/// A row of the host's land as the world's terrain patches (16 m each).
pub fn terrain_event(handle: RegionHandle, row: u32, size_x: u32, heights: &str) -> Option<NetEvent> {
    let bytes = STANDARD.decode(heights).ok()?;
    let values: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .map(|v| if v.is_finite() { v.clamp(-100.0, 4096.0) } else { 0.0 })
        .collect();
    let w = size_x as usize;
    if values.len() != w * 16 {
        return None;
    }
    let patches = (0..size_x / 16)
        .map(|px| {
            let mut heights = Vec::with_capacity(256);
            for j in 0..16 {
                let start = j * w + px as usize * 16;
                heights.extend_from_slice(&values[start..start + 16]);
            }
            TerrainPatch {
                x: px,
                y: row,
                size: 16,
                heights,
            }
        })
        .collect();
    Some(NetEvent::Terrain { handle, patches })
}

/// Our own land, kept while we visit, to put back when we leave.
pub struct SavedLand {
    size_x: u32,
    heights: Vec<f32>,
}

impl SavedLand {
    pub fn of(world: &World) -> Option<SavedLand> {
        let map = &world.main()?.heightmap;
        Some(SavedLand {
            size_x: map.size_x,
            heights: map.heights.clone(),
        })
    }

    /// The land as it was, as terrain patches.
    pub fn restore(&self, handle: RegionHandle) -> Option<NetEvent> {
        let w = self.size_x as usize;
        if w == 0 || self.heights.len() % (w * 16) != 0 {
            return None;
        }
        let rows = self.heights.len() / (w * 16);
        let mut patches = Vec::new();
        for row in 0..rows {
            for px in 0..w / 16 {
                let mut heights = Vec::with_capacity(256);
                for j in 0..16 {
                    let start = (row * 16 + j) * w + px * 16;
                    heights.extend_from_slice(&self.heights[start..start + 16]);
                }
                patches.push(TerrainPatch {
                    x: px as u32,
                    y: row as u32,
                    size: 16,
                    heights,
                });
            }
        }
        Some(NetEvent::Terrain { handle, patches })
    }
}

/// How often our own place is looked at again for what to hide.
const HIDE_CHECK: Duration = Duration::from_secs(1);

/// A guest visiting the host's place: ours is hidden (its objects and the
/// people the instance does not show) and its land kept, to come back when
/// the visit ends. We stay where we are in SL: only what we see changes.
pub struct Visit {
    /// The host, whose place we see.
    pub host: Uuid,
    land: Option<SavedLand>,
    /// What we hid of our place, to show it again.
    hidden: HashSet<Uuid>,
    checked: Option<Instant>,
}

impl Visit {
    pub fn start(world: &World, host: Uuid) -> Visit {
        log::info!("shared instance: visiting the host's place");
        Visit {
            host,
            land: SavedLand::of(world),
            hidden: HashSet::new(),
            checked: None,
        }
    }

    /// What of our place to hide now: the root objects of our region and its
    /// people, but not us, what we sit on, nor who the instance shows
    /// (`shown`). Each id with whether it is an avatar.
    pub fn to_hide(&mut self, world: &World, shown: impl Fn(&Uuid) -> bool, now: Instant) -> Vec<(Uuid, bool)> {
        if self.checked.is_some_and(|t| now.duration_since(t) < HIDE_CHECK) {
            return Vec::new();
        }
        self.checked = Some(now);
        let Some(handle) = world.main().map(|r| r.handle) else {
            return Vec::new();
        };
        let seat = seat_root(world);
        // (what is hidden has left the world: anything still in it is new,
        // or came back at a teleport, which clears temporary derenders)
        let mut out = Vec::new();
        for (_, o) in world.objects.iter() {
            if o.key.region != handle
                || o.parent_id != 0
                || o.key.local_id >= FIRST_LOCAL_ID
                || o.full_id == world.agent_id
                || o.full_id.is_nil()
                || Some(o.key.local_id) == seat
                || o.is_client_preview()
                || shown(&o.full_id)
            {
                continue;
            }
            self.hidden.insert(o.full_id);
            out.push((o.full_id, o.is_avatar()));
        }
        out
    }

    /// The visit ends: what to show again, and our land as it was.
    pub fn end(self, handle: RegionHandle) -> (Vec<Uuid>, Option<NetEvent>) {
        log::info!("shared instance: back to our own place");
        (self.hidden.into_iter().collect(), self.land.and_then(|l| l.restore(handle)))
    }
}

/// The local id of the root of what our avatar sits on, if anything.
fn seat_root(world: &World) -> Option<u32> {
    let avatar = world.objects.index_of_uuid(&world.agent_id).and_then(|i| world.objects.get(i))?;
    let mut key = avatar.key;
    let mut parent = avatar.parent_id;
    // a linkset is never deeper than this
    for _ in 0..8 {
        if parent == 0 {
            return (key.local_id != avatar.key.local_id).then_some(key.local_id);
        }
        key.local_id = parent;
        parent = world.objects.index_of(&key).and_then(|i| world.objects.get(i))?.parent_id;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terrain_row_round_trips_into_patches() {
        let mut map = crate::world::terrain::Heightmap::new(256, 256);
        for (i, v) in map.heights.iter_mut().enumerate() {
            *v = (i % 256) as f32 + (i / 256) as f32 * 0.5;
        }
        let rows = terrain_rows(&map, Uuid::from_u128(1));
        assert_eq!(rows.len(), 16);
        let Msg::Terrain { row, size_x, heights, .. } = &rows[3] else {
            panic!("a terrain row");
        };
        let Some(NetEvent::Terrain { patches, .. }) = terrain_event(7, *row, *size_x, heights) else {
            panic!("patches");
        };
        assert_eq!(patches.len(), 16);
        let p = &patches[2];
        assert_eq!((p.x, p.y), (2, 3));
        // first height of patch (2, 3): x = 32, y = 48
        assert_eq!(p.heights[0], 32.0 + 48.0 * 0.5);
        assert!(wire::valid(&rows[3]));
    }
}
