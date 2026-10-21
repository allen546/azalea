use azalea_client::{
    ClientMovementState, WalkDirection,
    block_actions::{BlockActionOutcome, BlockActionRefusal, BlockActionResults, LookAtBlockQueued, PlaceBlockQueued},
    test_utils::prelude::*,
};
use azalea_block::BlockState;
use azalea_core::{
    direction::Direction,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_entity::{LookDirection, Position};
use azalea_inventory::ItemStack;
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{
            ClientboundBlockChangedAck, ClientboundBlockUpdate, ClientboundContainerSetSlot,
            ClientboundPlayerPosition, ServerboundGamePacket,
        },
    },
};
use azalea_registry::builtin::{BlockKind, ItemKind};
use bevy_ecs::prelude::*;

const FLOOR_Y: i32 = 119;

/// A bot at (0.5, 120, 0.5) holding dirt, on a stone floor of the given cells.
fn setup(floor: &[BlockPos], bot: Vec3) -> (Simulation, SentPackets) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.tick();
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.receive_packet(ClientboundContainerSetSlot {
        container_id: 0,
        state_id: 1,
        slot: *azalea_inventory::Player::HOTBAR_SLOTS.start() as u16,
        item_stack: ItemStack::from(ItemKind::Dirt),
    });
    for p in floor {
        s.receive_packet(ClientboundBlockUpdate { pos: *p, block_state: BlockKind::Stone.into() });
    }
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: bot,
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    for _ in 0..4 {
        s.tick();
    }
    sent.clear();
    (s, sent)
}

fn insert(s: &mut Simulation, c: impl Component) {
    let e = s.entity;
    s.app.world_mut().entity_mut(e).insert(c);
}

fn look_at(s: &mut Simulation, position: BlockPos, direction: Direction) {
    insert(s, LookAtBlockQueued { position, direction: Some(direction) });
    s.tick();
    s.tick();
}

fn used_item_on(sent: &SentPackets) -> usize {
    sent.list.lock().iter().filter(|p| matches!(p, ServerboundGamePacket::UseItemOn(_))).count()
}

// Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
#[test]
fn a_placed_block_is_written_at_once_and_follows_the_server_after_the_ack() {
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    let target = floor.up(1);

    // the server refuses: no update, only the ack
    look_at(&mut s, floor, Direction::Up);
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    assert_eq!(used_item_on(&sent), 1);
    assert_eq!(s.get_block_state(target), Some(BlockKind::Dirt.into()), "written in the tick of the click");
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    assert_eq!(s.get_block_state(target), Some(BlockState::AIR), "rolled back to the server's state");
    let r = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!(r.outcome, BlockActionOutcome::Refused);
    assert_eq!(r.refusal, None);

    // the server accepts: its update comes first, then the ack; the block stays
    look_at(&mut s, floor, Direction::Up);
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    assert_eq!(s.get_block_state(target), Some(BlockKind::Dirt.into()));
    s.receive_packet(ClientboundBlockUpdate { pos: target, block_state: BlockKind::Dirt.into() });
    s.receive_packet(ClientboundBlockChangedAck { seq: 2 });
    s.tick();
    assert_eq!(s.get_block_state(target), Some(BlockKind::Dirt.into()));
    assert_eq!(s.component::<BlockActionResults>().last_place.unwrap().outcome, BlockActionOutcome::Accepted);
}

// Spec: specs/B6-placement-breaking.md §4.10 (the safety rule: inside the zone, predicted or refused)
#[test]
fn inside_the_movement_zone_a_place_that_cannot_be_predicted_is_refused_and_nothing_is_sent() {
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    // a fence is outside the stage 1 families
    s.receive_packet(ClientboundContainerSetSlot {
        container_id: 0,
        state_id: 2,
        slot: *azalea_inventory::Player::HOTBAR_SLOTS.start() as u16,
        item_stack: ItemStack::from(ItemKind::OakFence),
    });
    s.tick();
    look_at(&mut s, floor, Direction::Up);
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    assert_eq!(used_item_on(&sent), 0);
    assert_eq!(s.get_block_state(floor.up(1)), Some(BlockState::AIR));
    let r = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!((r.outcome, r.refusal), (BlockActionOutcome::Refused, Some(BlockActionRefusal::PlaceNotPredicted)));
    assert!(r.evidence.is_some(), "a refusal carries its evidence (patch 28)");
}

// Spec: specs/B6-placement-breaking.md §4.10 (a bridge: walk onto each block as soon as it is placed)
#[test]
fn a_sneaking_bot_walks_onto_the_block_it_just_placed() {
    let _lock = init();
    let support = BlockPos::new(0, FLOOR_Y, 6);
    let (mut s, _sent) = setup(&[support, BlockPos::new(0, FLOOR_Y, 7)], Vec3::new(0.5, 120., 6.5));
    s.with_component_mut::<ClientMovementState>(|m| {
        m.trying_to_crouch = true;
        m.move_direction = WalkDirection::Backward;
    });
    // out over the edge: walk back until the eye is past the support's north plane, stop, let the
    // glide end (an aim made while gliding misses: the face is a hand's breadth from the eye),
    // then aim
    let mut n = 0;
    while s.component::<Position>().z >= 6.0 - 0.08 {
        s.tick();
        n += 1;
        assert!(n < 100, "sneaking backward never got out over the edge");
    }
    s.with_component_mut::<ClientMovementState>(|m| m.move_direction = WalkDirection::None);
    for _ in 0..6 {
        s.tick();
    }
    look_at(&mut s, support, Direction::North);
    let z = s.component::<Position>().z;
    assert!(z < 5.95 && z > 5.69, "sneaking holds the bot on the edge block: z {z}");
    insert(&mut s, PlaceBlockQueued { position: support, direction: Direction::North, auto_crouch: false });
    s.tick();
    let r = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!((r.outcome, r.refusal), (BlockActionOutcome::Pending, None), "{r:?}");
    assert_eq!(s.get_block_state(support.north(1)), Some(BlockKind::Dirt.into()));
    // no server update yet: the bot still walks on, onto the block it wrote
    s.with_component_mut::<ClientMovementState>(|m| m.move_direction = WalkDirection::Backward);
    for _ in 0..40 {
        s.tick();
    }
    let p = s.component::<Position>();
    assert!(p.z < 4.9 && p.y == 120.0, "the bot stands on the predicted block: {:?}", *p);
}
