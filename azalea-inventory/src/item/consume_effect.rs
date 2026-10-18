use azalea_buf::AzBuf;
use azalea_core::sound::CustomSound;
use azalea_registry::{
    Holder, HolderSet,
    builtin::{ConsumeEffectKind, MobEffect, SoundEvent},
    identifier::Identifier,
};
use serde::Serialize;

use crate::components::MobEffectInstance;

// The variant names are the registry names, since vanilla writes the type with
// `byNameCodec`.
#[derive(AzBuf, Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ConsumeEffect {
    #[serde(rename = "minecraft:apply_effects")]
    ApplyEffects {
        effects: Vec<MobEffectInstance>,
        #[serde(skip_serializing_if = "is_default_probability")]
        probability: f32,
    },
    #[serde(rename = "minecraft:remove_effects")]
    RemoveEffects {
        effects: HolderSet<MobEffect, Identifier>,
    },
    #[serde(rename = "minecraft:clear_all_effects")]
    ClearAllEffects,
    #[serde(rename = "minecraft:teleport_randomly")]
    TeleportRandomly {
        #[serde(skip_serializing_if = "is_default_diameter")]
        diameter: f32,
    },
    #[serde(rename = "minecraft:play_sound")]
    PlaySound {
        sound: Holder<SoundEvent, CustomSound>,
    },
}
// Vanilla's codecs leave out a field equal to its default: probability 1,
// diameter 16.
fn is_default_probability(value: &f32) -> bool {
    *value == 1.
}
fn is_default_diameter(value: &f32) -> bool {
    *value == 16.
}

impl From<ConsumeEffect> for ConsumeEffectKind {
    fn from(effect: ConsumeEffect) -> Self {
        match effect {
            ConsumeEffect::ApplyEffects { .. } => ConsumeEffectKind::ApplyEffects,
            ConsumeEffect::RemoveEffects { .. } => ConsumeEffectKind::RemoveEffects,
            ConsumeEffect::ClearAllEffects => ConsumeEffectKind::ClearAllEffects,
            ConsumeEffect::TeleportRandomly { .. } => ConsumeEffectKind::TeleportRandomly,
            ConsumeEffect::PlaySound { .. } => ConsumeEffectKind::PlaySound,
        }
    }
}
