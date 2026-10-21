//! Pushes on the local player from other entities (Null fork, patch 32): the pure rules.
//!
//! Vanilla 1.21.11, Mojang names. On the client, `LivingEntity#pushEntities` runs at the end of
//! every living entity's `aiStep`; `ClientLevel#getPushableEntities` offers only the local
//! player, and only when its box strictly intersects the caller's box. `EntitySelector#pushableBy`
//! gates the target (`LivingEntity#isPushable`: alive, not a spectator, not on a climbable;
//! the team table in [`crate::teams`]). `LivingEntity#push(Entity)` adds "a sleeping target is
//! not pushed", `Entity#push(Entity)` adds "neither has `noPhysics`, the target has no
//! passengers" and holds the arithmetic. `AbstractBoat#tick` pushes with a box inflated sideways,
//! and `AbstractBoat#push` adds a height rule.
//!
//! Skipped on purpose, and written down here: the pusher's own mirror push (`Entity#push` also
//! changes the pusher's velocity; nothing in the fork reads another entity's velocity),
//! cramming damage (server only), the frozen-entity state (`TickRateManager#isEntityFrozen`:
//! the fork has no frozen-entity state, so mobs and boats keep pushing while the server freezes
//! ticks), and the chunk-section ticking state (an entity in a chunk that loads later ticks
//! later in vanilla; the fork treats every loaded entity as ticking).
//!
//! Grim does not compute the push. `MovementTicker#handleEntityCollisions` counts the pushable
//! entities near the player and widens the horizontal tolerance of check `Simulation` by 0.08
//! each, so only an exact model, not Grim, proves the push.
//!
//! Spec: specs/B1-movement.md §4.2 (Pushes from entities)

use azalea_core::{aabb::Aabb, position::Vec3};
use azalea_registry::builtin::EntityKind;
use bevy_ecs::component::Component;

/// `0.05F` of `Entity#push(Entity)`, widened as the float constant is.
pub const PUSH_SCALE: f64 = 0.05f32 as f64;
/// `0.01F`: an offset under this pushes nothing.
pub const PUSH_MIN_OFFSET: f64 = 0.01f32 as f64;
/// The sideways inflation of a boat's push box (`inflate(0.2F, -0.01F, 0.2F)`).
pub const BOAT_BAND_XZ: f64 = 0.2f32 as f64;
/// The vertical inflation of a boat's push box.
pub const BOAT_BAND_Y: f64 = -0.01f32 as f64;

/// The velocity `Entity#push(Entity)` adds to the target for a pusher at `(dx, dz)` from the
/// target (pusher minus target, horizontal), or `None` under the 0.01 limit. The sign is away
/// from the pusher; the vertical part is 0.
// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
pub fn push_velocity(dx: f64, dz: f64) -> Option<(f64, f64)> {
    let mut f = dx.abs().max(dz.abs());
    if f < PUSH_MIN_OFFSET {
        return None;
    }
    f = f.sqrt();
    let mut d = dx / f;
    let mut e = dz / f;
    let mut g = 1.0 / f;
    if g > 1.0 {
        g = 1.0;
    }
    d *= g;
    e *= g;
    d *= PUSH_SCALE;
    e *= PUSH_SCALE;
    Some((-d, -e))
}

/// How an entity pushes the local player.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PusherClass {
    /// `LivingEntity#pushEntities`: its own box, strict intersection.
    Living,
    /// `AbstractBoat#tick`: the box inflated by [`BOAT_BAND_XZ`]/[`BOAT_BAND_Y`], plus the height
    /// rule of `AbstractBoat#push`.
    Boat,
}

/// Which way an entity pushes a player, or `None` when it never does. `living` is the living
/// marker, `boat` the boat marker. Items, arrows, item frames and minecarts never push on the
/// client (both minecart behaviours push on the server only). An armor stand (`ArmorStand#doPush`
/// is empty) and a bat push nothing; a parrot skips players (`Parrot#doPush`); a Vex and the
/// Ender Dragon have `noPhysics` set where their `aiStep` runs. A dying mob and a ridden horse
/// still push: their own health and pushability are not checked.
// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
pub fn pusher_class(living: bool, boat: bool, kind: EntityKind) -> Option<PusherClass> {
    if boat {
        return Some(PusherClass::Boat);
    }
    if !living {
        return None;
    }
    match kind {
        EntityKind::ArmorStand
        | EntityKind::Bat
        | EntityKind::Parrot
        | EntityKind::Vex
        | EntityKind::EnderDragon => None,
        _ => Some(PusherClass::Living),
    }
}

/// `AbstractBoat#push`: a bot is pushed by a boat only when its box bottom is at or below the
/// boat's box bottom.
pub fn boat_height_rule(target_min_y: f64, boat_min_y: f64) -> bool {
    target_min_y <= boat_min_y
}

/// The box a pusher searches for the local player: its own for a living entity, the inflated
/// one for a boat.
pub fn push_box(class: PusherClass, pusher_box: &Aabb) -> Aabb {
    match class {
        PusherClass::Living => *pusher_box,
        PusherClass::Boat => pusher_box.inflate(Vec3::new(BOAT_BAND_XZ, BOAT_BAND_Y, BOAT_BAND_XZ)),
    }
}

/// The local player as a push target: `LivingEntity#isPushable` (alive, not a spectator, not on
/// a climbable), no passengers and no `noPhysics` (`Entity#push`; a spectator has `noPhysics`).
/// `sleeping` only blocks pushes by living pushers (`LivingEntity#push`), not boats.
pub fn target_pushable(
    spectator: bool,
    dead: bool,
    on_climbable: bool,
    has_passengers: bool,
    sleeping: bool,
    class: PusherClass,
) -> bool {
    if spectator || dead || on_climbable || has_passengers {
        return false;
    }
    !(sleeping && class == PusherClass::Living)
}

/// Pushes the local player received, for evidence (rows and load tests read it). A "tick" is the
/// bot's own game tick as counted by the first pass.
// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[derive(Clone, Component, Debug, Default, PartialEq)]
pub struct PushRecord {
    /// Game ticks the push passes have seen.
    pub ticks: u64,
    /// Pushes applied, ever.
    pub total: u64,
    /// The tick of the last push (`ticks` then), if any.
    pub last_tick: Option<u64>,
    /// Pushes applied in the current tick.
    pub in_tick: u32,
    /// The most pushes applied in one tick.
    pub max_in_tick: u32,
    /// Velocity added in the current tick (both passes), for the trace.
    pub tick_delta: (f64, f64),
}

impl PushRecord {
    pub fn begin_tick(&mut self) {
        self.ticks += 1;
        self.in_tick = 0;
        self.tick_delta = (0.0, 0.0);
    }
    pub fn record(&mut self, dv: (f64, f64)) {
        self.total += 1;
        self.in_tick += 1;
        self.max_in_tick = self.max_in_tick.max(self.in_tick);
        self.last_tick = Some(self.ticks);
        self.tick_delta.0 += dv.0;
        self.tick_delta.1 += dv.1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn below_the_limit_nothing_happens_and_at_it_a_push_starts() {
        assert_eq!(push_velocity(0.005, 0.0), None);
        assert_eq!(push_velocity(0.0, -0.0099), None);
        let (x, z) = push_velocity(0.01, 0.0).unwrap();
        // f = 0.01, sqrt = 0.1, d = 0.1, g = 1 -> 0.1 * 0.05 = 0.005, away from the pusher.
        assert!(close(x, -0.1 * PUSH_SCALE), "{x}");
        assert_eq!(z, 0.0);
    }

    #[test]
    fn bands_follow_the_worked_check() {
        // f < 1: the larger axis gets 0.05 * sqrt(f). f >= 1: exactly 0.05.
        let (x, _) = push_velocity(0.3, 0.0).unwrap();
        assert!(close(x, -(0.05f32 as f64) * 0.3f64.sqrt()), "{x}");
        assert!((x + 0.0274).abs() < 1e-4);
        let (x, _) = push_velocity(0.75, 0.0).unwrap();
        assert!(close(x, -(0.05f32 as f64) * 0.75f64.sqrt()));
        let (x, _) = push_velocity(1.0, 0.0).unwrap();
        assert!(close(x, -(0.05f32 as f64)));
        let (x, _) = push_velocity(3.5, 0.0).unwrap();
        assert!(close(x, -(0.05f32 as f64)));
    }

    #[test]
    fn each_axis_and_the_diagonal() {
        let (x, z) = push_velocity(0.0, -0.3).unwrap();
        assert_eq!(x, 0.0);
        assert!(close(z, 0.05f32 as f64 * 0.3f64.sqrt()), "away from a pusher at -z is +z");
        // Diagonal: the larger axis decides the scale for both; the smaller axis is scaled alike.
        let (x, z) = push_velocity(0.3, 0.3).unwrap();
        assert!(close(x, z));
        assert!(close(x, -(0.05f32 as f64) * 0.3f64.sqrt()));
        let (x, z) = push_velocity(0.4, -0.2).unwrap();
        let s = 0.05f32 as f64 / 0.4f64.sqrt();
        assert!(close(x, -0.4 * s) && close(z, 0.2 * s));
        // Large diagonal: scale = 0.05 / f for f >= 1 (divide by sqrt, then by sqrt again).
        let (x, z) = push_velocity(4.0, 2.0).unwrap();
        assert!(close(x, -(0.05f32 as f64)) && close(z, -(0.05f32 as f64) * 0.5));
    }

    #[test]
    fn exceptions_by_kind() {
        use EntityKind::*;
        assert_eq!(pusher_class(true, false, Cow), Some(PusherClass::Living));
        assert_eq!(pusher_class(true, false, Player), Some(PusherClass::Living));
        assert_eq!(pusher_class(true, false, Shulker), Some(PusherClass::Living));
        for k in [ArmorStand, Bat, Parrot, Vex, EnderDragon] {
            assert_eq!(pusher_class(true, false, k), None, "{k:?}");
        }
        assert_eq!(pusher_class(false, true, OakBoat), Some(PusherClass::Boat));
        // Minecart, item, arrow: neither living nor boat.
        for k in [Minecart, Item, Arrow, ItemFrame] {
            assert_eq!(pusher_class(false, false, k), None, "{k:?}");
        }
    }

    #[test]
    fn target_conditions() {
        let l = PusherClass::Living;
        let b = PusherClass::Boat;
        assert!(target_pushable(false, false, false, false, false, l));
        assert!(!target_pushable(true, false, false, false, false, l), "spectator");
        assert!(!target_pushable(false, true, false, false, false, l), "dead");
        assert!(!target_pushable(false, false, true, false, false, l), "climbable");
        assert!(!target_pushable(false, false, false, true, false, l), "passenger");
        assert!(!target_pushable(false, false, false, false, true, l), "sleeping vs living");
        assert!(target_pushable(false, false, false, false, true, b), "sleeping vs boat");
    }

    #[test]
    fn the_boat_band_and_height_rule() {
        let boat = Aabb { min: Vec3::new(0.0, 64.0, 0.0), max: Vec3::new(1.0, 64.5, 1.0) };
        let band = push_box(PusherClass::Boat, &boat);
        assert!(close(band.min.x, -BOAT_BAND_XZ) && close(band.max.z, 1.0 + BOAT_BAND_XZ));
        assert!(close(band.min.y, 64.0 + 0.01f32 as f64) && close(band.max.y, 64.5 - 0.01f32 as f64));
        assert_eq!(push_box(PusherClass::Living, &boat), boat);
        // A bot 0.15 from the boat side (a gap) is inside the band; 0.25 is not.
        let near = Aabb { min: Vec3::new(-0.75, 64.0, 0.2), max: Vec3::new(-0.15, 65.8, 0.8) };
        let far = Aabb { min: Vec3::new(-0.85, 64.0, 0.2), max: Vec3::new(-0.25, 65.8, 0.8) };
        assert!(band.intersects_aabb(&near));
        assert!(!band.intersects_aabb(&far));
        assert!(boat_height_rule(64.0, 64.0));
        assert!(boat_height_rule(63.9, 64.0));
        assert!(!boat_height_rule(64.1, 64.0));
    }

    #[test]
    fn the_record_counts_pushes_per_tick() {
        let mut r = PushRecord::default();
        r.begin_tick();
        r.record((0.1, 0.0));
        r.record((0.0, 0.2));
        r.begin_tick();
        r.record((0.5, 0.5));
        assert_eq!((r.total, r.max_in_tick, r.in_tick, r.last_tick), (3, 2, 1, Some(2)));
        assert_eq!(r.tick_delta, (0.5, 0.5));
    }
}
