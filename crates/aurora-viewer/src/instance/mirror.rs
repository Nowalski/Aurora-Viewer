//! Other peers of a shared instance, turned into the world events the network
//! would produce for real avatars: each peer appears as an avatar object with
//! its movement, animations, appearance and nearby chat. The viewer applies
//! the returned events with `world.apply`, like any simulator message.

use super::capture::decode_appearance;
use super::wire::{Msg, PeerState, Worn};
use super::worn::{decode_te, prim_update};
use aurora_net::objects::{AttachedSound, ObjectUpdate, ParticleUpdate, TerseUpdate};
use aurora_net::{ChatMessage, ChatSourceType, ChatType, NetEvent, RegionHandle};
use aurora_prim::ExtraParams;
use aurora_prim::VolumeParams;
use aurora_prim::params::LL_PCODE_LEGACY_AVATAR;
use glam::{Quat, Vec3};
use std::collections::HashMap;
use uuid::Uuid;

/// Local ids of the mirrored avatars: far from the ids the simulator hands
/// out, so they never collide with the region's objects.
pub(super) const FIRST_LOCAL_ID: u32 = 0xA15A_0001;

/// Same size as the avatar of the demo (`demo::action_avatar`).
const AVATAR_SCALE: Vec3 = Vec3::new(0.45, 0.6, 1.9);

pub struct Mirror {
    peers: HashMap<Uuid, Peer>,
    next_local_id: u32,
    /// The host's place while we visit it: our local id and full id for
    /// each of its prims (by its key), and the local ids of each linkset.
    thing_ids: HashMap<u32, (u32, Uuid)>,
    things: HashMap<u32, Vec<u32>>,
    /// The latest message of each linkset and land row, to place them
    /// again when the height offset changes.
    thing_msgs: HashMap<u32, Worn>,
    land_rows: std::collections::BTreeMap<u32, (u32, String)>,
    /// Added to the heights of what the host's place shows (its land, its
    /// linksets, its people): the visit puts the host's ground at ours where
    /// we meet, whatever height each stands at in its own region.
    offset_z: f32,
}

struct Peer {
    name: String,
    local_id: u32,
    /// The avatar object exists in the world: the first State created it.
    object: bool,
    position: Vec3,
    rotation: Quat,
    velocity: Vec3,
    /// Animations last sent to the world; `None` before the first State.
    anims: Option<Vec<(Uuid, i32)>>,
    /// An Aurora peer (joined / left notices), not someone of the host's place.
    announced: bool,
    /// The avatar object's texture entry last applied (base64, as sent).
    body_te: Option<String>,
    /// What it wears, by root key: the latest linkset, applied once the
    /// avatar object exists.
    worn: HashMap<u32, Worn>,
    /// Our local id and full id for each of its worn prims (by its key).
    worn_ids: HashMap<u32, (u32, Uuid)>,
    /// The local ids each linkset was given, by root key.
    worn_local: HashMap<u32, Vec<u32>>,
}

impl Peer {
    /// Local ids of the avatar and everything it wears, to remove them.
    fn all_local_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.worn_local.values().flatten().copied().collect();
        if self.object {
            ids.push(self.local_id);
        }
        ids
    }
}

impl Default for Mirror {
    fn default() -> Self {
        Self::new()
    }
}

impl Mirror {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            next_local_id: FIRST_LOCAL_ID,
            thing_ids: HashMap::new(),
            things: HashMap::new(),
            thing_msgs: HashMap::new(),
            land_rows: Default::default(),
            offset_z: 0.0,
        }
    }

    /// Events for one message of a peer. A State or a Bye of an unknown peer
    /// (no Hello yet) gives nothing.
    pub fn apply(&mut self, msg: &Msg, handle: RegionHandle) -> Vec<NetEvent> {
        match msg {
            Msg::Hello { id, name } => self.hello(*id, name, true),
            Msg::Present { id, name } => self.hello(*id, name, false),
            Msg::State(state) => self.state(state, handle),
            Msg::Chat { id, text, chat_type } => self.chat(*id, text, *chat_type),
            Msg::Bye { id } => self.bye(*id, handle),
            Msg::Worn(worn) => self.worn(worn, handle),
            Msg::Unworn { id, root } => self.unworn(*id, *root, handle),
            Msg::Thing(thing) => self.thing(thing, handle),
            Msg::Gone { root, .. } => self.gone(*root, handle),
            Msg::Terrain { row, size_x, heights, .. } => {
                self.land_rows.insert(*row, (*size_x, heights.clone()));
                super::place::terrain_event(handle, *row, *size_x, heights, self.offset_z)
                    .into_iter()
                    .collect()
            }
        }
    }

    /// Removes the host's place (we stop visiting it).
    pub fn clear_place(&mut self, handle: RegionHandle) -> Vec<NetEvent> {
        let local_ids: Vec<u32> = self.things.drain().flat_map(|(_, l)| l).collect();
        self.thing_ids.clear();
        self.thing_msgs.clear();
        self.land_rows.clear();
        self.offset_z = 0.0;
        if local_ids.is_empty() {
            Vec::new()
        } else {
            vec![NetEvent::ObjectsKilled { handle, local_ids }]
        }
    }

    /// Removes every mirrored avatar (the instance ended or we left it).
    pub fn clear(&mut self, handle: RegionHandle) -> Vec<NetEvent> {
        let mut local_ids: Vec<u32> = self.peers.values().flat_map(Peer::all_local_ids).collect();
        self.peers.clear();
        local_ids.extend(self.things.drain().flat_map(|(_, l)| l));
        local_ids.sort_unstable();
        self.thing_ids.clear();
        self.thing_msgs.clear();
        self.land_rows.clear();
        self.offset_z = 0.0;
        if local_ids.is_empty() {
            Vec::new()
        } else {
            vec![NetEvent::ObjectsKilled { handle, local_ids }]
        }
    }

    /// Place the host's place `dz` higher than it stands in the host's
    /// region: its land and linksets again now, its people at their next
    /// state.
    pub fn set_offset_z(&mut self, dz: f32, handle: RegionHandle) -> Vec<NetEvent> {
        if !dz.is_finite() || (dz - self.offset_z).abs() < 0.01 {
            return Vec::new();
        }
        self.offset_z = dz;
        let mut events: Vec<NetEvent> = self
            .land_rows
            .iter()
            .filter_map(|(row, (size_x, heights))| super::place::terrain_event(handle, *row, *size_x, heights, dz))
            .collect();
        let things: Vec<Worn> = self.thing_msgs.values().cloned().collect();
        for thing in &things {
            events.extend(self.thing(thing, handle));
        }
        events
    }

    /// The boxes of the host's place, as we see it (for `ground::steer`).
    pub fn place_bounds(&self) -> Vec<super::ground::Bounds> {
        self.thing_msgs
            .values()
            .flat_map(|t| super::ground::linkset_bounds(t, self.offset_z))
            .collect()
    }

    /// The height added to the host's place.
    pub fn offset_z(&self) -> f32 {
        self.offset_z
    }

    /// Where a peer stands in the host's region, as it sent it.
    pub fn position_of(&self, id: &Uuid) -> Option<Vec3> {
        let peer = self.peers.get(id).filter(|p| p.object)?;
        Some(peer.position - Vec3::Z * self.offset_z)
    }

    /// Whether `id` is shown by the instance (rather than by the region).
    pub fn contains(&self, id: &Uuid) -> bool {
        self.peers.contains_key(id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// `notice`: an Aurora peer joining is announced; the people of the
    /// host's place just appear, as they would on arrival.
    fn hello(&mut self, id: Uuid, name: &str, notice: bool) -> Vec<NetEvent> {
        let name = clean_name(name);
        if let Some(peer) = self.peers.get_mut(&id) {
            // Repeated Hello: only the name may have changed.
            peer.name = name;
            return Vec::new();
        }
        let joined = notice.then(|| system_notice(aurora_i18n::tr!("instance-peer-joined", name = name.as_str())));
        let local_id = self.next_local_id;
        self.next_local_id = self.next_local_id.wrapping_add(1);
        self.peers.insert(
            id,
            Peer {
                name,
                local_id,
                object: false,
                position: Vec3::ZERO,
                rotation: Quat::IDENTITY,
                velocity: Vec3::ZERO,
                anims: None,
                announced: notice,
                body_te: None,
                worn: HashMap::new(),
                worn_ids: HashMap::new(),
                worn_local: HashMap::new(),
            },
        );
        joined.into_iter().collect()
    }

    fn state(&mut self, s: &PeerState, handle: RegionHandle) -> Vec<NetEvent> {
        let Some(peer) = self.peers.get_mut(&s.id) else {
            return Vec::new();
        };
        let position = finite_vec(s.pos) + Vec3::Z * self.offset_z;
        let rotation = finite_rotation(s.rot);
        let velocity = finite_vec(s.vel);
        peer.position = position;
        peer.rotation = rotation;
        peer.velocity = velocity;

        let mut events = Vec::with_capacity(3);
        let new_te = s.body_te.as_ref().filter(|te| peer.body_te.as_ref() != Some(*te));
        if peer.object && new_te.is_none() {
            events.push(NetEvent::TerseUpdates {
                handle,
                updates: vec![TerseUpdate {
                    local_id: peer.local_id,
                    state: 0,
                    is_avatar: true,
                    foot_plane: None,
                    position,
                    velocity,
                    acceleration: Vec3::ZERO,
                    rotation,
                    angular_velocity: Vec3::ZERO,
                    texture_entry: None,
                }],
            });
        } else {
            // first state, or new bakes in the object's own texture entry: a
            // full update, as the simulator sends when they change
            if let Some(te) = new_te {
                peer.body_te = Some(te.clone());
            }
            let mut object = avatar_object(peer.local_id, s.id, &peer.name, position, rotation, velocity);
            object.texture_entry = peer.body_te.as_deref().and_then(decode_te).map(Into::into);
            events.push(NetEvent::ObjectUpdates {
                handle,
                objects: vec![object],
            });
            if !peer.object {
                peer.object = true;
                let roots: Vec<u32> = peer.worn.keys().copied().collect();
                for root in roots {
                    events.extend(linkset_events(peer, &mut self.next_local_id, s.id, root, handle));
                }
            }
        }

        if peer.anims.as_deref() != Some(s.anims.as_slice()) {
            peer.anims = Some(s.anims.clone());
            events.push(NetEvent::AvatarAnimations {
                avatar: s.id,
                anims: s.anims.clone(),
                sources: Vec::new(),
            });
        }

        // Sent again now and then by the peer, so a late arrival gets it too.
        if let Some(appearance) = s.appearance.as_ref().and_then(|a| decode_appearance(s.id, a)) {
            events.push(NetEvent::Appearance(appearance));
        }
        events
    }

    fn chat(&self, id: Uuid, text: &str, chat_type: u8) -> Vec<NetEvent> {
        let Some(peer) = self.peers.get(&id) else {
            return Vec::new();
        };
        // Only the three nearby chat types a peer can say; anything else is
        // shown as normal speech rather than read as typing indicators.
        let chat_type = match chat_type {
            0 => ChatType::Whisper,
            2 => ChatType::Shout,
            _ => ChatType::Normal,
        };
        vec![NetEvent::Chat(ChatMessage {
            from_name: peer.name.clone(),
            source_id: id,
            owner_id: id,
            source_type: ChatSourceType::Agent,
            chat_type,
            position: peer.position,
            message: text.to_owned(),
        })]
    }

    fn bye(&mut self, id: Uuid, handle: RegionHandle) -> Vec<NetEvent> {
        let Some(peer) = self.peers.remove(&id) else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(2);
        let local_ids = peer.all_local_ids();
        if !local_ids.is_empty() {
            events.push(NetEvent::ObjectsKilled { handle, local_ids });
        }
        if peer.announced {
            events.push(system_notice(aurora_i18n::tr!("instance-peer-left", name = peer.name.as_str())));
        }
        events
    }
}

impl Mirror {
    fn worn(&mut self, worn: &Worn, handle: RegionHandle) -> Vec<NetEvent> {
        let Some(peer) = self.peers.get_mut(&worn.id) else {
            return Vec::new();
        };
        peer.worn.insert(worn.root, worn.clone());
        if !peer.object {
            return Vec::new();
        }
        linkset_events(peer, &mut self.next_local_id, worn.id, worn.root, handle)
    }

    fn unworn(&mut self, id: Uuid, root: u32, handle: RegionHandle) -> Vec<NetEvent> {
        let Some(peer) = self.peers.get_mut(&id) else {
            return Vec::new();
        };
        if let Some(worn) = peer.worn.remove(&root) {
            for p in &worn.prims {
                peer.worn_ids.remove(&p.key);
            }
        }
        match peer.worn_local.remove(&root) {
            Some(local_ids) if !local_ids.is_empty() => vec![NetEvent::ObjectsKilled { handle, local_ids }],
            _ => Vec::new(),
        }
    }
}

impl Mirror {
    /// A linkset of the host's place, standing in the region (the root's
    /// parent is none). It carries no rights here: the host owns nothing
    /// we could edit, copy or take, as for any object of someone else.
    fn thing(&mut self, thing: &Worn, handle: RegionHandle) -> Vec<NetEvent> {
        self.thing_msgs.insert(thing.root, thing.clone());
        let mut objects = Vec::with_capacity(thing.prims.len());
        let mut locals = Vec::with_capacity(thing.prims.len());
        let mut root_local = None;
        for p in &thing.prims {
            let next = &mut self.next_local_id;
            let (local_id, full_id) = *self.thing_ids.entry(p.key).or_insert_with(|| {
                let local = *next;
                *next = next.wrapping_add(1);
                (local, Uuid::new_v4())
            });
            match prim_update(p, local_id, full_id, root_local.unwrap_or(0), thing.id) {
                Some(mut update) => {
                    // the root stands in the region; the children follow it
                    if root_local.is_none() {
                        update.position.z += self.offset_z;
                    }
                    objects.push(update);
                    locals.push(local_id);
                    root_local.get_or_insert(local_id);
                }
                None if root_local.is_none() => break,
                None => {}
            }
        }
        let before = self.things.insert(thing.root, locals.clone()).unwrap_or_default();
        let gone: Vec<u32> = before.into_iter().filter(|l| !locals.contains(l)).collect();
        let mut events = Vec::with_capacity(2);
        if !gone.is_empty() {
            events.push(NetEvent::ObjectsKilled { handle, local_ids: gone });
        }
        if !objects.is_empty() {
            events.push(NetEvent::ObjectUpdates { handle, objects });
        }
        events
    }

    fn gone(&mut self, root: u32, handle: RegionHandle) -> Vec<NetEvent> {
        self.thing_msgs.remove(&root);
        match self.things.remove(&root) {
            Some(local_ids) if !local_ids.is_empty() => {
                self.thing_ids.retain(|_, (l, _)| !local_ids.contains(l));
                vec![NetEvent::ObjectsKilled { handle, local_ids }]
            }
            _ => Vec::new(),
        }
    }
}

/// The world events of a peer's linkset as it is now: full updates of its
/// prims, and the removal of those it no longer has.
fn linkset_events(peer: &mut Peer, next_local_id: &mut u32, avatar: Uuid, root: u32, handle: RegionHandle) -> Vec<NetEvent> {
    let Some(worn) = peer.worn.get(&root) else {
        return Vec::new();
    };
    let mut objects = Vec::with_capacity(worn.prims.len());
    let mut locals = Vec::with_capacity(worn.prims.len());
    let mut root_local = None;
    for p in &worn.prims {
        let (local_id, full_id) = *peer.worn_ids.entry(p.key).or_insert_with(|| {
            let local = *next_local_id;
            *next_local_id = next_local_id.wrapping_add(1);
            (local, Uuid::new_v4())
        });
        // the root hangs on the avatar, the others on the root
        let parent = root_local.unwrap_or(peer.local_id);
        match prim_update(p, local_id, full_id, parent, avatar) {
            Some(update) => {
                objects.push(update);
                locals.push(local_id);
                root_local.get_or_insert(local_id);
            }
            None if root_local.is_none() => break,
            None => {}
        }
    }
    let before = peer.worn_local.insert(root, locals.clone()).unwrap_or_default();
    let gone: Vec<u32> = before.into_iter().filter(|l| !locals.contains(l)).collect();
    let mut events = Vec::with_capacity(2);
    if !gone.is_empty() {
        events.push(NetEvent::ObjectsKilled { handle, local_ids: gone });
    }
    if !objects.is_empty() {
        events.push(NetEvent::ObjectUpdates { handle, objects });
    }
    events
}

/// A chat line from the instance itself rather than from an avatar.
fn system_notice(message: String) -> NetEvent {
    NetEvent::Chat(ChatMessage {
        from_name: String::new(),
        source_id: Uuid::nil(),
        owner_id: Uuid::nil(),
        source_type: ChatSourceType::System,
        chat_type: ChatType::Normal,
        position: Vec3::ZERO,
        message,
    })
}

/// Full avatar object, as the simulator describes an avatar on arrival.
fn avatar_object(local_id: u32, id: Uuid, name: &str, position: Vec3, rotation: Quat, velocity: Vec3) -> ObjectUpdate {
    let (first, last) = split_name(name);
    ObjectUpdate {
        local_id,
        full_id: id,
        parent_id: 0,
        pcode: LL_PCODE_LEGACY_AVATAR,
        state: 0,
        crc: 0,
        material: 0,
        click_action: 0,
        scale: AVATAR_SCALE,
        position,
        rotation,
        velocity,
        acceleration: Vec3::ZERO,
        angular_velocity: Vec3::ZERO,
        update_flags: 0,
        owner_id: id,
        volume: VolumeParams::default(),
        texture_entry: None,
        extra: ExtraParams::default(),
        name_values: format!("FirstName STRING RW SV {first}\nLastName STRING RW SV {last}"),
        text: String::new(),
        text_color: [0; 4],
        media_url: String::new(),
        tree_species: None,
        texture_anim: None,
        foot_plane: None,
        particles: ParticleUpdate::Keep,
        sound: AttachedSound::default(),
    }
}

/// "First Last" at the first space; a single word gets the "Resident" last name.
fn split_name(name: &str) -> (&str, &str) {
    match name.split_once(' ') {
        Some((first, last)) if !last.trim().is_empty() => (first, last.trim()),
        _ => (name, "Resident"),
    }
}

/// The name goes into `name_values`, where a line break or a control
/// character would break the format: they are removed.
fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_owned()
}

/// Peer positions come from the network: a non-finite value is dropped
/// rather than sent to the world.
fn finite_vec(a: [f32; 3]) -> Vec3 {
    let v = Vec3::from_array(a);
    if v.is_finite() { v } else { Vec3::ZERO }
}

fn finite_rotation(a: [f32; 4]) -> Quat {
    let q = Quat::from_array(a);
    if q.is_finite() && q.length_squared() > 1e-12 {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDLE: RegionHandle = 7;

    fn peer_id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn hello(id: Uuid, name: &str) -> Msg {
        Msg::Hello { id, name: name.to_owned() }
    }

    fn state(id: Uuid, anims: Vec<(Uuid, i32)>) -> Msg {
        Msg::State(PeerState {
            id,
            pos: [128.0, 128.0, 22.0],
            rot: [0.0, 0.0, 0.0, 1.0],
            vel: [0.0; 3],
            flying: false,
            anims,
            appearance: None,
            body_te: None,
        })
    }

    fn count(events: &[NetEvent], f: impl Fn(&NetEvent) -> bool) -> usize {
        events.iter().filter(|e| f(e)).count()
    }

    #[test]
    fn hello_then_state_creates_one_avatar_object() {
        let mut m = Mirror::new();
        let a = peer_id(1);
        let joined = m.apply(&hello(a, "Ada Lovelace"), HANDLE);
        assert_eq!(joined.len(), 1);
        assert!(matches!(joined[0], NetEvent::Chat(_)));

        let events = m.apply(&state(a, Vec::new()), HANDLE);
        assert_eq!(count(&events, |e| matches!(e, NetEvent::ObjectUpdates { .. })), 1);
        assert_eq!(count(&events, |e| matches!(e, NetEvent::TerseUpdates { .. })), 0);
        let NetEvent::ObjectUpdates { handle, objects } = &events[0] else {
            panic!("first event must be the avatar object");
        };
        assert_eq!(*handle, HANDLE);
        assert_eq!(objects.len(), 1);
        let avatar = &objects[0];
        assert!(avatar.is_avatar());
        assert_eq!(avatar.pcode, LL_PCODE_LEGACY_AVATAR);
        assert_eq!(avatar.local_id, FIRST_LOCAL_ID);
        assert_eq!(avatar.full_id, a);
        assert_eq!(avatar.owner_id, a);
        assert_eq!(avatar.name_value("FirstName").as_deref(), Some("Ada"));
        assert_eq!(avatar.name_value("LastName").as_deref(), Some("Lovelace"));
    }

    #[test]
    fn single_word_name_gets_resident_last_name() {
        let mut m = Mirror::new();
        let a = peer_id(2);
        m.apply(&hello(a, "Ada"), HANDLE);
        let events = m.apply(&state(a, Vec::new()), HANDLE);
        let NetEvent::ObjectUpdates { objects, .. } = &events[0] else {
            panic!("first event must be the avatar object");
        };
        assert_eq!(objects[0].name_value("LastName").as_deref(), Some("Resident"));
    }

    #[test]
    fn second_state_is_terse() {
        let mut m = Mirror::new();
        let a = peer_id(1);
        m.apply(&hello(a, "Ada Lovelace"), HANDLE);
        m.apply(&state(a, Vec::new()), HANDLE);
        let events = m.apply(&state(a, Vec::new()), HANDLE);
        assert_eq!(count(&events, |e| matches!(e, NetEvent::ObjectUpdates { .. })), 0);
        let NetEvent::TerseUpdates { updates, .. } = &events[0] else {
            panic!("a known peer's state is a terse update");
        };
        assert_eq!(updates.len(), 1);
        assert!(updates[0].is_avatar);
        assert_eq!(updates[0].local_id, FIRST_LOCAL_ID);
    }

    #[test]
    fn repeated_hello_only_updates_the_name() {
        let mut m = Mirror::new();
        let a = peer_id(1);
        m.apply(&hello(a, "Ada"), HANDLE);
        assert!(m.apply(&hello(a, "Ada Lovelace"), HANDLE).is_empty());
        assert_eq!(m.len(), 1);
        let chat = m.apply(
            &Msg::Chat {
                id: a,
                text: "hi".into(),
                chat_type: 1,
            },
            HANDLE,
        );
        let NetEvent::Chat(c) = &chat[0] else {
            panic!("chat expected");
        };
        assert_eq!(c.from_name, "Ada Lovelace");
    }

    #[test]
    fn people_of_the_host_place_come_and_go_without_notices() {
        let mut m = Mirror::new();
        let id = Uuid::from_u128(77);
        let present = Msg::Present {
            id,
            name: "Someone Resident".into(),
        };
        assert!(m.apply(&present, HANDLE).is_empty());
        assert!(m.contains(&id));
        let created = m.apply(&state(id, Vec::new()), HANDLE);
        assert_eq!(count(&created, |e| matches!(e, NetEvent::ObjectUpdates { .. })), 1);
        let gone = m.apply(&Msg::Bye { id }, HANDLE);
        assert_eq!(count(&gone, |e| matches!(e, NetEvent::Chat(_))), 0);
        assert_eq!(count(&gone, |e| matches!(e, NetEvent::ObjectsKilled { .. })), 1);
        assert!(!m.contains(&id));
    }

    fn worn(id: Uuid, keys: &[u32]) -> Msg {
        let prim = |key| super::super::wire::WornPrim {
            key,
            state: 0x40,
            pcode: aurora_prim::params::LL_PCODE_VOLUME,
            material: 3,
            pos: [0.0; 3],
            rot: [0.0, 0.0, 0.0, 1.0],
            scale: [0.1; 3],
            shape: [0; 18],
            te: String::new(),
            extra: "AA==".into(),
        };
        Msg::Worn(Worn {
            id,
            root: keys[0],
            prims: keys.iter().map(|&k| prim(k)).collect(),
        })
    }

    fn updates(events: &[NetEvent]) -> Vec<(u32, u32)> {
        events
            .iter()
            .filter_map(|e| match e {
                NetEvent::ObjectUpdates { objects, .. } => Some(objects.iter().map(|o| (o.local_id, o.parent_id))),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn worn_linksets_hang_on_the_avatar_and_go_with_it() {
        let mut m = Mirror::new();
        let id = peer_id(9);
        m.apply(&hello(id, "Ada Lovelace"), HANDLE);
        // before the avatar exists it waits
        assert!(m.apply(&worn(id, &[50, 51, 52]), HANDLE).is_empty());
        let created = updates(&m.apply(&state(id, Vec::new()), HANDLE));
        let avatar = FIRST_LOCAL_ID;
        assert_eq!(created.len(), 4);
        assert_eq!(created[0], (avatar, 0));
        let root = created[1].0;
        assert_eq!(created[1].1, avatar);
        assert!(created[2..].iter().all(|&(_, parent)| parent == root));
        // a child taken out of the linkset is removed, the others keep their ids
        let changed = m.apply(&worn(id, &[50, 51]), HANDLE);
        assert_eq!(updates(&changed), vec![(root, avatar), (created[2].0, root)]);
        assert_eq!(
            count(
                &changed,
                |e| matches!(e, NetEvent::ObjectsKilled { local_ids, .. } if local_ids == &vec![created[3].0])
            ),
            1
        );
        // a second linkset, then taken off
        m.apply(&worn(id, &[60]), HANDLE);
        let off = m.apply(&Msg::Unworn { id, root: 60 }, HANDLE);
        assert_eq!(
            count(
                &off,
                |e| matches!(e, NetEvent::ObjectsKilled { local_ids, .. } if local_ids.len() == 1)
            ),
            1
        );
        // leaving takes the avatar and what it still wears
        let gone = m.apply(&Msg::Bye { id }, HANDLE);
        let killed: usize = gone
            .iter()
            .map(|e| match e {
                NetEvent::ObjectsKilled { local_ids, .. } => local_ids.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(killed, 3);
    }

    #[test]
    fn the_hosts_place_stands_in_the_region_and_leaves_with_the_visit() {
        let mut m = Mirror::new();
        let host = peer_id(3);
        // no Hello needed: the place is not a peer
        let Msg::Worn(w) = worn(host, &[70, 71]) else {
            panic!("a linkset");
        };
        let created = updates(&m.apply(&Msg::Thing(w), HANDLE));
        assert_eq!(created.len(), 2);
        let root = created[0].0;
        assert_eq!(created[0].1, 0);
        assert_eq!(created[1].1, root);
        assert!(root >= FIRST_LOCAL_ID);
        let Msg::Worn(w) = worn(host, &[80]) else {
            panic!("a linkset");
        };
        m.apply(&Msg::Thing(w), HANDLE);
        let gone = m.apply(&Msg::Gone { id: host, root: 70 }, HANDLE);
        assert_eq!(
            count(
                &gone,
                |e| matches!(e, NetEvent::ObjectsKilled { local_ids, .. } if local_ids.len() == 2)
            ),
            1
        );
        // raised to meet our ground: the root again, 5 m higher
        let raised = m.set_offset_z(5.0, HANDLE);
        let z: Vec<f32> = raised
            .iter()
            .filter_map(|e| match e {
                NetEvent::ObjectUpdates { objects, .. } => Some(objects.iter().map(|o| o.position.z)),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(z, vec![5.0]);
        assert!(m.set_offset_z(5.0, HANDLE).is_empty());
        let cleared = m.clear_place(HANDLE);
        assert_eq!(
            count(
                &cleared,
                |e| matches!(e, NetEvent::ObjectsKilled { local_ids, .. } if local_ids.len() == 1)
            ),
            1
        );
        assert!(m.clear_place(HANDLE).is_empty());
    }

    #[test]
    fn bye_kills_the_right_local_id() {
        let mut m = Mirror::new();
        let (a, b) = (peer_id(1), peer_id(2));
        m.apply(&hello(a, "Ada Lovelace"), HANDLE);
        m.apply(&state(a, Vec::new()), HANDLE);
        m.apply(&hello(b, "Grace Hopper"), HANDLE);
        m.apply(&state(b, Vec::new()), HANDLE);

        let events = m.apply(&Msg::Bye { id: b }, HANDLE);
        let killed: Vec<Vec<u32>> = events
            .iter()
            .filter_map(|e| match e {
                NetEvent::ObjectsKilled { local_ids, .. } => Some(local_ids.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(killed, vec![vec![FIRST_LOCAL_ID + 1]]);
        assert_eq!(count(&events, |e| matches!(e, NetEvent::Chat(_))), 1);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn bye_of_a_peer_without_object_only_says_goodbye() {
        let mut m = Mirror::new();
        let a = peer_id(1);
        m.apply(&hello(a, "Ada"), HANDLE);
        let events = m.apply(&Msg::Bye { id: a }, HANDLE);
        assert_eq!(count(&events, |e| matches!(e, NetEvent::ObjectsKilled { .. })), 0);
        assert_eq!(count(&events, |e| matches!(e, NetEvent::Chat(_))), 1);
        assert!(m.is_empty());
    }

    #[test]
    fn chat_from_unknown_peer_is_ignored() {
        let mut m = Mirror::new();
        let chat = Msg::Chat {
            id: peer_id(9),
            text: "hello".into(),
            chat_type: 1,
        };
        assert!(m.apply(&chat, HANDLE).is_empty());
    }

    #[test]
    fn state_from_unknown_peer_is_ignored() {
        let mut m = Mirror::new();
        assert!(m.apply(&state(peer_id(9), Vec::new()), HANDLE).is_empty());
        assert!(m.is_empty());
    }

    #[test]
    fn anims_are_only_resent_when_changed() {
        let mut m = Mirror::new();
        let a = peer_id(1);
        let (walk, wave) = (peer_id(100), peer_id(101));
        m.apply(&hello(a, "Ada Lovelace"), HANDLE);

        let anim_events = |events: &[NetEvent]| count(events, |e| matches!(e, NetEvent::AvatarAnimations { .. }));
        assert_eq!(anim_events(&m.apply(&state(a, vec![(walk, 1)]), HANDLE)), 1);
        assert_eq!(anim_events(&m.apply(&state(a, vec![(walk, 1)]), HANDLE)), 0);
        assert_eq!(anim_events(&m.apply(&state(a, vec![(walk, 1), (wave, 2)]), HANDLE)), 1);
        assert_eq!(anim_events(&m.apply(&state(a, vec![(walk, 1), (wave, 2)]), HANDLE)), 0);
    }

    #[test]
    fn clear_kills_every_mirrored_avatar() {
        let mut m = Mirror::new();
        let (a, b) = (peer_id(1), peer_id(2));
        m.apply(&hello(a, "Ada"), HANDLE);
        m.apply(&state(a, Vec::new()), HANDLE);
        m.apply(&hello(b, "Grace"), HANDLE);
        m.apply(&state(b, Vec::new()), HANDLE);

        let events = m.clear(HANDLE);
        let [NetEvent::ObjectsKilled { local_ids, .. }] = events.as_slice() else {
            panic!("one ObjectsKilled expected");
        };
        assert_eq!(local_ids, &vec![FIRST_LOCAL_ID, FIRST_LOCAL_ID + 1]);
        assert!(m.is_empty());
        assert!(m.clear(HANDLE).is_empty());
    }
}
