use azalea_client::{
    ItemInUse,
    item_use::{ReleaseUseItemQueued, UseItemQueued},
    tick_clock::DroppedTicks,
    test_utils::prelude::*,
};
use azalea_entity::{EntityDataItem, EntityDataValue, EntityMetadataItems};
use azalea_inventory::ItemStack;
use azalea_protocol::packets::{
    ConnectionProtocol,
    game::{
        ClientboundContainerSetSlot, ClientboundSetEntityData, ClientboundSetHealth,
        ServerboundGamePacket, s_interact::InteractionHand, s_player_action::Action,
    },
};
use azalea_registry::builtin::ItemKind;

fn setup(food: u32) -> (Simulation, SentPackets) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.tick();
    s.receive_packet(ClientboundContainerSetSlot {
        container_id: 0,
        state_id: 1,
        slot: *azalea_inventory::Player::HOTBAR_SLOTS.start() as u16,
        item_stack: ItemStack::from(ItemKind::Bread),
    });
    s.receive_packet(ClientboundSetHealth { health: 20., food, saturation: 5. });
    s.tick();
    (s, sent)
}

fn using_flag(s: &mut Simulation, on: bool) {
    s.receive_packet(ClientboundSetEntityData {
        id: s.minecraft_entity_id(),
        packed_items: EntityMetadataItems(vec![EntityDataItem {
            index: 8,
            value: EntityDataValue::Byte(on as u8),
        }]),
    });
    s.tick();
}

fn insert(s: &mut Simulation, c: impl bevy_ecs::component::Component) {
    let e = s.entity;
    s.app.world_mut().entity_mut(e).insert(c);
}

fn count(sent: &SentPackets, f: impl Fn(&ServerboundGamePacket) -> bool) -> usize {
    sent.list.lock().iter().filter(|p| f(p)).count()
}

#[test]
fn hungry_player_eats_until_the_server_clears_the_flag() {
    let _lock = init();
    let (mut s, sent) = setup(10);
    sent.clear();
    s.with_component_mut::<azalea_entity::LookDirection>(|l| l.update_x_rot(90.));
    s.tick();

    insert(&mut s, UseItemQueued { hand: InteractionHand::MainHand });
    s.tick();
    // UseItem goes out even though the crosshair is on the floor.
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::UseItem(u) if u.hand == InteractionHand::MainHand)), 1);
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::UseItemOn(_))), 0);
    let used = s.component::<ItemInUse>();
    assert_eq!(used.hand, InteractionHand::MainHand);
    assert_eq!(used.ticks, 1);

    using_flag(&mut s, true);
    for _ in 0..5 {
        s.tick();
    }
    assert!(s.component::<ItemInUse>().confirmed);
    assert_eq!(s.component::<ItemInUse>().ticks, 7);

    using_flag(&mut s, false);
    assert!(!s.has_component::<ItemInUse>());
}

// Patch 39: client ticks dropped after the flag arrived are server ticks of the use; dropped
// ticks before the flag are not.
#[test]
fn dropped_client_ticks_count_as_server_ticks_of_the_use() {
    let _lock = init();
    let (mut s, _sent) = setup(10);
    insert(&mut s, UseItemQueued { hand: InteractionHand::MainHand });
    s.tick();
    assert_eq!(s.component::<ItemInUse>().server_ticks(), None);
    s.app.world_mut().insert_resource(DroppedTicks(4));
    using_flag(&mut s, true);
    // The 4 ticks dropped before the flag do not count; the tick that sees the flag counts as 1.
    assert_eq!(s.component::<ItemInUse>().server_ticks(), Some(1));
    for _ in 0..3 {
        s.tick();
    }
    assert_eq!(s.component::<ItemInUse>().server_ticks(), Some(4));
    // A stall: 5 ticks were due and not run.
    s.app.world_mut().resource_mut::<DroppedTicks>().0 += 5;
    s.tick();
    assert_eq!(s.component::<ItemInUse>().server_ticks(), Some(5 + 5));
    s.tick();
    assert_eq!(s.component::<ItemInUse>().server_ticks(), Some(6 + 5));
}

#[test]
fn full_player_does_not_start_eating_and_release_sends_one_packet() {
    let _lock = init();
    let (mut s, sent) = setup(20);
    sent.clear();
    insert(&mut s, UseItemQueued { hand: InteractionHand::MainHand });
    s.tick();
    assert!(!s.has_component::<ItemInUse>());
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::UseItem(_))), 1);

    let (mut s, sent) = setup(10);
    insert(&mut s, UseItemQueued { hand: InteractionHand::MainHand });
    s.tick();
    sent.clear();
    insert(&mut s, ReleaseUseItemQueued);
    s.tick();
    assert!(!s.has_component::<ItemInUse>());
    assert_eq!(
        count(&sent, |p| matches!(p, ServerboundGamePacket::PlayerAction(a) if a.action == Action::ReleaseUseItem)),
        1
    );
    // Nothing to release now: no packet.
    sent.clear();
    insert(&mut s, ReleaseUseItemQueued);
    s.tick();
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::PlayerAction(_))), 0);
}
