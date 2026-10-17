//! Who hurt us, and when.
//!
//! A vanilla server tells a player about each hit with a `ClientboundDamageEvent`: the entity hurt,
//! the damage type, the entity that caused it (the player who swung, or the shooter of an arrow)
//! and the direct one (the arrow itself, or the same player for melee). `ClientboundHurtAnimation`
//! follows with the direction of the hit. We keep both for the local player in
//! [`RecentAttackers`], stamped with a tick counter of our own.

use azalea_core::{entity_id::MinecraftEntityId, position::Vec3, tick::GameTick};
use azalea_protocol::packets::game::{ClientboundDamageEvent, ClientboundHurtAnimation};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

/// Hits older than this many ticks are dropped (20 s).
const KEEP_TICKS: u64 = 400;

pub struct AttackersPlugin;
impl Plugin for AttackersPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(GameTick, age_recent_attackers);
    }
}

/// One damage event that hit the local player.
#[derive(Clone, Debug, PartialEq)]
pub struct AttackerHit {
    /// [`RecentAttackers::now`] when the packet was handled.
    pub tick: u64,
    /// The entity that caused the damage (a player; the shooter of an arrow), if any.
    pub cause: Option<MinecraftEntityId>,
    /// The entity that dealt it (the arrow; the player in melee), if any.
    pub direct: Option<MinecraftEntityId>,
    /// Index into the damage-type registry (fall, arrow, player attack, ...).
    pub source_type: u32,
    /// Where the damage came from when no entity did (explosions, `damage ... at`).
    pub position: Option<Vec3>,
}

impl AttackerHit {
    /// The entity to blame: the cause, else the direct one.
    pub fn attacker(&self) -> Option<MinecraftEntityId> {
        self.cause.or(self.direct)
    }
}

/// The damage events of the last [`KEEP_TICKS`] ticks, oldest first.
#[derive(Clone, Component, Debug, Default)]
pub struct RecentAttackers {
    /// Local game ticks since this component was added.
    pub now: u64,
    pub hits: Vec<AttackerHit>,
    /// The latest hurt animation: the tick it came and the yaw of the hit relative to us.
    pub last_hurt: Option<(u64, f32)>,
}

impl RecentAttackers {
    /// Hits that came in the last `ticks` ticks, oldest first.
    pub fn within(&self, ticks: u64) -> impl Iterator<Item = &AttackerHit> {
        self.hits.iter().filter(move |h| self.now.saturating_sub(h.tick) <= ticks)
    }
}

pub fn age_recent_attackers(mut query: Query<&mut RecentAttackers>) {
    for mut r in &mut query {
        r.now += 1;
        let cutoff = r.now.saturating_sub(KEEP_TICKS);
        r.hits.retain(|h| h.tick >= cutoff);
    }
}

// Spec: specs/B5-combat.md §4.7 (attribution needs to know who hit us)
pub(crate) fn handle_damage_event(ecs: &mut World, player: Entity, p: &ClientboundDamageEvent) {
    let Ok(mut e) = ecs.get_entity_mut(player) else { return };
    if e.get::<MinecraftEntityId>() != Some(&p.entity_id) {
        return;
    }
    let Some(mut r) = e.get_mut::<RecentAttackers>() else { return };
    let tick = r.now;
    r.hits.push(AttackerHit {
        tick,
        cause: p.source_cause_id.0.map(|i| MinecraftEntityId(i as i32)),
        direct: p.source_direct_id.0.map(|i| MinecraftEntityId(i as i32)),
        source_type: p.source_type_id,
        position: p.source_position,
    });
}

pub(crate) fn handle_hurt_animation(ecs: &mut World, player: Entity, p: &ClientboundHurtAnimation) {
    let Ok(mut e) = ecs.get_entity_mut(player) else { return };
    if e.get::<MinecraftEntityId>() != Some(&p.id) {
        return;
    }
    if let Some(mut r) = e.get_mut::<RecentAttackers>() {
        r.last_hurt = Some((r.now, p.yaw));
    }
}
