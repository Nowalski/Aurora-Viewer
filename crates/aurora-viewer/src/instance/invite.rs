//! An invitation into someone's shared instance, sent to a friend as the
//! last line of an ordinary instant message (the simulator drops the binary
//! bucket of a plain IM, so it travels in the text; the line above says what
//! it is for viewers that do not know it). Accepting it is the user's choice, like
//! a teleport offer: the viewer then joins the instance and teleports next
//! to the host.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What marks the line of an invitation.
const PREFIX: &str = "aurora-instance:";
/// Longest invitation line accepted (an IM holds 1024 bytes).
const MAX_LINE: usize = 600;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    /// Where the host listens: "host:port".
    pub address: String,
    /// The host's id in the instance; nil when unknown (the first peer that
    /// says Hello is then taken for the host).
    pub host: Uuid,
    /// The host's region and position there, where the teleport goes when
    /// the host's own position does not come in time.
    pub region: String,
    pub pos: [f32; 3],
}

impl Invitation {
    /// The IM text: `text`, then the invitation's line.
    pub fn to_message(&self, text: &str) -> String {
        // serialising this plain struct cannot fail
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{text}\n{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// The invitation an IM's text ends with, and the text without it.
    pub fn from_message(message: &str) -> Option<(Invitation, String)> {
        let trimmed = message.trim_end();
        let (head, last) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
        let last = last.trim();
        if last.len() > MAX_LINE {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(last.strip_prefix(PREFIX)?).ok()?;
        let invitation: Invitation = serde_json::from_slice(&json).ok()?;
        invitation.valid().then(|| (invitation, head.trim_end().to_owned()))
    }

    fn valid(&self) -> bool {
        let address_ok = self.address.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty()
                && host.len() <= 253
                && host.chars().all(|c| c.is_ascii_alphanumeric() || ".-[]:".contains(c))
                && port.parse::<u16>().is_ok_and(|p| p != 0)
        });
        let [x, y, z] = self.pos;
        address_ok
            && !self.region.trim().is_empty()
            && self.region.chars().count() <= 64
            && !self.region.chars().any(char::is_control)
            && (0.0..=256.0).contains(&x)
            && (0.0..=256.0).contains(&y)
            && (-100.0..=4096.0).contains(&z)
    }
}

/// AURORA_DEMO_INSTANCE_INVITE=<host:port>: the demo's first friend invites
/// us into the instance at that address (another demo window hosting it).
/// The host's id is not known here: the first peer that says Hello is it.
pub fn demo_event(world: &crate::world::World) -> Option<aurora_net::NetEvent> {
    let address = std::env::var("AURORA_DEMO_INSTANCE_INVITE").ok()?.trim().to_owned();
    let invitation = Invitation {
        address,
        host: Uuid::nil(),
        region: world.region_name(),
        pos: world.agent.position.to_array(),
    };
    if !invitation.valid() {
        log::warn!("shared instance: AURORA_DEMO_INSTANCE_INVITE is not host:port");
        return None;
    }
    let from = crate::demo::DEMO_FRIEND;
    let text = aurora_i18n::tr!("instance-invite-message");
    Some(aurora_net::NetEvent::InstantMessage(aurora_net::InstantMessage {
        from_agent_id: from,
        from_name: world.social.name_of(&from),
        to_agent_id: world.agent_id,
        dialog: 0,
        session_id: Uuid::from_u128(from.as_u128() ^ world.agent_id.as_u128()),
        message: invitation.to_message(text),
        offline: false,
        from_group: false,
        binary_bucket: Vec::new(),
        timestamp: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation() -> Invitation {
        Invitation {
            address: "127.0.0.1:7777".into(),
            host: Uuid::from_u128(4),
            region: "Aurora Plaza".into(),
            pos: [128.0, 128.0, 22.0],
        }
    }

    #[test]
    fn round_trips_through_a_message() {
        let i = invitation();
        let text = i.to_message("Join my place.");
        assert_eq!(Invitation::from_message(&text), Some((i.clone(), "Join my place.".to_owned())));
        // as a simulator may hand it back, with trailing spaces
        assert_eq!(Invitation::from_message(&format!("{text}  \n")).map(|(i, _)| i), Some(i));
    }

    #[test]
    fn refuses_what_is_not_one() {
        assert!(Invitation::from_message("").is_none());
        assert!(Invitation::from_message("hello\naurora-instance:").is_none());
        assert!(Invitation::from_message("aurora-instance:e30").is_none());
        let bad = |f: fn(&mut Invitation)| {
            let mut i = invitation();
            f(&mut i);
            Invitation::from_message(&i.to_message("x")).is_none()
        };
        assert!(bad(|i| i.address = "127.0.0.1".into()));
        assert!(bad(|i| i.address = "127.0.0.1:0".into()));
        assert!(bad(|i| i.address = "evil host:7777".into()));
        assert!(bad(|i| i.region = "\n".into()));
        assert!(bad(|i| i.pos[0] = f32::NAN));
        assert!(bad(|i| i.pos[1] = 300.0));
    }
}
