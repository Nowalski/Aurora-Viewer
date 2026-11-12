//! Wire format of a shared instance (experimental): one JSON object per
//! line over TCP. Every message is bounded so a peer cannot make another
//! viewer allocate without limit. Only ids and small state travel: assets
//! are never sent, each viewer loads them itself.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest line accepted (bytes, newline excluded). An appearance (texture
/// entry and visual parameters in base64) is well under it.
pub const MAX_LINE: usize = 64 * 1024;
pub const MAX_NAME: usize = 64;
pub const MAX_CHAT: usize = 1024;
pub const MAX_ANIMS: usize = 64;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum Msg {
    /// First message of a peer: its instance id and display name.
    Hello {
        id: Uuid,
        name: String,
    },
    State(PeerState),
    /// Nearby chat said by a peer (`chat_type`: 0 whisper, 1 normal, 2 shout).
    Chat {
        id: Uuid,
        text: String,
        chat_type: u8,
    },
    /// The peer left.
    Bye {
        id: Uuid,
    },
}

impl Msg {
    pub fn id(&self) -> Uuid {
        match self {
            Msg::Hello { id, .. } | Msg::Chat { id, .. } | Msg::Bye { id } => *id,
            Msg::State(s) => s.id,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeerState {
    pub id: Uuid,
    /// Region-local position (m).
    pub pos: [f32; 3],
    /// Body rotation (x, y, z, w).
    pub rot: [f32; 4],
    pub vel: [f32; 3],
    pub flying: bool,
    /// Animations playing on the avatar: (asset id, sequence).
    pub anims: Vec<(Uuid, i32)>,
    /// Sent when it changed, and again now and then for late arrivals.
    pub appearance: Option<PeerAppearance>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeerAppearance {
    /// Packed texture entry (`aurora_prim::te::pack_texture_entry`), base64.
    pub te: String,
    /// Visual parameters (one byte each), base64.
    pub visual_params: String,
    pub hover: f32,
    pub cof_version: i32,
}

/// One line for the socket, newline included.
pub fn encode(msg: &Msg) -> Vec<u8> {
    // serialising these plain structs cannot fail
    let mut line = serde_json::to_vec(msg).unwrap_or_default();
    line.push(b'\n');
    line
}

/// Parse and validate one line (newline already removed). `None` for
/// anything malformed, oversized or out of bounds.
pub fn decode(line: &[u8]) -> Option<Msg> {
    if line.len() > MAX_LINE {
        return None;
    }
    let msg: Msg = serde_json::from_slice(line).ok()?;
    valid(&msg).then_some(msg)
}

/// Largest base64 string of a texture entry or of the visual parameters.
const MAX_APPEARANCE_B64: usize = 16 * 1024;

/// Whether a decoded message stays within the protocol's bounds. A message
/// that fails is dropped, so a peer cannot make another viewer allocate
/// without limit or place an avatar outside the region.
pub fn valid(msg: &Msg) -> bool {
    // A nil id is never a real avatar or instance: refuse it in every message.
    if msg.id().is_nil() {
        return false;
    }
    match msg {
        Msg::Hello { name, .. } => valid_name(name),
        Msg::State(state) => valid_state(state),
        Msg::Chat { text, chat_type, .. } => !text.is_empty() && text.len() <= MAX_CHAT && *chat_type <= 2,
        Msg::Bye { .. } => true,
    }
}

/// Non-blank, at most `MAX_NAME` characters, no control characters (they
/// would break the nameplate and the log).
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= MAX_NAME && !name.chars().any(char::is_control)
}

fn valid_state(state: &PeerState) -> bool {
    let finite = state.pos.iter().chain(&state.rot).chain(&state.vel).all(|v| v.is_finite());
    let [x, y, z] = state.pos;
    let [qx, qy, qz, qw] = state.rot;
    let norm = (qx * qx + qy * qy + qz * qz + qw * qw).sqrt();
    // Range checks also reject NaN, since a comparison with NaN is false.
    finite
        && (0.0..=8192.0).contains(&x)
        && (0.0..=8192.0).contains(&y)
        && (-100.0..=4096.0).contains(&z)
        && (0.5..=1.5).contains(&norm)
        && state.vel.iter().all(|v| v.abs() <= 1000.0)
        && state.anims.len() <= MAX_ANIMS
        && state.anims.iter().all(|(asset, _)| !asset.is_nil())
        && state.appearance.as_ref().is_none_or(valid_appearance)
}

fn valid_appearance(appearance: &PeerAppearance) -> bool {
    appearance.te.len() <= MAX_APPEARANCE_B64
        && appearance.visual_params.len() <= MAX_APPEARANCE_B64
        && (-2.0..=2.0).contains(&appearance.hover)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> Uuid {
        Uuid::from_u128(1)
    }

    fn state() -> PeerState {
        PeerState {
            id: peer(),
            pos: [128.0, 128.0, 22.0],
            rot: [0.0, 0.0, 0.0, 1.0],
            vel: [0.0; 3],
            flying: false,
            anims: vec![(Uuid::from_u128(2), 1)],
            appearance: None,
        }
    }

    fn appearance() -> PeerAppearance {
        PeerAppearance {
            te: "AAAA".into(),
            visual_params: "AAAA".into(),
            hover: 0.0,
            cof_version: 3,
        }
    }

    /// Encode, drop the newline, decode: what a socket reader does.
    fn round_trip(msg: &Msg) -> Option<Msg> {
        let line = encode(msg);
        decode(&line[..line.len() - 1])
    }

    #[test]
    fn round_trip_each_variant() {
        let hello = Msg::Hello {
            id: peer(),
            name: "Tester".into(),
        };
        let chat = Msg::Chat {
            id: peer(),
            text: "hello there".into(),
            chat_type: 1,
        };
        let bye = Msg::Bye { id: peer() };
        let plain = Msg::State(state());
        let mut with_appearance = state();
        with_appearance.appearance = Some(appearance());
        let with_appearance = Msg::State(with_appearance);

        for msg in [hello, chat, bye, plain, with_appearance] {
            assert_eq!(round_trip(&msg), Some(msg.clone()));
        }
    }

    #[test]
    fn rejects_oversized_line() {
        let msg = Msg::Chat {
            id: peer(),
            text: "a".into(),
            chat_type: 1,
        };
        let mut line = encode(&msg);
        line.pop();
        line.resize(MAX_LINE + 1, b' ');
        assert!(decode(&line).is_none());
    }

    #[test]
    fn rejects_unknown_field() {
        let msg = Msg::Bye { id: peer() };
        let mut value = serde_json::to_value(&msg).expect("serialises");
        value.as_object_mut().expect("object").insert("extra".into(), serde_json::json!(1));
        let line = serde_json::to_vec(&value).expect("serialises");
        assert!(decode(&line).is_none());
    }

    #[test]
    fn rejects_unknown_tag() {
        let line = format!(r#"{{"t":"teleport","id":"{}"}}"#, peer());
        assert!(decode(line.as_bytes()).is_none());
    }

    #[test]
    fn rejects_nan_position() {
        let mut s = state();
        s.pos[0] = f32::NAN;
        assert!(!valid(&Msg::State(s)));
        // NaN is written as null on the wire, which does not parse as a float.
        let mut s = state();
        s.pos[1] = f32::NAN;
        let line = encode(&Msg::State(s));
        assert!(decode(&line[..line.len() - 1]).is_none());
    }

    #[test]
    fn rejects_position_out_of_region() {
        let mut s = state();
        s.pos[0] = 8193.0;
        assert!(!valid(&Msg::State(s)));
        let mut s = state();
        s.pos[2] = -101.0;
        assert!(!valid(&Msg::State(s)));
    }

    #[test]
    fn rejects_empty_name() {
        for name in ["", "   "] {
            let msg = Msg::Hello {
                id: peer(),
                name: name.into(),
            };
            assert!(!valid(&msg));
        }
    }

    #[test]
    fn rejects_long_name_and_control_characters() {
        let long = Msg::Hello {
            id: peer(),
            name: "x".repeat(MAX_NAME + 1),
        };
        assert!(!valid(&long));
        let control = Msg::Hello {
            id: peer(),
            name: "bad\nname".into(),
        };
        assert!(!valid(&control));
    }

    #[test]
    fn rejects_bad_chat() {
        let empty = Msg::Chat {
            id: peer(),
            text: String::new(),
            chat_type: 1,
        };
        assert!(!valid(&empty));
        let long = Msg::Chat {
            id: peer(),
            text: "x".repeat(MAX_CHAT + 1),
            chat_type: 1,
        };
        assert!(!valid(&long));
        let bad_type = Msg::Chat {
            id: peer(),
            text: "hi".into(),
            chat_type: 3,
        };
        assert!(!valid(&bad_type));
    }

    #[test]
    fn rejects_too_many_anims() {
        let mut s = state();
        s.anims = vec![(Uuid::from_u128(2), 0); MAX_ANIMS + 1];
        assert!(!valid(&Msg::State(s)));
        let mut s = state();
        s.anims = vec![(Uuid::from_u128(2), 0); MAX_ANIMS];
        assert!(valid(&Msg::State(s)));
    }

    #[test]
    fn rejects_nil_id() {
        assert!(!valid(&Msg::Bye { id: Uuid::nil() }));
        assert!(!valid(&Msg::Hello {
            id: Uuid::nil(),
            name: "Tester".into(),
        }));
        assert!(!valid(&Msg::Chat {
            id: Uuid::nil(),
            text: "hi".into(),
            chat_type: 1,
        }));
        let mut s = state();
        s.id = Uuid::nil();
        assert!(!valid(&Msg::State(s)));
        let mut s = state();
        s.anims = vec![(Uuid::nil(), 0)];
        assert!(!valid(&Msg::State(s)));
    }

    #[test]
    fn rejects_bad_appearance() {
        let mut s = state();
        let mut a = appearance();
        a.te = "A".repeat(MAX_APPEARANCE_B64 + 1);
        s.appearance = Some(a);
        assert!(!valid(&Msg::State(s)));

        let mut s = state();
        let mut a = appearance();
        a.hover = 2.5;
        s.appearance = Some(a);
        assert!(!valid(&Msg::State(s)));
    }

    #[test]
    fn rejects_garbage_bytes() {
        assert!(decode(b"").is_none());
        assert!(decode(b"{").is_none());
        assert!(decode(b"\xff\x00\xfe not json").is_none());
    }
}
