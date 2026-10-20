//! Server-set inputs of the client's own computations that the fork does not model.
//!
//! A bot whose attribute or status effect changes what a vanilla client computes (hitbox,
//! gravity, speed in water, ...) must not move or break on a wrong prediction. This lists the
//! ones in force so the layer above can stop. Spec: specs/F5-version-layer.md §4.6.

use azalea_registry::builtin::{Attribute, MobEffect};

use crate::{ActiveEffects, Attributes};

/// What an unmodelled input changes. `Movement` changes the bot's motion even with no input, so
/// stopping the skill is not enough (the client keeps simulating and sending positions); what to
/// do then is a decision above the fork. `Action` changes only what an action computes (reach,
/// break time).
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
}

impl UnmodelledInput {
    /// Scale (hitbox), levitation, slow falling and dolphin's grace all move the bot or its box
    /// every tick.
    pub fn kind(&self) -> UnmodelledKind {
        UnmodelledKind::Movement
    }
}

/// Status effects the client reads for its own movement that the fork does not model yet:
/// `LivingEntity#travelInAir` (levitation), `#getEffectiveGravity` (slow falling) and
/// `#travelInWater` (dolphin's grace). Blindness (`Player#isMobilityRestricted`, sprinting),
/// jump boost, haste, conduit power, mining fatigue, weaving, speed and slowness are modelled.
const UNMODELLED_EFFECTS: [MobEffect; 3] = [
    MobEffect::Levitation,
    MobEffect::SlowFalling,
    MobEffect::DolphinsGrace,
];

/// Every unmodelled attribute or effect in force; empty when the client's predictions hold.
// Spec: specs/F5-version-layer.md §4.6
pub fn unmodelled_inputs(attributes: &Attributes, effects: &ActiveEffects) -> Vec<UnmodelledInput> {
    let mut out: Vec<UnmodelledInput> = attributes
        .unmodelled_in_force()
        .into_iter()
        .map(|(a, v)| UnmodelledInput::Attribute(a, v))
        .collect();
    for effect in UNMODELLED_EFFECTS {
        if let Some(level) = effects.get_level(effect) {
            out.push(UnmodelledInput::Effect(effect, level));
        }
    }
    out
}

#[cfg(test)]
mod tests {
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
}
