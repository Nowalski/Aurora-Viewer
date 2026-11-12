//! An invitation into someone's shared instance, sent to a friend in the
//! binary bucket of an ordinary instant message (the text says what it is
//! for viewers that do not know it). Accepting it is the user's choice, like
//! a teleport offer: the viewer then joins the instance and teleports next
//! to the host.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What marks the bucket of an invitation.
const PREFIX: &[u8] = b"aurora-instance:";
/// Longest bucket accepted (the simulator's own limit is 1024 bytes).
const MAX_BUCKET: usize = 512;

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
    pub fn to_bucket(&self) -> Vec<u8> {
        let mut bucket = PREFIX.to_vec();
        // serialising this plain struct cannot fail
        bucket.extend(serde_json::to_vec(self).unwrap_or_default());
        bucket
    }

    /// The invitation in an IM's bucket, if it holds a valid one.
    pub fn from_bucket(bucket: &[u8]) -> Option<Invitation> {
        let bucket = bucket.strip_suffix(b"\0").unwrap_or(bucket);
        if bucket.len() > MAX_BUCKET {
            return None;
        }
        let json = bucket.strip_prefix(PREFIX)?;
        let invitation: Invitation = serde_json::from_slice(json).ok()?;
        invitation.valid().then_some(invitation)
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
    Some(aurora_net::NetEvent::InstantMessage(aurora_net::InstantMessage {
        from_agent_id: from,
        from_name: world.social.name_of(&from),
        to_agent_id: world.agent_id,
        dialog: 0,
        session_id: Uuid::from_u128(from.as_u128() ^ world.agent_id.as_u128()),
        message: aurora_i18n::tr!("instance-invite-message").into(),
        offline: false,
        from_group: false,
        binary_bucket: invitation.to_bucket(),
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
    fn round_trips_through_a_bucket() {
        let i = invitation();
        assert_eq!(Invitation::from_bucket(&i.to_bucket()), Some(i.clone()));
        // as the simulator may hand it back, with a closing zero
        let mut zero = i.to_bucket();
        zero.push(0);
        assert_eq!(Invitation::from_bucket(&zero), Some(i));
    }

    #[test]
    fn refuses_what_is_not_one() {
        assert!(Invitation::from_bucket(b"").is_none());
        assert!(Invitation::from_bucket(b"16|1234").is_none());
        let bad = |f: fn(&mut Invitation)| {
            let mut i = invitation();
            f(&mut i);
            Invitation::from_bucket(&i.to_bucket()).is_none()
        };
        assert!(bad(|i| i.address = "127.0.0.1".into()));
        assert!(bad(|i| i.address = "127.0.0.1:0".into()));
        assert!(bad(|i| i.address = "evil host:7777".into()));
        assert!(bad(|i| i.region = "\n".into()));
        assert!(bad(|i| i.pos[0] = f32::NAN));
        assert!(bad(|i| i.pos[1] = 300.0));
    }
}
