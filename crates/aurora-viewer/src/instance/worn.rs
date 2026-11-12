//! What avatars wear, for the other viewers of the instance: each linkset
//! attached to an avatar goes out as one `Worn` message (its prims' shape,
//! texture entry and extra parameters, ids only), again when it changes,
//! and an `Unworn` when it is taken off. HUDs stay private, as the
//! simulator never sends another avatar's HUD either.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use aurora_net::objects::{AttachedSound, ObjectUpdate, ParticleUpdate};
use aurora_prim::extra::{
    PARAMS_EXTENDED_MESH, PARAMS_FLEXIBLE, PARAMS_LIGHT, PARAMS_LIGHT_IMAGE, PARAMS_REFLECTION_PROBE, PARAMS_RENDER_MATERIAL,
    PARAMS_SCULPT, pack_extended_mesh, pack_flexible, pack_light, pack_light_image, pack_reflection_probe, pack_sculpt, parse_extra_params,
};
use aurora_prim::params::{LL_PCODE_VOLUME, RawShape};
use aurora_prim::te::{pack_texture_entry, parse_texture_entry};
use aurora_prim::{ExtraParams, TextureEntry};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use glam::{Quat, Vec3};
use uuid::Uuid;

use super::wire::{self, MAX_WORN_PRIMS, Msg, Worn, WornPrim};
use crate::world::World;
use crate::world::objects::Object;

/// How often an avatar's attachments are looked at again.
const CHECK: Duration = Duration::from_secs(1);

/// HUD attachment points (LLViewerJointAttachment::getIsHUDAttachment).
const HUD_POINTS: std::ops::RangeInclusive<u8> = 31..=38;

/// The linksets last sent, per avatar: root key -> hash of the message.
#[derive(Default)]
pub struct WornSender {
    sent: HashMap<Uuid, (Instant, HashMap<u32, u64>)>,
}

impl WornSender {
    /// Messages for what `avatar` wears now, sent under the instance id `id`.
    pub fn messages(&mut self, world: &World, id: Uuid, avatar: &Object, now: Instant) -> Vec<Msg> {
        if self.sent.get(&id).is_some_and(|(at, _)| now.duration_since(*at) < CHECK) {
            return Vec::new();
        }
        let (at, sent) = self.sent.entry(id).or_insert_with(|| (now, HashMap::new()));
        *at = now;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for &idx in world.objects.children_of(&avatar.key) {
            let Some(root) = world.objects.get(idx) else { continue };
            if !root.is_attachment() || HUD_POINTS.contains(&root.attachment_point()) {
                continue;
            }
            let key = root.key.local_id;
            let prims = linkset(world, root);
            let msg = Msg::Worn(Worn { id, root: key, prims });
            let line = wire::encode(&msg);
            if line.len() > wire::MAX_LINE {
                log::warn!("instance: a worn linkset is too large to share ({} bytes)", line.len());
                continue;
            }
            seen.insert(key);
            let mut h = DefaultHasher::new();
            line.hash(&mut h);
            let hash = h.finish();
            if sent.get(&key) != Some(&hash) {
                sent.insert(key, hash);
                out.push(msg);
            }
        }
        let gone: Vec<u32> = sent.keys().filter(|k| !seen.contains(k)).copied().collect();
        for root in gone {
            sent.remove(&root);
            out.push(Msg::Unworn { id, root });
        }
        out
    }

    /// The avatar left: its linksets go with it.
    pub fn forget(&mut self, id: &Uuid) {
        self.sent.remove(id);
    }
}

/// A root and its children (at most `MAX_WORN_PRIMS` in all), for the wire.
pub(super) fn linkset(world: &World, root: &Object) -> Vec<WornPrim> {
    let mut prims = vec![worn_prim(root)];
    for &c in world.objects.children_of(&root.key).iter().take(MAX_WORN_PRIMS - 1) {
        if let Some(child) = world.objects.get(c) {
            prims.push(worn_prim(child));
        }
    }
    prims
}

fn worn_prim(o: &Object) -> WornPrim {
    let r = o.volume.to_raw();
    let shape = [
        r.path_curve as i32,
        r.profile_curve as i32,
        r.path_begin as i32,
        r.path_end as i32,
        r.path_scale_x as i32,
        r.path_scale_y as i32,
        r.path_shear_x as i32,
        r.path_shear_y as i32,
        r.path_twist as i32,
        r.path_twist_begin as i32,
        r.path_radius_offset as i32,
        r.path_taper_x as i32,
        r.path_taper_y as i32,
        r.path_revolutions as i32,
        r.path_skew as i32,
        r.profile_begin as i32,
        r.profile_end as i32,
        r.profile_hollow as i32,
    ];
    let te = o.te.as_ref().map(|t| pack_texture_entry(t, t.faces.len())).unwrap_or_default();
    WornPrim {
        key: o.key.local_id,
        state: o.state,
        pcode: o.pcode,
        material: o.prim_material,
        pos: o.position.to_array(),
        rot: o.rotation.to_array(),
        scale: o.scale.to_array(),
        shape,
        te: STANDARD.encode(te),
        extra: STANDARD.encode(pack_extra(&o.extra)),
    }
}

/// An ExtraParams blob as the simulator sends it (`count u8, { type u16,
/// size u32, data }*`), the inverse of `parse_extra_params`.
fn pack_extra(e: &ExtraParams) -> Vec<u8> {
    let mut entries: Vec<(u16, Vec<u8>)> = Vec::new();
    if let Some(f) = &e.flexible {
        entries.push((PARAMS_FLEXIBLE, pack_flexible(f)));
    }
    if let Some(l) = &e.light {
        entries.push((PARAMS_LIGHT, pack_light(l)));
    }
    if let Some(s) = &e.sculpt {
        entries.push((PARAMS_SCULPT, pack_sculpt(s)));
    }
    if let Some(l) = &e.light_image {
        entries.push((PARAMS_LIGHT_IMAGE, pack_light_image(l)));
    }
    if let Some(flags) = e.extended_mesh_flags {
        entries.push((PARAMS_EXTENDED_MESH, pack_extended_mesh(flags)));
    }
    if !e.render_materials.is_empty() {
        let n = e.render_materials.len().min(u8::MAX as usize);
        let mut d = vec![n as u8];
        for (face, id) in e.render_materials.iter().take(n) {
            d.push(*face);
            d.extend_from_slice(id.as_bytes());
        }
        entries.push((PARAMS_RENDER_MATERIAL, d));
    }
    if let Some(r) = &e.reflection_probe {
        entries.push((PARAMS_REFLECTION_PROBE, pack_reflection_probe(r)));
    }
    let mut out = vec![entries.len() as u8];
    for (ty, d) in entries {
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
        out.extend_from_slice(&d);
    }
    out
}

/// Unpack a quantised shape; `None` when a field is out of its range.
fn raw_shape(s: &[i32; 18]) -> Option<RawShape> {
    let u8_ = |v: i32| u8::try_from(v).ok();
    let i8_ = |v: i32| i8::try_from(v).ok();
    let u16_ = |v: i32| u16::try_from(v).ok();
    Some(RawShape {
        path_curve: u8_(s[0])?,
        profile_curve: u8_(s[1])?,
        path_begin: u16_(s[2])?,
        path_end: u16_(s[3])?,
        path_scale_x: u8_(s[4])?,
        path_scale_y: u8_(s[5])?,
        path_shear_x: u8_(s[6])?,
        path_shear_y: u8_(s[7])?,
        path_twist: i8_(s[8])?,
        path_twist_begin: i8_(s[9])?,
        path_radius_offset: i8_(s[10])?,
        path_taper_x: i8_(s[11])?,
        path_taper_y: i8_(s[12])?,
        path_revolutions: u8_(s[13])?,
        path_skew: i8_(s[14])?,
        profile_begin: u16_(s[15])?,
        profile_end: u16_(s[16])?,
        profile_hollow: u16_(s[17])?,
    })
}

/// A packed texture entry from a peer; `None` on bad base64 or an
/// unreadable entry.
pub fn decode_te(b64: &str) -> Option<TextureEntry> {
    let bytes = STANDARD.decode(b64).ok()?;
    parse_texture_entry(&bytes)
}

/// The full update of a worn prim from a peer, attached to `parent` (the
/// avatar's local id for the root, the root's for a child). `None` for a
/// prim that is not a plain volume or whose fields do not decode.
pub fn prim_update(p: &WornPrim, local_id: u32, full_id: Uuid, parent: u32, owner: Uuid) -> Option<ObjectUpdate> {
    if p.pcode != LL_PCODE_VOLUME {
        return None;
    }
    let extra = parse_extra_params(&STANDARD.decode(&p.extra).ok()?);
    let mut volume = raw_shape(&p.shape)?.to_params();
    volume.sculpt = extra.sculpt;
    let texture_entry = if p.te.is_empty() { None } else { Some(decode_te(&p.te)?.into()) };
    let rotation = Quat::from_array(p.rot);
    if rotation.length() < 0.5 {
        return None;
    }
    Some(ObjectUpdate {
        local_id,
        full_id,
        parent_id: parent,
        pcode: p.pcode,
        state: p.state,
        crc: 0,
        material: p.material,
        click_action: 0,
        scale: Vec3::from_array(p.scale),
        position: Vec3::from_array(p.pos),
        rotation: rotation.normalize(),
        velocity: Vec3::ZERO,
        acceleration: Vec3::ZERO,
        angular_velocity: Vec3::ZERO,
        update_flags: 0,
        owner_id: owner,
        volume,
        texture_entry,
        extra,
        name_values: String::new(),
        text: String::new(),
        text_color: [0; 4],
        media_url: String::new(),
        tree_species: None,
        texture_anim: None,
        foot_plane: None,
        particles: ParticleUpdate::Keep,
        sound: AttachedSound::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_prim::SculptParams;

    #[test]
    fn extra_params_round_trip() {
        let e = ExtraParams {
            sculpt: Some(SculptParams {
                texture: Uuid::from_u128(5),
                sculpt_type: 5,
            }),
            extended_mesh_flags: Some(1),
            render_materials: vec![(0, Uuid::from_u128(6)), (2, Uuid::from_u128(7))],
            ..Default::default()
        };
        let back = parse_extra_params(&pack_extra(&e));
        assert_eq!(back.sculpt, e.sculpt);
        assert_eq!(back.extended_mesh_flags, Some(1));
        assert_eq!(back.render_materials, e.render_materials);
        assert!(parse_extra_params(&pack_extra(&ExtraParams::default())).sculpt.is_none());
    }

    #[test]
    fn shape_out_of_range_is_refused() {
        let mut s = [0; 18];
        assert!(raw_shape(&s).is_some());
        s[8] = 300;
        assert!(raw_shape(&s).is_none());
    }
}
