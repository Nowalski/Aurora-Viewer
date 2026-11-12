//! Other peers of a shared instance, turned into the world events the network
//! would produce for real avatars: each peer appears as an avatar object with
//! its movement, animations, appearance and nearby chat. The viewer applies
//! the returned events with `world.apply`, like any simulator message.

use super::capture::decode_appearance;
use super::wire::{Msg, PeerState};
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
const FIRST_LOCAL_ID: u32 = 0xA15A_0001;

/// Same size as the avatar of the demo (`demo::action_avatar`).
const AVATAR_SCALE: Vec3 = Vec3::new(0.45, 0.6, 1.9);

pub struct Mirror {
    peers: HashMap<Uuid, Peer>,
    next_local_id: u32,
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
        }
    }

    /// Events for one message of a peer. A State or a Bye of an unknown peer
    /// (no Hello yet) gives nothing.
    pub fn apply(&mut self, msg: &Msg, handle: RegionHandle) -> Vec<NetEvent> {
        match msg {
            Msg::Hello { id, name } => self.hello(*id, name),
            Msg::State(state) => self.state(state, handle),
            Msg::Chat { id, text, chat_type } => self.chat(*id, text, *chat_type),
            Msg::Bye { id } => self.bye(*id, handle),
        }
    }

    /// Removes every mirrored avatar (the instance ended or we left it).
    pub fn clear(&mut self, handle: RegionHandle) -> Vec<NetEvent> {
        let local_ids: Vec<u32> = self.peers.values().filter(|p| p.object).map(|p| p.local_id).collect();
        self.peers.clear();
        if local_ids.is_empty() {
            Vec::new()
        } else {
            vec![NetEvent::ObjectsKilled { handle, local_ids }]
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    fn hello(&mut self, id: Uuid, name: &str) -> Vec<NetEvent> {
        let name = clean_name(name);
        if let Some(peer) = self.peers.get_mut(&id) {
            // Repeated Hello: only the name may have changed.
            peer.name = name;
            return Vec::new();
        }
        let notice = system_notice(aurora_i18n::tr!("instance-peer-joined", name = name.as_str()));
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
            },
        );
        vec![notice]
    }

    fn state(&mut self, s: &PeerState, handle: RegionHandle) -> Vec<NetEvent> {
        let Some(peer) = self.peers.get_mut(&s.id) else {
            return Vec::new();
        };
        let position = finite_vec(s.pos);
        let rotation = finite_rotation(s.rot);
        let velocity = finite_vec(s.vel);
        peer.position = position;
        peer.rotation = rotation;
        peer.velocity = velocity;

        let mut events = Vec::with_capacity(3);
        if peer.object {
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
            peer.object = true;
            events.push(NetEvent::ObjectUpdates {
                handle,
                objects: vec![avatar_object(peer.local_id, s.id, &peer.name, position, rotation, velocity)],
            });
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
        if peer.object {
            events.push(NetEvent::ObjectsKilled {
                handle,
                local_ids: vec![peer.local_id],
            });
        }
        events.push(system_notice(aurora_i18n::tr!("instance-peer-left", name = peer.name.as_str())));
        events
    }
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
