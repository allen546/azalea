//! The client's interpolation of other entities' positions.
//!
//! Null fork, patch 31. A vanilla client does not put another entity at the position the server
//! sent. It moves the entity toward that position over a few ticks. Pushes, the crosshair ray and
//! attack reach all read the smoothed position, so the fork computes it as vanilla does.
//!
//! Vanilla 1.21.11, Mojang names: `InterpolationHandler#interpolateTo` and `#interpolate`;
//! `Entity#moveOrInterpolateTo`; `ClientPacketListener#handleMoveEntity`,
//! `#handleEntityPositionSync`, `#handleTeleportEntity` (`setValuesFromPositionPacket`) and
//! `#handleAddEntity`. Only the position is smoothed: no client decision reads another entity's
//! rotation (Allen, 2026-10-20), so a rotation is set at once and kept here only for the "same
//! target" test.
//!
//! Grim checks this must stay clean for: `Reach` and `Hitboxes` (the server's own boxes, widened
//! by the interpolation range) and `MovementTicker#handleEntityCollisions`.
//!
//! Spec: specs/F5-version-layer.md §4.6 (What Null computes), specs/B1-movement.md §4.2

use azalea_core::position::Vec3;
use bevy_ecs::prelude::*;

use crate::{
    LocalEntity, LookDirection, Physics, Position,
    dimensions::EntityDimensions,
    metadata::{
        AbstractBoat, AbstractLiving, AbstractMinecart, ExperienceOrb, FishingBobber, Shulker,
    },
};

/// `InterpolationHandler.DEFAULT_INTERPOLATION_STEPS`.
pub const DEFAULT_INTERPOLATION_STEPS: u32 = 3;
/// A jump farther than 64 blocks (squared) snaps (`handleEntityPositionSync`,
/// `setValuesFromPositionPacket`).
pub const SNAP_DISTANCE_SQ: f64 = 4096.0;
/// What `interpolate_to` did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Interpolate {
    /// The entity has no steps: the caller sets the values at once.
    Snap,
    /// A new run of steps started from the entity's current position.
    Started,
    /// The target equals the active one: nothing changed.
    Ignored,
}

/// `InterpolationHandler`, position part: the target, and how many steps are left to reach it.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InterpolationState {
    left: u32,
    target: Vec3,
    rot: (f32, f32),
}

impl InterpolationState {
    /// An idle state. `rot` is the entity's rotation now.
    pub fn new(rot: (f32, f32)) -> Self {
        Self { left: 0, target: Vec3::ZERO, rot }
    }
    /// `hasActiveInterpolation`.
    pub fn is_active(&self) -> bool {
        self.left > 0
    }
    pub fn steps_left(&self) -> u32 {
        self.left
    }
    /// `InterpolationHandler#position`: the target while a run is active, else `current`.
    pub fn position(&self, current: Vec3) -> Vec3 {
        if self.is_active() { self.target } else { current }
    }
    pub fn rot(&self) -> (f32, f32) {
        self.rot
    }
    pub fn set_rot(&mut self, rot: (f32, f32)) {
        self.rot = rot;
    }
    /// `InterpolationHandler#cancel`.
    pub fn cancel(&mut self) {
        self.left = 0;
    }

    /// `Entity#moveOrInterpolateTo` for an entity with a handler of `steps` steps (`steps` 0 is
    /// `Display`: set at once). A missing part keeps the handler's current one. A run restarts
    /// when it is idle or the target differs in position or rotation.
    pub fn interpolate_to(
        &mut self,
        steps: u32,
        pos: Option<Vec3>,
        rot: Option<(f32, f32)>,
        current: Vec3,
    ) -> Interpolate {
        let rot = rot.unwrap_or(self.rot);
        if steps == 0 {
            self.cancel();
            self.rot = rot;
            return Interpolate::Snap;
        }
        let pos = pos.unwrap_or_else(|| self.position(current));
        if self.is_active() && self.rot == rot && self.target == pos {
            return Interpolate::Ignored;
        }
        self.left = steps;
        self.target = pos;
        self.rot = rot;
        Interpolate::Started
    }

    /// The position the next [`step`](Self::step) would give, without taking it. The pushes use
    /// it for entities that tick before the local player: vanilla has already taken their step
    /// in this tick (patch 32).
    pub fn peek(&self, current: Vec3) -> Vec3 {
        if !self.is_active() {
            return current;
        }
        let d = 1.0 / self.left as f64;
        let lerp = |a: f64, b: f64| a + d * (b - a);
        Vec3::new(
            lerp(current.x, self.target.x),
            lerp(current.y, self.target.y),
            lerp(current.z, self.target.z),
        )
    }

    /// `InterpolationHandler#interpolate`, once per tick: move `1 / steps left` of the way to the
    /// target on each axis, then count one step off. The displacement shift for riders and
    /// carried entities is not modelled. Returns the new position, or `None` when idle.
    pub fn step(&mut self, current: Vec3) -> Option<Vec3> {
        if !self.is_active() {
            return None;
        }
        let d = 1.0 / self.left as f64;
        let lerp = |a: f64, b: f64| a + d * (b - a);
        let next = Vec3::new(
            lerp(current.x, self.target.x),
            lerp(current.y, self.target.y),
            lerp(current.z, self.target.z),
        );
        self.left -= 1;
        Some(next)
    }
}

/// The steps of the entity's interpolation handler: 3 for living entities (not the shulker, which
/// has none), boats, experience orbs, fishing hooks and minecarts (old behaviour); 0 for
/// everything else (`Display` has a handler of 0 steps; items and projectiles have none).
/// `NewMinecartBehavior` moves by `ClientboundMoveMinecartAlongTrack`, which the fork does not
/// handle, so a server that uses it leaves its minecarts where they were.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
pub fn interpolation_steps(entity: &EntityWorldMut) -> u32 {
    let living = entity.contains::<AbstractLiving>() && !entity.contains::<Shulker>();
    if living
        || entity.contains::<AbstractBoat>()
        || entity.contains::<ExperienceOrb>()
        || entity.contains::<FishingBobber>()
        || entity.contains::<AbstractMinecart>()
    {
        DEFAULT_INTERPOLATION_STEPS
    } else {
        0
    }
}

/// A non-bot entity's interpolation state. The entity's [`Position`] is the smoothed position;
/// this holds the server's target. Added by the first server move.
#[derive(Clone, Component, Copy, Debug, PartialEq)]
pub struct EntityInterpolation(pub InterpolationState);

/// Apply a server position and rotation to a shared (non-bot) entity as `moveOrInterpolateTo`
/// does: smooth the position when the entity has steps, else set it at once. The rotation is
/// always set at once. `snap` forces the at-once path (add, a jump over 64 blocks).
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
pub fn apply_server_pose(
    entity: &mut EntityWorldMut,
    pos: Option<Vec3>,
    rot: Option<LookDirection>,
    snap: bool,
) {
    let steps = if snap { 0 } else { interpolation_steps(entity) };
    let Some(current) = entity.get::<Position>().map(|p| **p) else { return };
    let look = entity.get::<LookDirection>().copied();
    if let Some(rot) = rot
        && look != Some(rot)
        && let Some(mut l) = entity.get_mut::<LookDirection>()
    {
        *l = rot;
    }
    let rot_pair = rot.map(|r| (r.y_rot(), r.x_rot()));
    let initial_rot = look.map_or((0., 0.), |l| (l.y_rot(), l.x_rot()));

    let mut state = entity
        .get::<EntityInterpolation>()
        .map_or_else(|| InterpolationState::new(initial_rot), |s| s.0);
    let outcome = state.interpolate_to(steps, pos, rot_pair, current);
    if outcome == Interpolate::Snap
        && let Some(pos) = pos
        && pos != current
        && let Some(mut p) = entity.get_mut::<Position>()
    {
        **p = pos;
    }
    if outcome == Interpolate::Snap && !entity.contains::<EntityInterpolation>() {
        return;
    }
    entity.insert(EntityInterpolation(state));
}

/// One interpolation step for every non-bot entity with a run in progress, at the point in the
/// tick where vanilla ticks the entity (`LivingEntity#aiStep`, `AbstractBoat#tick`,
/// `OldMinecartBehavior#tick`: after this tick's packets and clicks). It refreshes the entity's
/// bounding box at once, so the next tick's ray and reach checks read the moved box.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
pub fn interpolate_entities(
    mut query: Query<
        (&mut Position, &mut EntityInterpolation, &EntityDimensions, &mut Physics),
        Without<LocalEntity>,
    >,
) {
    for (mut position, mut interpolation, dimensions, mut physics) in &mut query {
        if !interpolation.0.is_active() {
            continue;
        }
        let Some(next) = interpolation.0.step(**position) else { continue };
        if next != **position {
            **position = next;
            physics.bounding_box = dimensions.make_bounding_box(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3::new(x, y, z)
    }
    fn dims() -> EntityDimensions {
        EntityDimensions::new(0.6, 1.8)
    }

    #[test]
    fn steps_are_a_third_a_half_then_the_rest() {
        let mut s = InterpolationState::new((0., 0.));
        let mut at = v(0., 0., 0.);
        assert_eq!(s.interpolate_to(3, Some(v(9., 0., 0.)), None, at), Interpolate::Started);
        at = s.step(at).unwrap();
        assert!((at.x - 3.0).abs() < 1e-12);
        at = s.step(at).unwrap();
        assert!((at.x - 6.0).abs() < 1e-12);
        at = s.step(at).unwrap();
        assert!((at.x - 9.0).abs() < 1e-12);
        assert!(!s.is_active());
        assert!(s.step(at).is_none());
    }

    #[test]
    fn peek_shows_the_next_step_without_taking_it() {
        let mut s = InterpolationState::new((0., 0.));
        let at = v(0., 0., 0.);
        assert_eq!(s.peek(at), at, "idle: where it is");
        s.interpolate_to(3, Some(v(9., 0., 0.)), None, at);
        assert_eq!(s.peek(at), v(3., 0., 0.));
        assert_eq!(s.steps_left(), 3, "peeking takes no step");
        let next = s.step(at).unwrap();
        assert_eq!(next, v(3., 0., 0.));
        assert_eq!(s.peek(next), v(6., 0., 0.));
    }

    #[test]
    fn an_equal_target_is_ignored_and_a_new_one_restarts() {
        let mut s = InterpolationState::new((10., 5.));
        let at = v(0., 0., 0.);
        s.interpolate_to(3, Some(v(3., 0., 0.)), None, at);
        let at = s.step(at).unwrap();
        assert_eq!(s.interpolate_to(3, Some(v(3., 0., 0.)), None, at), Interpolate::Ignored);
        assert_eq!(s.steps_left(), 2);
        // A different rotation restarts the run toward the same target.
        assert_eq!(
            s.interpolate_to(3, Some(v(3., 0., 0.)), Some((20., 5.)), at),
            Interpolate::Started
        );
        assert_eq!(s.steps_left(), 3);
        // A rotation-only packet keeps the active target.
        assert_eq!(s.position(at), v(3., 0., 0.));
        s.interpolate_to(3, None, Some((30., 5.)), at);
        assert_eq!(s.position(at), v(3., 0., 0.));
    }

    #[test]
    fn zero_steps_snap() {
        let mut s = InterpolationState::new((0., 0.));
        assert_eq!(s.interpolate_to(0, Some(v(1., 1., 1.)), Some((1., 2.)), v(0., 0., 0.)), Interpolate::Snap);
        assert!(!s.is_active());
        assert_eq!(s.rot(), (1., 2.));
    }
}
