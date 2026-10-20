//! Server-set inputs of the client's own computations that the fork does not model.
//!
//! A bot whose attribute, status effect, vehicle or surroundings change what a vanilla client
//! computes (hitbox, motion, ...) must not move or break on a wrong prediction. This lists the
//! ones in force so the layer above can stop, and `azalea-client` disconnects the bot when one
//! of them moves it by itself. Spec: specs/F5-version-layer.md §4.6, specs/B1-movement.md §4.2.

use azalea_core::{aabb::Aabb, entity_id::MinecraftEntityId, position::BlockPos};
use azalea_registry::builtin::{Attribute, BlockKind, MobEffect};

use crate::{ActiveEffects, Attributes};

/// What an unmodelled input changes. `Movement` changes the bot's motion even with no input, so
/// stopping the skill is not enough (the client keeps simulating and sending positions): the
/// client disconnects the bot (patch 30). `Action` changes only what an action or a plan
/// computes (reach, break time, the gravity a skill plans with).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmodelledKind {
    Movement,
    Action,
}

/// One unmodelled input in force.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnmodelledInput {
    /// The attribute's current value.
    Attribute(Attribute, f64),
    /// The effect's amplifier (0 for level I).
    Effect(MobEffect, i32),
    /// The bot is a passenger (`Entity#isPassenger`); the vehicle moves it.
    Passenger,
    /// The bot's box touches a block whose effect on motion the fork does not model.
    Block(BlockPos, BlockKind),
    /// The bot's box is outside or at the world border (`WorldBorder` collision).
    WorldBorder,
}

impl UnmodelledInput {
    /// Scale (hitbox and eye height) and Dolphin's Grace change the motion every tick, as do a
    /// vehicle, a block the fork does not model and the world border. Levitation and Slow Falling
    /// are modelled (patch 30), but skills and the pathfinder plan with normal gravity, so they
    /// only stop the skill (Allen, 2026-10-20, P30.2).
    // Spec: specs/F5-version-layer.md §4.6
    pub fn kind(&self) -> UnmodelledKind {
        match self {
            UnmodelledInput::Effect(MobEffect::Levitation | MobEffect::SlowFalling, _) => {
                UnmodelledKind::Action
            }
            _ => UnmodelledKind::Movement,
        }
    }
}

/// Status effects the client reads for its own movement. Dolphin's Grace is not modelled
/// (`LivingEntity#travelInWater`) and is a `Movement` entry; Levitation
/// (`LivingEntity#travelInAir`) and Slow Falling (`#getEffectiveGravity`) are modelled and are
/// `Action` entries. Blindness (`Player#isMobilityRestricted`, sprinting), jump boost, haste,
/// conduit power, mining fatigue, weaving, speed and slowness are modelled and not listed.
const LISTED_EFFECTS: [MobEffect; 3] = [
    MobEffect::Levitation,
    MobEffect::SlowFalling,
    MobEffect::DolphinsGrace,
];

/// Every unmodelled attribute or effect in force; empty when the client's predictions hold. The
/// vehicle, block and border entries come from [`UnmodelledContacts`], which `azalea-client`
/// derives at the start of each tick.
// Spec: specs/F5-version-layer.md §4.6
pub fn unmodelled_inputs(attributes: &Attributes, effects: &ActiveEffects) -> Vec<UnmodelledInput> {
    let mut out: Vec<UnmodelledInput> = attributes
        .unmodelled_in_force()
        .into_iter()
        .map(|(a, v)| UnmodelledInput::Attribute(a, v))
        .collect();
    for effect in LISTED_EFFECTS {
        if let Some(level) = effects.get_level(effect) {
            out.push(UnmodelledInput::Effect(effect, level));
        }
    }
    out
}

/// The `Passenger`, `Block` and `WorldBorder` entries in force, set at the start of each
/// `GameTick` by the unmodelled-movement gate in `azalea-client`.
#[cfg_attr(feature = "bevy_ecs", derive(bevy_ecs::component::Component))]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnmodelledContacts(pub Vec<UnmodelledInput>);

/// The vehicle the local player rides, from `ClientboundSetPassengers`
/// (`ClientPacketListener#handleSetEntityPassengers`). Absent when the bot is not a passenger.
#[cfg_attr(feature = "bevy_ecs", derive(bevy_ecs::component::Component))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RidingVehicle(pub MinecraftEntityId);

/// The world border as the server last described it (`ClientboundInitializeBorder`,
/// `SetBorderCenter`, `SetBorderSize`, `SetBorderLerpSize`), so the gate can tell when the bot's
/// box is outside or at it. Absent until the server sends one.
#[cfg_attr(feature = "bevy_ecs", derive(bevy_ecs::component::Component))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldBorderState {
    pub center_x: f64,
    pub center_z: f64,
    /// Size at `start_tick`, and the size it moves to over `lerp_ticks` (`WorldBorder.MovingBorderExtent`).
    pub from_size: f64,
    pub to_size: f64,
    pub start_tick: u64,
    pub lerp_ticks: u64,
}

impl WorldBorderState {
    /// The border's side length at `tick` (the local tick count).
    pub fn size_at(&self, tick: u64) -> f64 {
        if self.lerp_ticks == 0 || tick >= self.start_tick + self.lerp_ticks {
            self.to_size
        } else {
            let t = tick.saturating_sub(self.start_tick) as f64 / self.lerp_ticks as f64;
            self.from_size + (self.to_size - self.from_size) * t
        }
    }

    /// Whether the box is outside the border or touches its edge (within 1e-3, the margin the
    /// block test uses too).
    // Spec: specs/F5-version-layer.md §4.6
    pub fn touches(&self, bb: &Aabb, tick: u64) -> bool {
        let half = self.size_at(tick) / 2.0;
        const M: f64 = 1e-3;
        bb.min.x <= self.center_x - half + M
            || bb.max.x >= self.center_x + half - M
            || bb.min.z <= self.center_z - half + M
            || bb.max.z >= self.center_z + half - M
    }
}

/// Whether a block of this kind changes a bot's motion in a way the fork does not model:
/// `BubbleColumnBlock#entityInside`, `SlimeBlock#stepOn` and `#updateEntityMovementAfterFallOn`,
/// `BedBlock#updateEntityMovementAfterFallOn`, `ScaffoldingBlock#getCollisionShape` (depends on
/// the entity), `PistonMovingBlockEntity#moveCollidedEntities`. Honey is handled apart
/// ([`honey_touches_side`]): standing on top is modelled (patch 26).
// Spec: specs/B1-movement.md §4.3
pub fn is_unmodelled_block(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::BubbleColumn
            | BlockKind::SlimeBlock
            | BlockKind::Scaffolding
            | BlockKind::MovingPiston
    ) || kind.to_str().ends_with("_bed")
}

/// `HoneyBlock#entityInside` slides a bot that is in the block's cell below its top (15/16) and
/// not on the ground; standing on the top is not that. The box touches the cell from the side
/// when its feet are below the top of the honey.
// Spec: specs/B1-movement.md §4.3
pub fn honey_touches_side(bb: &Aabb, cell: BlockPos) -> bool {
    bb.min.y < cell.y as f64 + 0.9375 - 1e-7
}

#[cfg(test)]
mod tests {
    use azalea_core::position::Vec3;
    use azalea_registry::builtin::EntityKind;

    use super::*;
    use crate::MobEffectData;

    // An effect entry appears when the server adds it and goes when it is removed.
    #[test]
    fn effects_are_listed_while_active() {
        let attrs = Attributes::new(EntityKind::Player);
        let mut fx = ActiveEffects::default();
        assert!(unmodelled_inputs(&attrs, &fx).is_empty());
        fx.insert(MobEffect::Levitation, MobEffectData { amplifier: 1, duration: 100, ..Default::default() });
        fx.insert(MobEffect::Speed, MobEffectData::default());
        assert_eq!(unmodelled_inputs(&attrs, &fx), vec![UnmodelledInput::Effect(MobEffect::Levitation, 1)]);
        fx.remove(MobEffect::Levitation);
        assert!(unmodelled_inputs(&attrs, &fx).is_empty());
    }

    // Levitation and Slow Falling are modelled (patch 30): they stop the skill, not the bot.
    // Dolphin's Grace and a scale away from 1 move the bot and disconnect it.
    #[test]
    fn entry_kinds() {
        use UnmodelledKind::*;
        let e = |m| UnmodelledInput::Effect(m, 0);
        assert_eq!(e(MobEffect::Levitation).kind(), Action);
        assert_eq!(e(MobEffect::SlowFalling).kind(), Action);
        assert_eq!(e(MobEffect::DolphinsGrace).kind(), Movement);
        assert_eq!(UnmodelledInput::Attribute(Attribute::Scale, 2.0).kind(), Movement);
        assert_eq!(UnmodelledInput::Passenger.kind(), Movement);
        assert_eq!(UnmodelledInput::WorldBorder.kind(), Movement);
        assert_eq!(UnmodelledInput::Block(BlockPos::new(0, 0, 0), BlockKind::BubbleColumn).kind(), Movement);
    }

    // An effect stays until the server removes it; the duration is never counted down
    // (`MobEffectInstance#tickClient` does not remove it on the client).
    #[test]
    fn an_effect_at_duration_zero_stays() {
        let attrs = Attributes::new(EntityKind::Player);
        let mut fx = ActiveEffects::default();
        fx.insert(MobEffect::SlowFalling, MobEffectData { amplifier: 0, duration: 0, ..Default::default() });
        assert_eq!(unmodelled_inputs(&attrs, &fx), vec![UnmodelledInput::Effect(MobEffect::SlowFalling, 0)]);
    }

    #[test]
    fn block_kinds() {
        for kind in [BlockKind::BubbleColumn, BlockKind::SlimeBlock, BlockKind::Scaffolding, BlockKind::MovingPiston, BlockKind::WhiteBed, BlockKind::RedBed] {
            assert!(is_unmodelled_block(kind), "{kind:?}");
        }
        for kind in [BlockKind::Stone, BlockKind::HoneyBlock, BlockKind::Cobweb, BlockKind::Water, BlockKind::OakSlab] {
            assert!(!is_unmodelled_block(kind), "{kind:?}");
        }
    }

    // Honey: standing on top is modelled; feet below the 15/16 top touch it from the side.
    #[test]
    fn honey_side_vs_top() {
        let cell = BlockPos::new(0, 70, 0);
        let on_top = Aabb { min: Vec3::new(0.2, 70.9375, 0.2), max: Vec3::new(0.8, 72.7, 0.8) };
        let beside = Aabb { min: Vec3::new(-0.5, 70.0, 0.2), max: Vec3::new(0.05, 71.8, 0.8) };
        assert!(!honey_touches_side(&on_top, cell));
        assert!(honey_touches_side(&beside, cell));
    }

    #[test]
    fn world_border_touch_and_lerp() {
        let mut b = WorldBorderState { center_x: 0.0, center_z: 0.0, from_size: 100.0, to_size: 100.0, start_tick: 0, lerp_ticks: 0 };
        let at = |x: f64| Aabb { min: Vec3::new(x - 0.3, 70.0, -0.3), max: Vec3::new(x + 0.3, 71.8, 0.3) };
        assert!(!b.touches(&at(10.0), 5));
        assert!(b.touches(&at(49.8), 5));
        assert!(b.touches(&at(60.0), 5));
        assert!(b.touches(&at(-49.8), 5));
        // shrinking from 100 to 20 over 10 ticks: halfway the border is 60 wide
        b.from_size = 100.0;
        b.to_size = 20.0;
        b.start_tick = 0;
        b.lerp_ticks = 10;
        assert_eq!(b.size_at(5), 60.0);
        assert!(b.touches(&at(29.9), 5));
        assert!(!b.touches(&at(29.9), 0));
        assert_eq!(b.size_at(50), 20.0);
    }
}
