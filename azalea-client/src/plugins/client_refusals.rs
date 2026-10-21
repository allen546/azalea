//! Client refusals for movement the fork does not model (patch 34, AD-F5.5 tier 1).
//!
//! The rule: the bot never starts a movement the fork does not predict. It refuses on the client,
//! with a reason, and sends nothing. A refusal must trigger exactly when a vanilla client would
//! start the unmodelled behaviour: earlier refuses a safe input, later is a ban.
//!
//! - Elytra: a jump edge in the air with a glider worn ([`refuse_elytra_start`]). Vanilla
//!   (`LocalPlayer#aiStep`) then sends `START_FALL_FLYING` and glides on the client. Grim:
//!   `ElytraA`-`ElytraI`, `PredictionEngineElytra`.
//! - Riptide: the start of a use of a trident whose spin strength is above 0
//!   ([`use_start_refusal`]). Vanilla (`TridentItem#releaseUsing`) launches the player on the
//!   client. Grim models only the vanilla Riptide enchantment (`Riptide`); anything else draws
//!   `Simulation`.
//! - Item use speed: the start of a use of a stack whose `use_effects` differs from the default
//!   speed or sprint rule ([`use_start_refusal`]). The fork hard-codes the 0.2 slow-down.
//! - Mounting: see `entity_actions::click_mounts`; the refusal there is
//!   `EntityUseRefusal::WouldMount`.
//!
//! Every refusal is noted in [`RespawnLog::refused_inputs`]; `BotView::refused_inputs` reads it.
//! Spec: specs/F5-version-layer.md §4.6, specs/B1-movement.md §4.2.

use std::collections::HashMap;

use azalea_core::{
    registry_holder::{
        RegistryHolder,
        components::TridentSpinAttackStrength,
        value::ValueEffect,
    },
    data_registry::ResolvableDataRegistry,
    tick::GameTick,
};
use azalea_entity::{
    ActiveEffects, Jumping, OnClimbable, Physics, inventory::Inventory,
    metadata::FallFlying, unmodelled::RidingVehicle,
};
use azalea_inventory::{ItemStack, components};
use azalea_physics::PhysicsSystems;
use azalea_registry::{builtin::{ItemKind, MobEffect}, data::Enchantment};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::respawn::RespawnLog;

/// Something the fork refused to start because it does not model what follows.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputRefusal {
    /// A jump edge in the air with a glider worn: vanilla would start a glide.
    ElytraStart,
    /// A trident with a spin attack (Riptide or a datapack equivalent).
    Riptide,
    /// An item whose `use_effects` changes the use speed or lets the player sprint.
    UseEffects,
}

impl InputRefusal {
    pub fn name(self) -> &'static str {
        match self {
            InputRefusal::ElytraStart => "elytra_start",
            InputRefusal::Riptide => "riptide",
            InputRefusal::UseEffects => "use_effects",
        }
    }

    /// A stable text for the layer above and the logs.
    pub fn as_str(self) -> &'static str {
        match self {
            InputRefusal::ElytraStart => {
                "a jump in the air with a glider worn starts a glide, which the fork does not model"
            }
            InputRefusal::Riptide => {
                "a trident with a spin attack launches the player, which the fork does not model"
            }
            InputRefusal::UseEffects => {
                "the item's use_effects change the use speed or sprinting, which the fork does not model"
            }
        }
    }
}

impl RespawnLog {
    /// Notes a refused input (nothing was sent).
    // Spec: specs/F5-version-layer.md §4.6
    pub fn refuse_input(&mut self, why: InputRefusal) {
        tracing::debug!("{} refused on the client: {}", why.name(), why.as_str());
        match self.refused_inputs.iter_mut().find(|(a, _)| *a == why) {
            Some((_, n)) => *n += 1,
            None => self.refused_inputs.push((why, 1)),
        }
    }
}

/// The jump input of the previous tick, as `LocalPlayer#aiStep` reads it before `input.tick()`.
#[derive(Clone, Component, Copy, Debug, Default, Eq, PartialEq)]
pub struct PreviousJump(pub bool);

pub struct ClientRefusalsPlugin;
impl Plugin for ClientRefusalsPlugin {
    fn build(&self, app: &mut App) {
        // After the unmodelled gate and the death-screen refusal (which clear inputs themselves),
        // before physics reads `Jumping` and before movement packets go out.
        app.add_systems(
            GameTick,
            refuse_elytra_start
                .after(crate::unmodelled_disconnect::disconnect_on_unmodelled_movement)
                .after(crate::respawn::refuse_input_while_dead)
                .before(crate::item_use::handle_use_item_queued)
                .before(crate::interact::handle_start_use_item_queued)
                .before(PhysicsSystems)
                .before(crate::movement::send_position),
        );
    }
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: elytra)
/// `LivingEntity#canGlideUsing`: the stack has the `glider` component, its `equippable` slot is
/// the slot it sits in, and one more point of damage would not break it. Any item can be a
/// glider; it need not be an elytra.
pub fn can_glide_using(stack: &ItemStack, slot: components::EquipmentSlot) -> bool {
    let ItemStack::Present(_) = stack else { return false };
    if stack.get_component::<components::Glider>().is_none() {
        return false;
    }
    match stack.get_component::<components::Equippable>() {
        Some(e) if e.slot == slot => !next_damage_will_break(stack),
        _ => false,
    }
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: elytra)
/// `ItemStack#nextDamageWillBreak`: a damageable stack one point from its maximum.
pub fn next_damage_will_break(stack: &ItemStack) -> bool {
    let Some(max) = stack.get_component::<components::MaxDamage>() else { return false };
    if stack.get_component::<components::Unbreakable>().is_some() {
        return false;
    }
    let damage = stack.get_component::<components::Damage>().map_or(0, |d| d.amount);
    damage >= max.amount - 1
}

/// The equipment slots `canGlide` scans (`EquipmentSlot.VALUES`). Body and saddle belong to the
/// animals; the player has none.
const GLIDE_SLOTS: [components::EquipmentSlot; 6] = [
    components::EquipmentSlot::Mainhand,
    components::EquipmentSlot::Offhand,
    components::EquipmentSlot::Feet,
    components::EquipmentSlot::Legs,
    components::EquipmentSlot::Chest,
    components::EquipmentSlot::Head,
];

// Spec: specs/F5-version-layer.md §4.6 (client refusals: elytra)
/// Whether some equipment slot passes [`can_glide_using`].
pub fn wears_glider(inventory: &Inventory) -> bool {
    GLIDE_SLOTS
        .iter()
        .any(|&slot| inventory.get_equipment(slot).is_some_and(|s| can_glide_using(s, slot)))
}

/// What the glide start rule reads, from the state at the end of the last tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct GlideInputs {
    pub jump_now: bool,
    pub jump_before: bool,
    pub on_climbable: bool,
    pub already_gliding: bool,
    pub in_water: bool,
    pub on_ground: bool,
    pub passenger: bool,
    pub levitation: bool,
    pub wears_glider: bool,
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: elytra)
/// Whether a vanilla client sends `START_FALL_FLYING` this tick (`LocalPlayer#aiStep`): the jump
/// key is down now and was not last tick, the player is not on a climbable block, and
/// `Player#tryToStartFallFlying` holds (not gliding, not in water, `LivingEntity#canGlide`: not
/// on the ground, not a passenger, no Levitation, a glider worn). The flying-toggle exception
/// (`abilities.flying` toggled this tick) needs `mayfly`, which a survival bot never has; this
/// function ignores it and so refuses a little more than vanilla would.
pub fn glide_would_start(i: GlideInputs) -> bool {
    i.jump_now
        && !i.jump_before
        && !i.on_climbable
        && !i.already_gliding
        && !i.in_water
        && !i.on_ground
        && !i.passenger
        && !i.levitation
        && i.wears_glider
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: elytra)
/// Clears a jump edge that would start a glide, sends nothing, and logs the refusal. A held jump
/// is not an edge and is left alone. See [`glide_would_start`].
#[allow(clippy::type_complexity)]
pub fn refuse_elytra_start(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut Jumping,
        Option<&mut PreviousJump>,
        &Physics,
        &Inventory,
        &ActiveEffects,
        &OnClimbable,
        Option<&FallFlying>,
        Option<&RidingVehicle>,
        Option<&mut RespawnLog>,
    )>,
) {
    for (entity, mut jumping, previous, physics, inventory, effects, climbable, flying, vehicle, log) in
        &mut query
    {
        let before = previous.as_ref().is_some_and(|p| p.0);
        let now = **jumping;
        let refuse = glide_would_start(GlideInputs {
            jump_now: now,
            jump_before: before,
            on_climbable: **climbable,
            already_gliding: flying.is_some_and(|f| f.0),
            in_water: physics.is_in_water(),
            on_ground: physics.on_ground(),
            passenger: vehicle.is_some(),
            levitation: effects.get_level(MobEffect::Levitation).is_some(),
            wears_glider: wears_glider(inventory),
        });
        // Vanilla keeps the key state whatever happens, so the next tick sees this one.
        match previous {
            Some(mut p) => p.0 = now,
            None => {
                commands.entity(entity).insert(PreviousJump(now));
            }
        }
        if refuse {
            **jumping = false;
            if let Some(mut log) = log {
                log.refuse_input(InputRefusal::ElytraStart);
            }
        }
    }
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: riptide)
/// What `ValueEffect` makes of `base` (vanilla `ValueEffect#process`, without randomness:
/// `RemoveBinomial` only removes, so it keeps the base here).
pub fn apply_value_effect(effect: &ValueEffect, level: i32, base: f32) -> f32 {
    match effect {
        ValueEffect::Set { value } => value.calculate(level),
        ValueEffect::Add { value } => base + value.calculate(level),
        ValueEffect::Multiply { factor } => base * factor.calculate(level),
        ValueEffect::RemoveBinomial { .. } => base,
        ValueEffect::AllOf { effects } => {
            effects.iter().fold(base, |b, e| apply_value_effect(e, level, b))
        }
        ValueEffect::Exponential { base: b, exponent } => {
            base * (b.calculate(level) as f64).powf(exponent.calculate(level) as f64) as f32
        }
    }
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: riptide)
/// Whether `EnchantmentHelper#getTridentSpinAttackStrength` may be above 0 for these enchantments.
/// `effects_of` gives the `trident_spin_attack_strength` effects of an enchantment, or `None`
/// when it is not in the registry: unknown counts as above 0 (refuse). The sum is taken over all
/// enchantments, and each alone is checked too, so an order-dependent datapack cannot slip past.
pub fn trident_spin_may_be_positive(
    levels: &HashMap<Enchantment, i32>,
    effects_of: impl Fn(&Enchantment) -> Option<Vec<ValueEffect>>,
) -> bool {
    let mut total = 0.0_f32;
    for (enchant, &level) in levels {
        let Some(effects) = effects_of(enchant) else { return true };
        let alone = effects.iter().fold(0.0_f32, |b, e| apply_value_effect(e, level, b));
        if alone > 0.0 {
            return true;
        }
        total = effects.iter().fold(total, |b, e| apply_value_effect(e, level, b));
    }
    total > 0.0
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: riptide, item use speed)
/// Whether the fork must refuse to start using this stack, and why. Checked before `UseItem` is
/// sent. Riptide is refused whether or not the bot is in water or rain, because the fork has no
/// weather state (`TridentItem#use` allows the spin only in water or rain). A trident whose spin
/// strength is 0 is not refused: the plain throw is decided by the server alone.
pub fn use_start_refusal(stack: &ItemStack, registries: &RegistryHolder) -> Option<InputRefusal> {
    let ItemStack::Present(_) = stack else { return None };
    if let Some(effects) = stack.get_component::<components::UseEffects>()
        && (effects.can_sprint || effects.speed_multiplier != 0.2)
    {
        return Some(InputRefusal::UseEffects);
    }
    if stack.kind() == ItemKind::Trident {
        let levels = stack
            .get_component::<components::Enchantments>()
            .map(|e| e.levels.clone())
            .unwrap_or_default();
        let spin = trident_spin_may_be_positive(&levels, |enchant| {
            let (_, data) = enchant.resolve(registries)?;
            Some(
                data.get::<TridentSpinAttackStrength>()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.0.clone())
                    .collect(),
            )
        });
        if spin {
            return Some(InputRefusal::Riptide);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use azalea_core::registry_holder::value::LevelBasedValue;
    use azalea_inventory::{ItemStackData, components::EquipmentSlot};
    use azalea_registry::Registry;

    use super::*;

    fn inputs() -> GlideInputs {
        GlideInputs {
            jump_now: true,
            jump_before: false,
            wears_glider: true,
            ..Default::default()
        }
    }

    fn elytra(damage: i32) -> ItemStack {
        ItemStack::from(ItemKind::Elytra).with_component(components::Damage { amount: damage })
    }

    #[test]
    fn a_fresh_jump_edge_in_the_air_with_a_glider_is_refused() {
        assert!(glide_would_start(inputs()));
    }

    #[test]
    fn a_held_jump_is_not_an_edge() {
        assert!(!glide_would_start(GlideInputs { jump_before: true, ..inputs() }));
        assert!(!glide_would_start(GlideInputs { jump_now: false, ..inputs() }));
    }

    #[test]
    fn each_vanilla_exception_lets_the_jump_through() {
        for i in [
            GlideInputs { on_ground: true, ..inputs() },
            GlideInputs { on_climbable: true, ..inputs() },
            GlideInputs { in_water: true, ..inputs() },
            GlideInputs { levitation: true, ..inputs() },
            GlideInputs { passenger: true, ..inputs() },
            GlideInputs { already_gliding: true, ..inputs() },
            GlideInputs { wears_glider: false, ..inputs() },
        ] {
            assert!(!glide_would_start(i), "{i:?}");
        }
    }

    #[test]
    fn a_glider_must_sit_in_its_own_slot_and_not_be_one_hit_from_broken() {
        // elytra: equippable chest, max damage 432
        assert!(can_glide_using(&elytra(0), EquipmentSlot::Chest));
        assert!(can_glide_using(&elytra(430), EquipmentSlot::Chest));
        // wrong slot
        assert!(!can_glide_using(&elytra(0), EquipmentSlot::Head));
        assert!(!can_glide_using(&elytra(0), EquipmentSlot::Mainhand));
        // the next point of damage would break it: vanilla does not glide with it
        assert!(!can_glide_using(&elytra(431), EquipmentSlot::Chest));
        assert!(next_damage_will_break(&elytra(431)));
        assert!(!next_damage_will_break(&elytra(430)));
        // not a glider
        let stone = ItemStack::Present(ItemStackData::new(ItemKind::Stone, 1));
        assert!(!can_glide_using(&stone, EquipmentSlot::Chest));
        assert!(!can_glide_using(&ItemStack::Empty, EquipmentSlot::Chest));
    }

    #[test]
    fn any_item_with_glider_and_a_matching_slot_glides() {
        // A non-elytra item given the two components, as a datapack or /give can.
        let mut eq = components::Equippable::new();
        eq.slot = EquipmentSlot::Chest;
        let stack = ItemStack::from(ItemKind::Stick)
            .with_component(components::Glider)
            .with_component(eq);
        assert!(can_glide_using(&stack, EquipmentSlot::Chest));
        assert!(!can_glide_using(&stack, EquipmentSlot::Legs));
    }

    fn spin(base: f32, per_level: f32) -> ValueEffect {
        ValueEffect::Add {
            value: LevelBasedValue::Linear { base, per_level_above_first: per_level },
        }
    }

    #[test]
    fn riptide_strength_follows_the_registry_effects() {
        let mut levels = HashMap::new();
        levels.insert(Enchantment::from_u32(1).unwrap(), 2);
        // vanilla Riptide: add linear(1.5, 0.75)
        assert!(trident_spin_may_be_positive(&levels, |_| Some(vec![spin(1.5, 0.75)])));
        // a datapack enchantment with the effect
        assert!(trident_spin_may_be_positive(&levels, |_| Some(vec![ValueEffect::Set {
            value: LevelBasedValue::Constant(0.5)
        }])));
        // an enchantment with no spin effect, and an effect that nets 0
        assert!(!trident_spin_may_be_positive(&levels, |_| Some(vec![])));
        assert!(!trident_spin_may_be_positive(&levels, |_| Some(vec![ValueEffect::Set {
            value: LevelBasedValue::Constant(0.0)
        }])));
        // unknown enchantment: refuse
        assert!(trident_spin_may_be_positive(&levels, |_| None));
        // no enchantments at all: the plain throw
        assert!(!trident_spin_may_be_positive(&HashMap::new(), |_| None));
    }

    #[test]
    fn use_effects_other_than_the_default_are_refused() {
        let reg = RegistryHolder::default();
        let plain = ItemStack::Present(ItemStackData::new(ItemKind::Apple, 1));
        assert_eq!(use_start_refusal(&plain, &reg), None);
        // a plain trident (strength 0): not refused
        let trident = ItemStack::Present(ItemStackData::new(ItemKind::Trident, 1));
        assert_eq!(use_start_refusal(&trident, &reg), None);
        // a spear: speed 1.0 and sprint
        let spear = ItemStack::Present(ItemStackData::new(ItemKind::IronSpear, 1));
        assert_eq!(use_start_refusal(&spear, &reg), Some(InputRefusal::UseEffects));
        // an item given a custom speed
        let fast = ItemStack::from(ItemKind::Apple).with_component(components::UseEffects {
            can_sprint: false,
            interact_vibrations: true,
            speed_multiplier: 1.0,
        });
        assert_eq!(use_start_refusal(&fast, &reg), Some(InputRefusal::UseEffects));
        // the explicit default is not a change
        let same = ItemStack::from(ItemKind::Apple).with_component(components::UseEffects::new());
        assert_eq!(use_start_refusal(&same, &reg), None);
    }

    #[test]
    fn a_trident_with_an_unresolvable_enchantment_is_refused() {
        let reg = RegistryHolder::default();
        let mut levels = HashMap::new();
        levels.insert(Enchantment::from_u32(0).unwrap(), 3);
        let stack = ItemStack::from(ItemKind::Trident).with_component(components::Enchantments { levels });
        assert_eq!(use_start_refusal(&stack, &reg), Some(InputRefusal::Riptide));
    }

    #[test]
    fn the_log_counts_refusals() {
        let mut log = RespawnLog::default();
        log.refuse_input(InputRefusal::ElytraStart);
        log.refuse_input(InputRefusal::ElytraStart);
        log.refuse_input(InputRefusal::Riptide);
        assert_eq!(
            log.refused_inputs,
            vec![(InputRefusal::ElytraStart, 2), (InputRefusal::Riptide, 1)]
        );
    }
}
