//! `AURORA_DEMO_INSTANCE=peer`: a simulated second viewer, entirely offline, so
//! one demo window shows a mirrored avatar walking and dancing in place. It
//! produces the same `wire::Msg`s a guest would send, without any socket.

use std::time::{Duration, Instant};

use glam::{Quat, Vec3};
use uuid::Uuid;

use super::wire::{Msg, PeerAppearance, PeerState};
use crate::own_motion::{ANIM_STAND, ANIM_WALK};

const PEER_ID: Uuid = Uuid::from_u128(0xA15A_DE30_0000_0000_0000_0000_0000_0001);
const PEER_NAME: &str = "Nova Instance";

/// Linden built-in "dance1" (indra/llcharacter/llanimationstates.cpp). Not a
/// named constant elsewhere: the gesture table holds it as a tuple.
const ANIM_DANCE1: Uuid = Uuid::from_u128(0xb68a3d7c_de9e_fc87_eec8_543d787e5b0d);

const RADIUS: f32 = 3.0;
const WALK_S: f32 = 20.0;
const DANCE_S: f64 = 10.0;
const CYCLE_S: f64 = WALK_S as f64 + DANCE_S;

const STATE_EVERY: Duration = Duration::from_millis(100);
const APPEARANCE_EVERY: Duration = Duration::from_secs(5);
const HELLO_CHAT_AT: Duration = Duration::from_secs(3);
const PLACE_CHAT_AT: Duration = Duration::from_secs(25);
/// Chat type 1: normal speech (0 whisper, 2 shout).
const CHAT_NORMAL: u8 = 1;

pub struct DemoPeer {
    centre: Vec3,
    appearance: Option<PeerAppearance>,
    /// Set by the first poll: every phase is measured from it.
    start: Option<Instant>,
    last_state: Option<Instant>,
    last_appearance: Option<Instant>,
    /// Walking (true) or dancing, as of the last state sent.
    walking: Option<bool>,
    /// Bumped whenever the animation set changes, as the simulator does.
    anim_seq: i32,
    said_hello: bool,
    said_place: bool,
}

impl DemoPeer {
    pub fn new(centre: Vec3, appearance: Option<PeerAppearance>) -> Self {
        Self {
            centre,
            appearance,
            start: None,
            last_state: None,
            last_appearance: None,
            walking: None,
            anim_seq: 0,
            said_hello: false,
            said_place: false,
        }
    }

    /// The messages due at `now`: the Hello first, then at most one State
    /// every 100 ms, plus the two scripted chat lines once each.
    pub fn poll(&mut self, now: Instant) -> Vec<Msg> {
        let start = match self.start {
            Some(start) => start,
            None => {
                self.start = Some(now);
                return vec![Msg::Hello {
                    id: PEER_ID,
                    name: PEER_NAME.to_owned(),
                }];
            }
        };
        let elapsed = now.saturating_duration_since(start);
        let mut out = Vec::new();
        if !self.said_hello && elapsed >= HELLO_CHAT_AT {
            self.said_hello = true;
            out.push(chat(aurora_i18n::tr!("demo-instance-peer-chat-hello").to_string()));
        }
        if !self.said_place && elapsed >= PLACE_CHAT_AT {
            self.said_place = true;
            out.push(chat(aurora_i18n::tr!("demo-instance-peer-chat-place").to_string()));
        }
        if self.last_state.is_none_or(|t| now.saturating_duration_since(t) >= STATE_EVERY) {
            self.last_state = Some(now);
            out.push(Msg::State(self.state(now, elapsed)));
        }
        out
    }

    fn state(&mut self, now: Instant, elapsed: Duration) -> PeerState {
        let pose = pose(self.centre, elapsed);
        if self.walking != Some(pose.walking) {
            self.walking = Some(pose.walking);
            self.anim_seq += 1;
        }
        // Dancing keeps the default stand underneath the gesture; walking replaces it.
        let anims = if pose.walking {
            vec![(ANIM_WALK, self.anim_seq)]
        } else {
            vec![(ANIM_STAND, self.anim_seq), (ANIM_DANCE1, self.anim_seq)]
        };
        // The appearance travels with the first state, then every 5 s for late arrivals.
        let appearance = match &self.appearance {
            Some(appearance)
                if self
                    .last_appearance
                    .is_none_or(|t| now.saturating_duration_since(t) >= APPEARANCE_EVERY) =>
            {
                self.last_appearance = Some(now);
                Some(appearance.clone())
            }
            _ => None,
        };
        PeerState {
            id: PEER_ID,
            pos: pose.pos.to_array(),
            rot: pose.rot.to_array(),
            vel: pose.vel.to_array(),
            flying: false,
            anims,
            appearance,
        }
    }
}

fn chat(text: String) -> Msg {
    Msg::Chat {
        id: PEER_ID,
        text,
        chat_type: CHAT_NORMAL,
    }
}

/// Where the avatar is `elapsed` after the start of the loop.
struct Pose {
    pos: Vec3,
    rot: Quat,
    vel: Vec3,
    walking: bool,
}

/// Walk one lap of the circle in `WALK_S`, then stand still at the lap's
/// starting point for `DANCE_S`, and repeat. The body faces its walking
/// direction (+X is forward, so the yaw is the tangent's angle).
fn pose(centre: Vec3, elapsed: Duration) -> Pose {
    let t = elapsed.as_secs_f64() % CYCLE_S;
    if t < f64::from(WALK_S) {
        let theta = std::f32::consts::TAU * (t as f32 / WALK_S);
        let (sin, cos) = theta.sin_cos();
        let speed = RADIUS * std::f32::consts::TAU / WALK_S;
        Pose {
            pos: centre + Vec3::new(RADIUS * cos, RADIUS * sin, 0.0),
            rot: Quat::from_rotation_z(theta + std::f32::consts::FRAC_PI_2),
            vel: Vec3::new(-sin, cos, 0.0) * speed,
            walking: true,
        }
    } else {
        // The lap ends where it started (theta = 0), facing the tangent there.
        Pose {
            pos: centre + Vec3::new(RADIUS, 0.0, 0.0),
            rot: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            vel: Vec3::ZERO,
            walking: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTRE: Vec3 = Vec3::new(128.0, 64.0, 22.0);
    const EPS: f32 = 1e-3;

    /// Polls every 10 ms from the start up to `until_ms`, keeping the time of each message.
    fn run(peer: &mut DemoPeer, t0: Instant, until_ms: u64) -> Vec<(u64, Msg)> {
        let mut out = Vec::new();
        for ms in (0..=until_ms).step_by(10) {
            for msg in peer.poll(t0 + Duration::from_millis(ms)) {
                out.push((ms, msg));
            }
        }
        out
    }

    fn states(msgs: &[(u64, Msg)]) -> Vec<(u64, PeerState)> {
        msgs.iter()
            .filter_map(|(ms, m)| match m {
                Msg::State(s) => Some((*ms, s.clone())),
                _ => None,
            })
            .collect()
    }

    /// The first state at or after `ms`: states are sent 10 ms after the Hello, then every 100 ms.
    fn state_from(all: &[(u64, PeerState)], ms: u64) -> PeerState {
        all.iter().find(|(t, _)| *t >= ms).expect("a state after this time").1.clone()
    }

    fn appearance() -> PeerAppearance {
        PeerAppearance {
            te: String::new(),
            visual_params: String::new(),
            hover: 0.0,
            cof_version: 0,
        }
    }

    #[test]
    fn first_poll_is_hello() {
        let mut peer = DemoPeer::new(CENTRE, None);
        let msgs = peer.poll(Instant::now());
        assert!(matches!(msgs.as_slice(), [Msg::Hello { id, name }] if *id == PEER_ID && name == PEER_NAME));
    }

    #[test]
    fn states_come_at_most_every_100_ms() {
        let mut peer = DemoPeer::new(CENTRE, None);
        let t0 = Instant::now();
        let msgs = run(&mut peer, t0, 60_000);
        let times: Vec<u64> = states(&msgs).iter().map(|(ms, _)| *ms).collect();
        assert!(times.len() > 500, "expected about 10 states per second, got {}", times.len());
        for pair in times.windows(2) {
            assert!(pair[1] - pair[0] >= 100, "states too close: {pair:?}");
        }
    }

    #[test]
    fn position_stays_within_radius_of_centre() {
        let mut peer = DemoPeer::new(CENTRE, None);
        let t0 = Instant::now();
        for (_, s) in states(&run(&mut peer, t0, 90_000)) {
            let pos = Vec3::from_array(s.pos);
            assert!((pos - CENTRE).length() <= RADIUS + EPS, "outside the circle: {pos:?}");
            assert_eq!(pos.z, CENTRE.z);
        }
    }

    #[test]
    fn walking_moves_and_dancing_is_still() {
        let mut peer = DemoPeer::new(CENTRE, None);
        let t0 = Instant::now();
        let all = states(&run(&mut peer, t0, 35_000));
        let walk = state_from(&all, 5_000);
        assert_ne!(walk.vel, [0.0; 3]);
        assert_eq!(walk.anims, vec![(ANIM_WALK, 1)]);
        let dance = state_from(&all, 25_000);
        assert_eq!(dance.vel, [0.0; 3]);
        assert_eq!(dance.anims, vec![(ANIM_STAND, 2), (ANIM_DANCE1, 2)]);
        let walk_again = state_from(&all, 32_000);
        assert_eq!(walk_again.anims, vec![(ANIM_WALK, 3)]);
    }

    #[test]
    fn velocity_matches_the_walk_speed() {
        let mut peer = DemoPeer::new(CENTRE, None);
        let t0 = Instant::now();
        let speed = RADIUS * std::f32::consts::TAU / WALK_S;
        for (_, s) in states(&run(&mut peer, t0, 20_000))
            .iter()
            .filter(|(_, s)| s.anims[0].0 == ANIM_WALK)
        {
            assert!((Vec3::from_array(s.vel).length() - speed).abs() < EPS);
        }
    }

    #[test]
    fn appearance_is_sent_first_and_every_five_seconds() {
        let mut peer = DemoPeer::new(CENTRE, Some(appearance()));
        let t0 = Instant::now();
        let sent: Vec<u64> = states(&run(&mut peer, t0, 12_000))
            .iter()
            .filter(|(_, s)| s.appearance.is_some())
            .map(|(ms, _)| *ms)
            .collect();
        assert_eq!(sent, vec![10, 5_010, 10_010]);
    }

    #[test]
    fn chat_is_sent_once_at_its_time() {
        let mut peer = DemoPeer::new(CENTRE, None);
        let t0 = Instant::now();
        let chats: Vec<(u64, Msg)> = run(&mut peer, t0, 60_000)
            .into_iter()
            .filter(|(_, m)| matches!(m, Msg::Chat { .. }))
            .collect();
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0].0, 3_000);
        assert_eq!(chats[1].0, 25_000);
        assert!(matches!(
            &chats[0].1,
            Msg::Chat {
                chat_type: CHAT_NORMAL,
                ..
            }
        ));
    }
}
