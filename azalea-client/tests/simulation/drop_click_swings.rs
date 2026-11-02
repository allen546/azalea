use azalea_client::{inventory::{ContainerClickEvent, OpenInventoryEvent}, test_utils::prelude::*};
use azalea_core::position::ChunkPos;
use azalea_entity::inventory::Inventory;
use azalea_inventory::{
    ItemStack,
    operations::{ClickOperation, PickupClick, ThrowClick},
};
use azalea_protocol::packets::{ConnectionProtocol, game::ServerboundGamePacket};
use azalea_registry::builtin::ItemKind;

// Spec: specs/B3-inventory-crafting.md §4.1 (patch 49: a drop click swings before the click)
/// The click, then the packets it sent that are a swing or a container click, in order.
fn click_packets(
    s: &mut Simulation,
    sent: &SentPackets,
    slot_item: Option<(usize, i32)>,
    carried: Option<i32>,
    op: ClickOperation,
) -> Vec<&'static str> {
    s.with_component_mut::<Inventory>(|mut inv| {
        for i in 9..45 {
            *inv.inventory_menu.slot_mut(i).unwrap() = ItemStack::Empty;
        }
        if let Some((i, n)) = slot_item {
            *inv.inventory_menu.slot_mut(i).unwrap() = ItemStack::new(ItemKind::Stone, n);
        }
        inv.carried = carried.map_or(ItemStack::Empty, |n| ItemStack::new(ItemKind::Stone, n));
    });
    sent.clear();
    let e = s.entity;
    s.trigger(ContainerClickEvent { entity: e, window_id: 0, operation: op });
    s.tick();
    let mut out = Vec::new();
    while let Some(p) = sent.next() {
        match p {
            ServerboundGamePacket::Swing(_) => out.push("Swing"),
            ServerboundGamePacket::ContainerClick(_) => out.push("ContainerClick"),
            _ => {}
        }
    }
    out
}

#[test]
fn a_drop_click_swings_before_the_container_click() {
    let _lock = init();
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.tick();
    let e = s.entity;
    s.trigger(OpenInventoryEvent { entity: e });
    s.tick();
    s.tick();

    let swing_then_click = vec!["Swing", "ContainerClick"];
    // Throw one, throw the stack, a click outside with the whole cursor, one item of it.
    assert_eq!(
        click_packets(&mut s, &sent, Some((36, 5)), None, ThrowClick::Single { slot: 36 }.into()),
        swing_then_click
    );
    assert_eq!(
        click_packets(&mut s, &sent, Some((36, 5)), None, ThrowClick::All { slot: 36 }.into()),
        swing_then_click
    );
    assert_eq!(
        click_packets(&mut s, &sent, None, Some(3), PickupClick::LeftOutside.into()),
        swing_then_click
    );
    assert_eq!(
        click_packets(&mut s, &sent, None, Some(3), PickupClick::RightOutside.into()),
        swing_then_click
    );
    // An empty stack drops nothing, so vanilla does not swing: the click goes alone.
    assert_eq!(
        click_packets(&mut s, &sent, None, None, ThrowClick::Single { slot: 36 }.into()),
        vec!["ContainerClick"]
    );
    assert_eq!(
        click_packets(&mut s, &sent, None, None, PickupClick::LeftOutside.into()),
        vec!["ContainerClick"]
    );
    // A throw with a stack on the cursor drops nothing either (`doClick` needs an empty cursor).
    assert_eq!(
        click_packets(&mut s, &sent, Some((36, 5)), Some(2), ThrowClick::Single { slot: 36 }.into()),
        vec!["ContainerClick"]
    );
    // A pickup in a slot is no drop.
    assert_eq!(
        click_packets(&mut s, &sent, Some((36, 5)), None, PickupClick::Left { slot: Some(36) }.into()),
        vec!["ContainerClick"]
    );
}
