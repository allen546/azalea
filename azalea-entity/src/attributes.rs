//! Attributes and modifiers for entities.
//!
//! Also see <https://minecraft.wiki/w/Attribute>.

use std::collections::{HashMap, hash_map};

use azalea_core::attribute_modifier_operation::AttributeModifierOperation;
use azalea_inventory::components::AttributeModifier;
use azalea_registry::{builtin::Attribute, identifier::Identifier};
use thiserror::Error;

/// The current attribute values for an entity.
///
/// Each attribute can have multiple modifiers, and these modifiers are the
/// result of things like sprinting or enchantments.
#[cfg_attr(feature = "bevy_ecs", derive(bevy_ecs::component::Component))]
#[derive(Clone, Debug)]
pub struct Attributes {
    pub movement_speed: AttributeInstance,
    pub sneaking_speed: AttributeInstance,
    pub attack_speed: AttributeInstance,
    pub water_movement_efficiency: AttributeInstance,
    pub mining_efficiency: AttributeInstance,
    pub block_break_speed: AttributeInstance,

    pub block_interaction_range: AttributeInstance,
    pub entity_interaction_range: AttributeInstance,

    pub step_height: AttributeInstance,

    /// `Attributes.JUMP_STRENGTH`: the upward speed of a jump.
    pub jump_strength: AttributeInstance,
    /// `Attributes.GRAVITY`: blocks per tick squared pulling the entity down.
    pub gravity: AttributeInstance,
    /// `Attributes.MOVEMENT_EFFICIENCY`: how much of a slowing block's speed
    /// factor is ignored.
    pub movement_efficiency: AttributeInstance,
    /// `Attributes.SUBMERGED_MINING_SPEED`: the mining multiplier with the eyes
    /// in water.
    pub submerged_mining_speed: AttributeInstance,

    /// Every other attribute the server sent, kept so that nothing is dropped.
    /// Most of them only matter to the server; see [`Self::unmodelled_in_force`]
    /// for the ones that change what a client computes.
    pub other: Vec<(Attribute, AttributeInstance)>,
}

/// Attributes that change what the client computes for itself but that the
/// fork does not model, with the vanilla default. While one is away from its
/// default, a prediction would be wrong, so Null refuses to move or break.
///
/// `Attributes.SCALE` changes the hitbox, the eye height and with them every
/// collision, reach and ray check.
const UNMODELLED_CLIENT_ATTRIBUTES: [(Attribute, f64); 1] = [(Attribute::Scale, 1.0)];

impl Attributes {
    /// Returns a mutable reference to the [`AttributeInstance`] for the given
    /// attribute, or `None` if the attribute isn't implemented.
    pub fn get_mut(&mut self, attribute: Attribute) -> Option<&mut AttributeInstance> {
        let value = match attribute {
            Attribute::MovementSpeed => &mut self.movement_speed,
            Attribute::SneakingSpeed => &mut self.sneaking_speed,
            Attribute::AttackSpeed => &mut self.attack_speed,
            Attribute::WaterMovementEfficiency => &mut self.water_movement_efficiency,
            Attribute::MiningEfficiency => &mut self.mining_efficiency,
            Attribute::BlockInteractionRange => &mut self.block_interaction_range,
            Attribute::EntityInteractionRange => &mut self.entity_interaction_range,
            Attribute::StepHeight => &mut self.step_height,
            Attribute::BlockBreakSpeed => &mut self.block_break_speed,
            Attribute::JumpStrength => &mut self.jump_strength,
            Attribute::Gravity => &mut self.gravity,
            Attribute::MovementEfficiency => &mut self.movement_efficiency,
            Attribute::SubmergedMiningSpeed => &mut self.submerged_mining_speed,
            _ => return None,
        };
        Some(value)
    }

    /// Apply one entry of `ClientboundUpdateAttributesPacket` like
    /// `ClientPacketListener#handleUpdateAttributes`: take the server's base
    /// value and replace every modifier (the sprinting one included; the
    /// server lists it while the player sprints).
    // Spec: specs/B1-movement.md §4.2
    pub fn apply_server_snapshot(
        &mut self,
        attribute: Attribute,
        base: f64,
        modifiers: &[AttributeModifier],
    ) {
        let instance = match self.get_mut(attribute) {
            Some(instance) => instance,
            None => {
                let index = match self.other.iter().position(|(a, _)| *a == attribute) {
                    Some(index) => index,
                    None => {
                        self.other.push((attribute, AttributeInstance::new(base)));
                        self.other.len() - 1
                    }
                };
                &mut self.other[index].1
            }
        };
        instance.base = base;
        instance.clear_modifiers();
        for modifier in modifiers {
            instance.insert(modifier.clone());
        }
    }

    /// The attributes in force that change what the client computes but that
    /// the fork does not model, with their current values. Empty when every
    /// such attribute has its default.
    // Spec: specs/B1-movement.md §4.2
    pub fn unmodelled_in_force(&self) -> Vec<(Attribute, f64)> {
        UNMODELLED_CLIENT_ATTRIBUTES
            .iter()
            .filter_map(|(attribute, default)| {
                let (_, instance) = self.other.iter().find(|(a, _)| a == attribute)?;
                let value = instance.calculate();
                // the server sends the whole list, so "default" means equal after the f32 round
                ((value - default).abs() > 1e-9).then_some((*attribute, value))
            })
            .collect()
    }
}

/// An individual attribute for an entity, which may have any number of
/// modifiers attached to it.
#[derive(Clone, Debug)]
pub struct AttributeInstance {
    pub base: f64,
    modifiers_by_id: HashMap<Identifier, AttributeModifier>,
    /// `RangedAttribute` limits; the value is clamped into them.
    min: f64,
    max: f64,
    // TODO: add cache
}

/// An error for when we try to call [`AttributeInstance::try_insert`] when the
/// modifier is already present.
#[derive(Clone, Debug, Error)]
#[error("A modifier with this ID is already present.")]
pub struct AlreadyPresentError;

impl AttributeInstance {
    pub fn new(base: f64) -> Self {
        Self::ranged(base, f64::NEG_INFINITY, f64::INFINITY)
    }

    /// An attribute whose value is clamped into `min..=max`, like
    /// `RangedAttribute#sanitizeValue`.
    pub fn ranged(base: f64, min: f64, max: f64) -> Self {
        Self {
            base,
            modifiers_by_id: HashMap::new(),
            min,
            max,
        }
    }

    /// `AttributeInstance#removeModifiers`.
    pub fn clear_modifiers(&mut self) {
        self.modifiers_by_id.clear();
    }

    /// In Mojmap, this is `AttributeInstance#calculateValue`: all `AddValue`
    /// modifiers first, then `AddMultipliedBase` (of that sum), then
    /// `AddMultipliedTotal`, whatever order the modifiers were added in. The
    /// result is clamped into the attribute's range (a NaN becomes the minimum).
    // Spec: specs/B1-movement.md §4.2
    pub fn calculate(&self) -> f64 {
        let mut with_adds = self.base;
        for modifier in self.modifiers_by_id.values() {
            if let AttributeModifierOperation::AddValue = modifier.operation {
                with_adds += modifier.amount;
            }
        }
        let mut total = with_adds;
        for modifier in self.modifiers_by_id.values() {
            if let AttributeModifierOperation::AddMultipliedBase = modifier.operation {
                total += with_adds * modifier.amount;
            }
        }
        for modifier in self.modifiers_by_id.values() {
            if let AttributeModifierOperation::AddMultipliedTotal = modifier.operation {
                total *= 1. + modifier.amount;
            }
        }
        if total.is_nan() {
            self.min
        } else {
            total.clamp(self.min, self.max)
        }
    }

    /// Add a new modifier to this attribute and return the previous value, if
    /// present.
    pub fn insert(&mut self, modifier: AttributeModifier) -> Option<AttributeModifier> {
        self.modifiers_by_id.insert(modifier.id.clone(), modifier)
    }

    /// Insert the given modifier if it's not already present, otherwise returns
    /// [`AlreadyPresentError`].
    pub fn try_insert(&mut self, modifier: AttributeModifier) -> Result<(), AlreadyPresentError> {
        match self.modifiers_by_id.entry(modifier.id.clone()) {
            hash_map::Entry::Occupied(_) => Err(AlreadyPresentError),
            hash_map::Entry::Vacant(entry) => {
                entry.insert(modifier);
                Ok(())
            }
        }
    }

    /// Remove the modifier with the given ID from this attribute, returning
    /// the previous modifier is present.
    pub fn remove(&mut self, id: &Identifier) -> Option<AttributeModifier> {
        self.modifiers_by_id.remove(id)
    }
}

pub fn sprinting_modifier() -> AttributeModifier {
    AttributeModifier {
        id: Identifier::new("sprinting"),
        amount: 0.3f32 as f64,
        operation: AttributeModifierOperation::AddMultipliedTotal,
    }
}
pub fn base_attack_speed_modifier(amount: f64) -> AttributeModifier {
    AttributeModifier {
        id: Identifier::new("base_attack_speed"),
        amount,
        operation: AttributeModifierOperation::AddValue,
    }
}
pub fn creative_block_interaction_range_modifier() -> AttributeModifier {
    AttributeModifier {
        id: Identifier::new("creative_mode_block_range"),
        amount: 0.5,
        operation: AttributeModifierOperation::AddValue,
    }
}

pub fn creative_entity_interaction_range_modifier() -> AttributeModifier {
    AttributeModifier {
        id: Identifier::new("creative_mode_entity_range"),
        amount: 2.0,
        operation: AttributeModifierOperation::AddValue,
    }
}

#[cfg(test)]
mod tests {
    use azalea_inventory::components::AttributeModifier;

    use super::*;

    // `AttributeInstance#calculateValue` adds the flat modifiers first and then
    // multiplies, whatever order they were inserted in (patch 26).
    #[test]
    fn test_modifier_operations_apply_in_vanilla_order() {
        for sprint_first in [true, false] {
            let mut speed = AttributeInstance::new(0.1);
            let frost = AttributeModifier {
                id: Identifier::new("powder_snow"),
                amount: -0.0136,
                operation: AttributeModifierOperation::AddValue,
            };
            if sprint_first {
                speed.insert(sprinting_modifier());
                speed.insert(frost);
            } else {
                speed.insert(frost);
                speed.insert(sprinting_modifier());
            }
            let want = (0.1 - 0.0136) * (1. + 0.3f32 as f64);
            assert!((speed.calculate() - want).abs() < 1e-12, "{}", speed.calculate());
        }
    }

    fn modifier(id: &str, amount: f64, operation: AttributeModifierOperation) -> AttributeModifier {
        AttributeModifier { id: Identifier::new(id), amount, operation }
    }

    // `ClientPacketListener#handleUpdateAttributes`: the base and the whole modifier list
    // are the server's. A modifier the client had and the packet lacks (the sprinting one)
    // is gone afterwards.
    #[test]
    fn server_snapshot_replaces_base_and_all_modifiers() {
        let mut a = Attributes::new(azalea_registry::builtin::EntityKind::Player);
        a.movement_speed.insert(sprinting_modifier());
        a.apply_server_snapshot(
            Attribute::MovementSpeed,
            0.2,
            &[modifier("test", 0.5, AttributeModifierOperation::AddMultipliedTotal)],
        );
        assert!((a.movement_speed.calculate() - 0.2 * 1.5).abs() < 1e-12);
        a.apply_server_snapshot(Attribute::MovementSpeed, 0.1f32 as f64, &[]);
        assert!((a.movement_speed.calculate() - 0.1f32 as f64).abs() < 1e-12);
    }

    // `AttributeInstance#calculateValue` clamps with the `RangedAttribute` limits.
    #[test]
    fn value_is_clamped_to_the_attribute_range() {
        let mut a = Attributes::new(azalea_registry::builtin::EntityKind::Player);
        a.apply_server_snapshot(
            Attribute::MovementSpeed,
            0.1,
            &[modifier("slow", -5.0, AttributeModifierOperation::AddValue)],
        );
        assert_eq!(a.movement_speed.calculate(), 0.0);
        a.apply_server_snapshot(Attribute::Gravity, 5.0, &[]);
        assert_eq!(a.gravity.calculate(), 1.0);
        a.apply_server_snapshot(Attribute::StepHeight, 50.0, &[]);
        assert_eq!(a.step_height.calculate(), 10.0);
    }

    // Scale (`Attributes.SCALE`, default 1) is not modelled: away from the default it is
    // listed. Attributes only the server uses never are.
    #[test]
    fn unmodelled_client_attributes_are_listed_only_off_the_default() {
        let mut a = Attributes::new(azalea_registry::builtin::EntityKind::Player);
        assert!(a.unmodelled_in_force().is_empty());
        a.apply_server_snapshot(Attribute::Scale, 1.0, &[]);
        a.apply_server_snapshot(Attribute::MaxHealth, 40.0, &[]);
        a.apply_server_snapshot(Attribute::Armor, 20.0, &[]);
        assert!(a.unmodelled_in_force().is_empty());
        a.apply_server_snapshot(
            Attribute::Scale,
            1.0,
            &[modifier("big", 0.5, AttributeModifierOperation::AddMultipliedTotal)],
        );
        assert_eq!(a.unmodelled_in_force(), vec![(Attribute::Scale, 1.5)]);
        a.apply_server_snapshot(Attribute::Scale, 1.0, &[]);
        assert!(a.unmodelled_in_force().is_empty());
    }
}
