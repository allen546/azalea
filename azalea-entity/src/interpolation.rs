//! The client's interpolation of other entities' positions, and the bot-to-bot view.
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

use std::collections::HashMap;

use azalea_core::{aabb::Aabb, delta::PositionDelta8, position::{ChunkPos, Vec3}};
use azalea_world::{WorldName, Worlds};
use bevy_ecs::prelude::*;
use tracing::info;

use crate::{
    LocalEntity, LookDirection, Physics, Position,
    dimensions::EntityDimensions,
    indexing::EntityIdIndex,
    metadata::{
        AbstractBoat, AbstractLiving, AbstractMinecart, ExperienceOrb, FishingBobber, Shulker,
    },
    vec_delta_codec::{VecDeltaCodec, quantise},
};

/// `InterpolationHandler.DEFAULT_INTERPOLATION_STEPS`.
pub const DEFAULT_INTERPOLATION_STEPS: u32 = 3;
/// A jump farther than 64 blocks (squared) snaps (`handleEntityPositionSync`,
/// `setValuesFromPositionPacket`).
pub const SNAP_DISTANCE_SQ: f64 = 4096.0;
/// How far from a bot another bot is kept in its view: reach plus three ticks of movement, with
/// margin (Allen, 2026-10-20).
pub const PEER_RADIUS: f64 = 16.0;
/// The most other bots one bot keeps in its view.
pub const PEER_CAP: usize = 32;

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

/// One other bot as this bot sees it: the server's packets for it, smoothed.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
#[derive(Clone, Debug)]
pub struct PeerEntry {
    codec: VecDeltaCodec,
    state: InterpolationState,
    /// The smoothed position this bot sees.
    pub position: Vec3,
    /// The box at that position.
    pub bounding_box: Aabb,
}

impl PeerEntry {
    /// Start a view of a bot with no packet seen yet: the server's position for it is the
    /// position it last sent, which is its own simulated position, rounded as the server's
    /// packets round. Error: up to one tick of its movement, until the next absolute packet.
    pub fn seed(true_position: Vec3, rot: (f32, f32), dimensions: &EntityDimensions) -> Self {
        let position = quantise(true_position);
        Self {
            codec: VecDeltaCodec::new(position),
            state: InterpolationState::new(rot),
            position,
            bounding_box: dimensions.make_bounding_box(position),
        }
    }

    /// `handleMoveEntity` (position and/or rotation delta; `delta` None is a rotation-only packet).
    pub fn on_move(&mut self, delta: Option<&PositionDelta8>, rot: Option<(f32, f32)>) {
        let pos = delta.map(|d| {
            let p = self.codec.decode(d);
            self.codec.set_base(p);
            p
        });
        self.state.interpolate_to(DEFAULT_INTERPOLATION_STEPS, pos, rot, self.position);
    }

    /// `handleEntityPositionSync`: sets the codec base; smooths unless the jump is over 64 blocks.
    pub fn on_position_sync(&mut self, pos: Vec3, rot: (f32, f32)) {
        self.codec.set_base(pos);
        self.on_teleport(pos, rot);
    }

    /// `handleTeleportEntity`: smooths unless the jump is over 64 blocks (the codec base is not
    /// touched, as in vanilla). `pos` is absolute.
    pub fn on_teleport(&mut self, pos: Vec3, rot: (f32, f32)) {
        if self.position.distance_squared_to(pos) > SNAP_DISTANCE_SQ {
            self.snap(pos, rot);
        } else {
            self.state.interpolate_to(DEFAULT_INTERPOLATION_STEPS, Some(pos), Some(rot), self.position);
        }
    }

    /// `handleAddEntity` (`recreateFromPacket`): the codec base and the position are set at once.
    pub fn on_add(&mut self, pos: Vec3, rot: (f32, f32)) {
        self.codec.set_base(pos);
        self.snap(pos, rot);
    }

    fn snap(&mut self, pos: Vec3, rot: (f32, f32)) {
        self.position = pos;
        self.state.cancel();
        self.state.set_rot(rot);
    }

    /// The rotation last received (position-only smoothing: it is set at once).
    pub fn rot(&self) -> (f32, f32) {
        self.state.rot()
    }

    /// Steps left in the run; for tests and the load counters.
    pub fn steps_left(&self) -> u32 {
        self.state.steps_left()
    }

    /// One tick: an interpolation step, then the box at the new position.
    pub fn tick(&mut self, dimensions: &EntityDimensions) {
        if let Some(next) = self.state.step(self.position) {
            self.position = next;
        }
        self.bounding_box = dimensions.make_bounding_box(self.position);
    }
}

/// One bot's view of the other bots near it (Allen, 2026-10-20). The swarm shares one world and a
/// bot writes its own exact position, but a vanilla client sees another player where the server
/// said, smoothed. So each bot keeps its own smoothed copy of the bots within [`PEER_RADIUS`],
/// the nearest [`PEER_CAP`] of them, always including its aim target. Outside the radius nothing
/// is kept. Readers use the entry for a bot target and the shared entity for everything else.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
#[derive(Component, Debug, Default)]
pub struct PeerView {
    entries: HashMap<Entity, PeerEntry>,
    aim_target: Option<Entity>,
    peak: usize,
    cap_hits: u32,
    cap_logged: bool,
}

impl PeerView {
    pub fn get(&self, peer: Entity) -> Option<&PeerEntry> {
        self.entries.get(&peer)
    }
    pub fn get_mut(&mut self, peer: Entity) -> Option<&mut PeerEntry> {
        self.entries.get_mut(&peer)
    }
    pub fn contains(&self, peer: Entity) -> bool {
        self.entries.contains_key(&peer)
    }
    pub fn iter(&self) -> impl Iterator<Item = (Entity, &PeerEntry)> {
        self.entries.iter().map(|(e, p)| (*e, p))
    }
    /// Entries now.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The most entries this view has held.
    pub fn peak(&self) -> usize {
        self.peak
    }
    /// How many times the cap forced a bot out or kept one out (once per episode).
    pub fn cap_hits(&self) -> u32 {
        self.cap_hits
    }
    /// Keep this bot in the view whatever the cap (the bot being aimed at or clicked).
    pub fn set_aim_target(&mut self, peer: Entity) {
        self.aim_target = Some(peer);
    }
    pub fn aim_target(&self) -> Option<Entity> {
        self.aim_target
    }

    /// Make room for `peer` at `distance_sq` from the observer. Returns false when the cap keeps
    /// it out. The farthest entry that is not the aim target goes first.
    fn admit(&mut self, observer: Entity, peer: Entity, distance_sq: f64, observer_pos: Vec3) -> bool {
        if self.entries.len() < PEER_CAP {
            return true;
        }
        if !self.cap_logged {
            self.cap_logged = true;
            self.cap_hits += 1;
            info!("bot {observer:?}: bot view at its cap of {PEER_CAP}; keeping the nearest");
        }
        let farthest = self
            .entries
            .iter()
            .filter(|(e, _)| Some(**e) != self.aim_target)
            .map(|(e, p)| (*e, p.position.distance_squared_to(observer_pos)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match farthest {
            Some((out, d)) if Some(peer) == self.aim_target || distance_sq < d => {
                self.entries.remove(&out);
                true
            }
            _ => false,
        }
    }

    fn insert(&mut self, peer: Entity, entry: PeerEntry) {
        self.entries.insert(peer, entry);
        self.peak = self.peak.max(self.entries.len());
    }
}

/// Keep each bot's [`PeerView`]: give every bot one, drop bots that left the radius or the world,
/// admit the bots that came within it (nearest first, aim target first, at most [`PEER_CAP`]),
/// then step every entry and refresh its box. Packets update entries in `Update`; this runs at
/// the end of the `GameTick`, with the other entities' interpolation.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
#[allow(clippy::type_complexity)]
pub fn update_peer_views(
    mut commands: Commands,
    mut observers: Query<
        (Entity, &Position, &WorldName, &EntityIdIndex, Option<&mut PeerView>),
        With<LocalEntity>,
    >,
    peers: Query<(&Position, &EntityDimensions, &LookDirection, &WorldName), With<LocalEntity>>,
    worlds: Res<Worlds>,
) {
    let radius_sq = PEER_RADIUS * PEER_RADIUS;
    for (observer, position, world_name, ids, view) in &mut observers {
        let Some(mut view) = view else {
            commands.entity(observer).insert(PeerView::default());
            continue;
        };
        let origin: Vec3 = **position;

        view.entries.retain(|&peer, _| {
            peers.get(peer).is_ok_and(|(p, _, _, w)| {
                w == world_name && (**p).distance_squared_to(origin) <= radius_sq && ids.contains_ecs_entity(peer)
            })
        });

        let mut candidates: Vec<(bool, f64, Entity)> = Vec::new();
        if let Some(world) = worlds.get(world_name) {
            let world = world.read();
            let chunk = |v: f64| (v / 16.0).floor() as i32;
            for cx in chunk(origin.x - PEER_RADIUS)..=chunk(origin.x + PEER_RADIUS) {
                for cz in chunk(origin.z - PEER_RADIUS)..=chunk(origin.z + PEER_RADIUS) {
                    let Some(in_chunk) = world.entities_by_chunk.get(&ChunkPos::new(cx, cz)) else {
                        continue;
                    };
                    for &peer in in_chunk {
                        if peer == observer || view.entries.contains_key(&peer) {
                            continue;
                        }
                        let Ok((p, _, _, w)) = peers.get(peer) else { continue };
                        let d = (**p).distance_squared_to(origin);
                        if w == world_name && d <= radius_sq && ids.contains_ecs_entity(peer) {
                            candidates.push((Some(peer) != view.aim_target, d, peer));
                        }
                    }
                }
            }
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        for (_, d, peer) in candidates {
            if !view.admit(observer, peer, d, origin) {
                break;
            }
            let (p, dims, look, _) = peers.get(peer).expect("checked above");
            view.insert(peer, PeerEntry::seed(**p, (look.y_rot(), look.x_rot()), dims));
        }
        if view.entries.len() < PEER_CAP {
            view.cap_logged = false;
        }

        for (peer, entry) in &mut view.entries {
            if let Ok((_, dims, _, _)) = peers.get(*peer) {
                entry.tick(dims);
            }
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

    #[test]
    fn a_peer_is_added_snapped_and_moves_over_three_ticks() {
        let d = dims();
        let mut e = PeerEntry::seed(v(0., 64., 0.), (0., 0.), &d);
        e.on_add(v(0., 64., 0.), (0., 0.));
        // A delta of 12288 units (3 blocks) on x.
        e.on_move(Some(&PositionDelta8 { xa: 12288, ya: 0, za: 0 }), None);
        e.tick(&d);
        assert!((e.position.x - 1.0).abs() < 1e-9);
        assert!((e.bounding_box.min.x - (1.0 - 0.3)).abs() < 1e-6, "box refreshed in the same tick");
        e.tick(&d);
        assert!((e.position.x - 2.0).abs() < 1e-9);
        e.tick(&d);
        assert!((e.position.x - 3.0).abs() < 1e-9);
        // The next delta is relative to the decoded target, not to the smoothed position.
        e.on_move(Some(&PositionDelta8 { xa: 4096, ya: 0, za: 0 }), None);
        e.tick(&d);
        assert!((e.position.x - (3.0 + 1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn a_jump_over_64_blocks_snaps() {
        let d = dims();
        let mut e = PeerEntry::seed(v(0., 64., 0.), (0., 0.), &d);
        e.on_teleport(v(100., 64., 0.), (0., 0.));
        assert_eq!(e.position.x, 100.0);
        assert_eq!(e.steps_left(), 0);
        e.on_position_sync(v(110., 64., 0.), (0., 0.));
        assert_eq!(e.steps_left(), 3);
        assert_eq!(e.position.x, 100.0);
    }

    #[test]
    fn the_cap_keeps_the_nearest_and_the_aim_target() {
        let d = dims();
        let observer = Entity::from_raw_u32(1).unwrap();
        let mut view = PeerView::default();
        let o = v(0., 0., 0.);
        // Fill to the cap with bots at distance 1..=32 blocks (positions are the sort keys).
        for i in 0..PEER_CAP {
            view.insert(
                Entity::from_raw_u32(100 + i as u32).unwrap(),
                PeerEntry::seed(v(1.0 + i as f64 * 0.4, 0., 0.), (0., 0.), &d),
            );
        }
        assert_eq!(view.len(), PEER_CAP);
        let far = Entity::from_raw_u32(500).unwrap();
        // A farther bot is kept out; the cap is logged once.
        assert!(!view.admit(observer, far, 14.0 * 14.0, o));
        assert!(!view.admit(observer, far, 14.0 * 14.0, o));
        assert_eq!(view.cap_hits(), 1);
        // A nearer bot pushes the farthest out.
        let near = Entity::from_raw_u32(501).unwrap();
        assert!(view.admit(observer, near, 0.25, o));
        assert_eq!(view.len(), PEER_CAP - 1);
        view.insert(near, PeerEntry::seed(v(0.5, 0., 0.), (0., 0.), &d));
        // The aim target gets in whatever its distance, and is never the one pushed out.
        view.set_aim_target(far);
        assert!(view.admit(observer, far, 15.0 * 15.0, o));
        view.insert(far, PeerEntry::seed(v(15., 0., 0.), (0., 0.), &d));
        assert_eq!(view.len(), PEER_CAP);
        assert!(view.admit(observer, Entity::from_raw_u32(502).unwrap(), 0.01, o));
        assert!(view.contains(far));
        assert_eq!(view.peak(), PEER_CAP);
    }
}
