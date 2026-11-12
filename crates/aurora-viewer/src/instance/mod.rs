//! Shared instance (experimental, AURORA_INSTANCE_HOST / AURORA_INSTANCE_JOIN):
//! Aurora viewers in the same place show each other's avatars, movement,
//! animations, appearance and nearby chat over a direct TCP link, one
//! viewer hosting. Only ids and small state are exchanged, never assets:
//! each viewer loads what it shows by itself. The peers' avatars enter the
//! world through the ordinary event path (`World::apply`), so they draw,
//! animate and chat like any other avatar.

pub mod capture;
pub mod client;
pub mod crowd;
pub mod demo_peer;
pub mod hub;
pub mod mirror;
#[cfg(test)]
mod tests;
pub mod wire;
pub mod worn;

use std::time::{Duration, Instant};

use aurora_net::{ChatMessage, ChatSourceType, ChatType, NetEvent};
use glam::Vec3;
use uuid::Uuid;

use crate::world::World;

/// How this viewer takes part.
enum Link {
    /// Hosting: the hub, and the people of our place it shares.
    Host(hub::Hub, crowd::Crowd),
    Guest(client::Client),
    /// AURORA_DEMO_INSTANCE=peer: a simulated peer, nothing on the network.
    Demo(Option<demo_peer::DemoPeer>),
}

/// What the environment asks for, read once at startup.
pub enum Request {
    Host { bind: String, port: u16 },
    Join(String),
    DemoPeer,
}

impl Request {
    pub fn from_env(demo: bool) -> Option<Request> {
        if let Ok(port) = std::env::var("AURORA_INSTANCE_HOST") {
            let bind = std::env::var("AURORA_INSTANCE_BIND").unwrap_or_else(|_| "127.0.0.1".into());
            return match port.trim().parse() {
                Ok(port) => Some(Request::Host { bind, port }),
                Err(_) => {
                    log::warn!("shared instance: AURORA_INSTANCE_HOST is not a port: {port}");
                    None
                }
            };
        }
        if let Ok(address) = std::env::var("AURORA_INSTANCE_JOIN") {
            return Some(Request::Join(address.trim().to_string()));
        }
        (demo && std::env::var("AURORA_DEMO_INSTANCE").is_ok_and(|v| v == "peer")).then_some(Request::DemoPeer)
    }
}

pub struct Instance {
    link: Link,
    /// Our id in the instance: the agent's id on a grid, a fresh one in the
    /// demo, where every window is the same demo agent.
    peer_id: Uuid,
    capture: capture::Capture,
    mirror: mirror::Mirror,
    /// What our own avatar wears.
    worn: worn::WornSender,
    lost_reported: bool,
    started: Instant,
}

impl Instance {
    /// Start what `request` asks for once the world has its region. Returns
    /// the instance, if any, and a notice for the nearby chat.
    pub fn start(request: Request, world: &World, demo: bool, now: Instant) -> (Option<Instance>, NetEvent) {
        let peer_id = if demo { Uuid::new_v4() } else { world.agent_id };
        let name = std::env::var("AURORA_INSTANCE_NAME")
            .ok()
            .map(|n| n.trim().chars().take(wire::MAX_NAME).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                if demo {
                    format!("Guest {}", &peer_id.simple().to_string()[..4])
                } else {
                    world.own_name()
                }
            });
        let hello = wire::Msg::Hello {
            id: peer_id,
            name: name.clone(),
        };
        let (link, text) = match request {
            Request::Host { bind, port } => match hub::Hub::start(&bind, port) {
                Ok(hub) => {
                    let port = hub.port().to_string();
                    hub.send(hello);
                    (
                        Link::Host(hub, crowd::Crowd::new()),
                        aurora_i18n::tr!("instance-hosting", port = port.as_str()),
                    )
                }
                Err(e) => {
                    let error = e.to_string();
                    return (
                        None,
                        notice(aurora_i18n::tr!("instance-failed", error = error.as_str()).to_string()),
                    );
                }
            },
            Request::Join(address) => match client::Client::connect(&address) {
                Ok(client) => {
                    client.send(hello);
                    (Link::Guest(client), aurora_i18n::tr!("instance-joined", address = address.as_str()))
                }
                Err(e) => {
                    let error = e.to_string();
                    return (
                        None,
                        notice(aurora_i18n::tr!("instance-failed", error = error.as_str()).to_string()),
                    );
                }
            },
            Request::DemoPeer => (Link::Demo(None), aurora_i18n::tr!("instance-joined", address = "demo")),
        };
        log::info!("shared instance: started as {name}");
        let instance = Instance {
            link,
            peer_id,
            capture: capture::Capture::new(),
            mirror: mirror::Mirror::new(),
            worn: worn::WornSender::default(),
            lost_reported: false,
            started: now,
        };
        (Some(instance), notice(text))
    }

    /// One frame: send our state when due, turn what the peers sent into
    /// world events.
    pub fn tick(&mut self, world: &World, now: Instant) -> Vec<NetEvent> {
        let Some(handle) = world.main().map(|r| r.handle) else {
            return Vec::new();
        };
        let state = self.capture.state(world, self.peer_id, now);
        // (after our first state, so the peers have our avatar to hang it on)
        let own_worn = match world.objects.index_of_uuid(&world.agent_id).and_then(|i| world.objects.get(i)) {
            Some(avatar) if self.capture.started() => self.worn.messages(world, self.peer_id, avatar, now),
            _ => Vec::new(),
        };
        let mut incoming = Vec::new();
        match &mut self.link {
            Link::Host(hub, crowd) => {
                if let Some(state) = state {
                    hub.send(wire::Msg::State(state));
                }
                for msg in own_worn {
                    hub.send(msg);
                }
                let mirror = &self.mirror;
                for msg in crowd.messages(world, |id| mirror.contains(id), now) {
                    hub.send(msg);
                }
                while let Some(msg) = hub.try_recv() {
                    incoming.push(msg);
                }
            }
            Link::Guest(client) => {
                if let Some(state) = state {
                    client.send(wire::Msg::State(state));
                }
                for msg in own_worn {
                    client.send(msg);
                }
                while let Some(msg) = client.try_recv() {
                    incoming.push(msg);
                }
            }
            Link::Demo(peer) => {
                // the simulated peer appears beside us once our own look is
                // known (it wears it), or after a few seconds without it
                if peer.is_none() {
                    let appearance = world.server_appearance().and_then(capture::encode_appearance);
                    if appearance.is_some() || now.duration_since(self.started) > Duration::from_secs(5) {
                        let centre = world.agent.position + Vec3::new(4.0, 0.0, 0.0);
                        *peer = Some(demo_peer::DemoPeer::new(centre, appearance));
                    }
                }
                if let Some(peer) = peer {
                    incoming.extend(peer.poll(now));
                }
            }
        }
        let mut events = Vec::new();
        // Someone the region already shows us (we are in the host's place
        // ourselves) stays the region's: the instance only adds who is missing.
        let native = |id: &Uuid| !self.mirror.contains(id) && world.objects.index_of_uuid(id).is_some();
        let incoming: Vec<_> = incoming
            .into_iter()
            .filter(|m| m.id() != self.peer_id && !native(&m.id()))
            .collect();
        for msg in &incoming {
            events.extend(self.mirror.apply(msg, handle));
        }
        if let Link::Guest(client) = &self.link
            && !client.connected()
            && !self.lost_reported
        {
            self.lost_reported = true;
            log::info!("shared instance: the host closed the link");
            events.extend(self.mirror.clear(handle));
            events.push(notice(aurora_i18n::tr!("instance-connection-lost").to_string()));
        }
        events
    }

    /// Our nearby chat goes to the peers too (channel 0 only: commands to
    /// scripts stay in the region).
    pub fn local_chat(&self, text: &str, chat_type: ChatType) {
        let chat_type = match chat_type {
            ChatType::Whisper => 0,
            ChatType::Normal => 1,
            ChatType::Shout => 2,
            _ => return,
        };
        if text.trim().is_empty() {
            return;
        }
        let text: String = text.chars().take(wire::MAX_CHAT).collect();
        let msg = wire::Msg::Chat {
            id: self.peer_id,
            text,
            chat_type,
        };
        match &self.link {
            Link::Host(hub, _) => hub.send(msg),
            Link::Guest(client) => client.send(msg),
            Link::Demo(_) => {}
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let bye = wire::Msg::Bye { id: self.peer_id };
        match &self.link {
            Link::Host(hub, _) => hub.send(bye),
            Link::Guest(client) => client.send(bye),
            Link::Demo(_) => {}
        }
    }
}

/// A line of the nearby chat from the viewer itself.
fn notice(message: String) -> NetEvent {
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
