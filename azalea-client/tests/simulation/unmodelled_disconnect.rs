use azalea_client::{
    packet::game::SendGamePacketEvent,
    test_utils::prelude::*,
    unmodelled_disconnect::{
        DisconnectOnUnmodelledMovement, PacketsAfterUnmodelledDisconnect, UnmodelledDisconnect,
    },
};
use azalea_core::{
    position::{BlockPos, ChunkPos, Vec3},
    entity_id::MinecraftEntityId,
};
use azalea_entity::{ActiveEffects, LookDirection, MobEffectData, unmodelled::UnmodelledInput};
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{
            ClientboundBlockUpdate, ClientboundInitializeBorder, ClientboundPlayerPosition, ClientboundSetBorderSize, ClientboundSetPassengers,
            ClientboundUpdateAttributes, ServerboundClientTickEnd,
            c_update_attributes::AttributeSnapshot,
        },
    },
};
use azalea_registry::builtin::{Attribute, BlockKind, MobEffect};
use azalea_world::WorldName;

// Spec: specs/F5-version-layer.md §4.6 (the disconnect for a Movement entry)

fn setup(with_gate: bool) -> (Simulation, SentPackets) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.tick();
    let floor = BlockPos::new(0, 2, 0);
    s.receive_packet(ClientboundBlockUpdate { pos: floor, block_state: BlockKind::Stone.into() });
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: floor.up(1).center_bottom(),
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    s.tick();
    s.tick();
    if with_gate {
        let e = s.entity;
        s.app.world_mut().entity_mut(e).insert(DisconnectOnUnmodelledMovement);
    }
    (s, sent)
}

fn set_attribute(s: &mut Simulation, attribute: Attribute, base: f64) {
    let id = s.minecraft_entity_id();
    s.receive_packet(ClientboundUpdateAttributes {
        entity_id: id,
        values: vec![AttributeSnapshot { attribute, base, modifiers: vec![] }],
    });
}

fn assert_disconnected_silently(s: &mut Simulation, sent: &SentPackets) {
    s.update();
    assert!(s.has_component::<UnmodelledDisconnect>(), "no record");
    assert!(!s.has_component::<WorldName>(), "still connected");
    assert!(sent.list.lock().is_empty(), "sent after the trigger: {:?}", sent.list.lock());
    for _ in 0..5 {
        s.tick();
    }
    assert!(sent.list.lock().is_empty(), "sent later: {:?}", sent.list.lock());
    assert_eq!(s.component::<PacketsAfterUnmodelledDisconnect>(), PacketsAfterUnmodelledDisconnect(0));
}

#[test]
fn scale_disconnects_before_the_next_tick_sends_anything() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    assert!(sent.list.lock().iter().any(|p| matches!(p, azalea_protocol::packets::game::ServerboundGamePacket::ClientTickEnd(_))));
    sent.clear();
    set_attribute(&mut s, Attribute::Scale, 2.0);
    s.tick();
    assert_disconnected_silently(&mut s, &sent);
    let rec = s.component::<UnmodelledDisconnect>();
    assert_eq!(rec.inputs, vec![UnmodelledInput::Attribute(Attribute::Scale, 2.0)]);
}

#[test]
fn no_gate_component_no_disconnect() {
    let _lock = init();
    let (mut s, _sent) = setup(false);
    set_attribute(&mut s, Attribute::Scale, 2.0);
    s.tick();
    s.tick();
    assert!(!s.has_component::<UnmodelledDisconnect>());
    assert!(s.has_component::<WorldName>());
}

#[test]
fn levitation_and_slow_falling_stay_connected() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    s.with_component_mut::<ActiveEffects>(|mut fx| {
        fx.insert(MobEffect::Levitation, MobEffectData { amplifier: 1, duration: 0, ..Default::default() });
        fx.insert(MobEffect::SlowFalling, MobEffectData { amplifier: 0, duration: 0, ..Default::default() });
    });
    sent.clear();
    for _ in 0..5 {
        s.tick();
    }
    assert!(!s.has_component::<UnmodelledDisconnect>());
    assert!(sent.list.lock().iter().any(|p| matches!(p, azalea_protocol::packets::game::ServerboundGamePacket::ClientTickEnd(_))));
    // still in force at duration 0: only the server removes an effect
    assert!(s.component::<ActiveEffects>().get(MobEffect::Levitation).is_some());
}

#[test]
fn dolphins_grace_disconnects() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    sent.clear();
    s.with_component_mut::<ActiveEffects>(|mut fx| {
        fx.insert(MobEffect::DolphinsGrace, MobEffectData::default());
    });
    s.tick();
    assert_disconnected_silently(&mut s, &sent);
}

#[test]
fn riding_a_vehicle_disconnects() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    sent.clear();
    let me = s.minecraft_entity_id();
    s.receive_packet(ClientboundSetPassengers { vehicle: MinecraftEntityId(77), passengers: vec![me] });
    s.tick();
    assert_disconnected_silently(&mut s, &sent);
    assert_eq!(s.component::<UnmodelledDisconnect>().inputs, vec![UnmodelledInput::Passenger]);
}

#[test]
fn touching_a_bubble_column_disconnects() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    sent.clear();
    s.receive_packet(ClientboundBlockUpdate { pos: BlockPos::new(0, 3, 0), block_state: BlockKind::BubbleColumn.into() });
    s.tick();
    assert_disconnected_silently(&mut s, &sent);
    assert!(matches!(
        s.component::<UnmodelledDisconnect>().inputs[0],
        UnmodelledInput::Block(_, BlockKind::BubbleColumn)
    ));
}

#[test]
fn sends_after_the_trigger_are_counted_and_dropped() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    sent.clear();
    set_attribute(&mut s, Attribute::Scale, 0.5);
    s.tick();
    let _ = &sent;
    let e = s.entity;
    s.trigger(SendGamePacketEvent::new(e, ServerboundClientTickEnd));
    assert_eq!(s.component::<PacketsAfterUnmodelledDisconnect>(), PacketsAfterUnmodelledDisconnect(1));
}

#[test]
fn the_world_border_edge_disconnects() {
    let _lock = init();
    let (mut s, sent) = setup(true);
    s.receive_packet(ClientboundInitializeBorder {
        new_center_x: 0.0,
        new_center_z: 0.0,
        old_size: 10.0,
        new_size: 10.0,
        lerp_time: 0,
        new_absolute_max_size: 29999984,
        warning_blocks: 5,
        warning_time: 15,
    });
    s.tick();
    s.tick();
    assert!(!s.has_component::<UnmodelledDisconnect>(), "inside the border");
    sent.clear();
    // the bot stands at (0.5, 3, 0.5): a border 1 wide has its edge at 0.5
    s.receive_packet(ClientboundSetBorderSize { size: 1.0 });
    s.tick();
    assert_disconnected_silently(&mut s, &sent);
    assert_eq!(s.component::<UnmodelledDisconnect>().inputs, vec![UnmodelledInput::WorldBorder]);
}

// A packet for the vehicle that no longer lists the bot ends the ride (`handleSetEntityPassengers`).
#[test]
fn dismounting_clears_the_vehicle() {
    let _lock = init();
    let (mut s, _sent) = setup(false);
    let me = s.minecraft_entity_id();
    s.receive_packet(ClientboundSetPassengers { vehicle: MinecraftEntityId(77), passengers: vec![me] });
    s.tick();
    assert!(s.has_component::<azalea_entity::unmodelled::RidingVehicle>());
    s.receive_packet(ClientboundSetPassengers { vehicle: MinecraftEntityId(77), passengers: vec![] });
    s.tick();
    assert!(!s.has_component::<azalea_entity::unmodelled::RidingVehicle>());
}
