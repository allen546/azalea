use azalea_client::test_utils::prelude::*;
use azalea_core::attribute_modifier_operation::AttributeModifierOperation;
use azalea_entity::{ActiveEffects, Attributes, MobEffectData};
use azalea_inventory::components::AttributeModifier;
use azalea_protocol::packets::{
    ConnectionProtocol,
    game::{ClientboundUpdateAttributes, c_update_attributes::AttributeSnapshot},
};
use azalea_registry::{
    builtin::{Attribute, MobEffect},
    data::DimensionKind,
    identifier::Identifier,
    DataRegistry,
};

// Spec: specs/B1-movement.md §4.2 (a respawn keeps what vanilla keeps)
// `ClientPacketListener#handleRespawn`: base values always, modifiers when keep bit 1 is set.

fn run(keep: u8) -> (f64, f64, bool) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.receive_packet(default_login_packet());
    s.tick();
    let id = s.minecraft_entity_id();
    let modifier = AttributeModifier {
        id: Identifier::new("test:half"),
        amount: -0.5,
        operation: AttributeModifierOperation::AddMultipliedTotal,
    };
    s.receive_packet(ClientboundUpdateAttributes {
        entity_id: id,
        values: vec![
            AttributeSnapshot { attribute: Attribute::MovementSpeed, base: 0.15, modifiers: vec![modifier] },
            AttributeSnapshot { attribute: Attribute::Scale, base: 1.0, modifiers: vec![] },
        ],
    });
    s.tick();
    s.with_component_mut::<ActiveEffects>(|mut fx| {
        fx.insert(MobEffect::Speed, MobEffectData::default());
    });
    let before = s.component::<Attributes>().movement_speed.calculate();
    assert!((before - 0.075).abs() < 1e-9, "{before}");

    let mut respawn = make_basic_respawn_packet(DimensionKind::new_raw(0), Identifier::new("minecraft:overworld"));
    respawn.data_to_keep = keep;
    s.receive_packet(respawn);
    s.tick();
    let a = s.component::<Attributes>();
    (a.movement_speed.base, a.movement_speed.calculate(), s.component::<ActiveEffects>().get(MobEffect::Speed).is_none())
}

#[test]
fn keep_0_keeps_the_base_value_and_drops_modifiers() {
    let _lock = init();
    let (base, value, effects_empty) = run(0);
    assert_eq!(base, 0.15);
    assert!((value - 0.15).abs() < 1e-9, "{value}");
    assert!(effects_empty);
}

#[test]
fn keep_1_keeps_the_modifiers_too() {
    let _lock = init();
    let (base, value, effects_empty) = run(1);
    assert_eq!(base, 0.15);
    assert!((value - 0.075).abs() < 1e-9, "{value}");
    assert!(effects_empty);
}
