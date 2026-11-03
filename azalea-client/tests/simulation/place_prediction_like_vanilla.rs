use azalea_client::{
    ClientMovementState, WalkDirection,
    block_actions::{ACK_TIMEOUT_TICKS, BlockActionOutcome, BlockActionRefusal, BlockActionResults, BreakBlockQueued, LookAtBlockQueued, PlaceBlockQueued},
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
use azalea_registry::{DataRegistry, data::DimensionKind, identifier::Identifier};
use azalea_registry::builtin::{BlockKind, EntityKind, ItemKind};
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
    assert_eq!(s.bot_block_state(target), Some(BlockKind::Dirt.into()), "written in the tick of the click");
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    assert_eq!(s.bot_block_state(target), Some(BlockState::AIR), "rolled back to the server's state");
    let r = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!(r.outcome, BlockActionOutcome::Refused);
    assert_eq!(r.refusal, None);

    // the server accepts: its update comes first, then the ack; the block stays
    look_at(&mut s, floor, Direction::Up);
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    assert_eq!(s.bot_block_state(target), Some(BlockKind::Dirt.into()));
    s.receive_packet(ClientboundBlockUpdate { pos: target, block_state: BlockKind::Dirt.into() });
    s.receive_packet(ClientboundBlockChangedAck { seq: 2 });
    s.tick();
    assert_eq!(s.bot_block_state(target), Some(BlockKind::Dirt.into()));
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
    assert_eq!(s.bot_block_state(floor.up(1)), Some(BlockState::AIR));
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
    assert_eq!(s.bot_block_state(support.north(1)), Some(BlockKind::Dirt.into()));
    // no server update yet: the bot still walks on, onto the block it wrote
    s.with_component_mut::<ClientMovementState>(|m| m.move_direction = WalkDirection::Backward);
    for _ in 0..40 {
        s.tick();
    }
    let p = s.component::<Position>();
    assert!(p.z < 4.9 && p.y == 120.0, "the bot stands on the predicted block: {:?}", *p);
}

fn swings(sent: &SentPackets) -> usize {
    sent.list.lock().iter().filter(|p| matches!(p, ServerboundGamePacket::Swing(_))).count()
}

fn hold(s: &mut Simulation, item: ItemKind, state_id: u32) {
    s.receive_packet(ClientboundContainerSetSlot {
        container_id: 0,
        state_id,
        slot: *azalea_inventory::Player::HOTBAR_SLOTS.start() as u16,
        item_stack: ItemStack::from(item),
    });
    s.tick();
}

/// Clicks the top of the block at x = 3, outside the movement zone of a bot at x = 0.5, so a place
/// that stage 1 cannot compute is sent, not refused.
fn place_far(s: &mut Simulation) {
    let floor = BlockPos::new(3, FLOOR_Y, 0);
    look_at(s, floor, Direction::Up);
    insert(s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 48: `Minecraft#startUseItem` swings on a client success)
#[test]
fn a_place_swings_when_the_client_result_is_a_success() {
    let _lock = init();
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), BlockPos::new(3, FLOOR_Y, 0)], Vec3::new(0.5, 120., 0.5));
    place_far(&mut s);
    assert_eq!((used_item_on(&sent), swings(&sent)), (1, 1));
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 48: `BlockItem#place` fails with an entity in the cell)
#[test]
fn a_place_with_an_entity_in_the_cell_sends_use_item_on_and_no_swing() {
    let _lock = init();
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), BlockPos::new(3, FLOOR_Y, 0)], Vec3::new(0.5, 120., 0.5));
    s.receive_packet(make_basic_add_entity(EntityKind::Zombie, 123, (3.95, 120., 0.5)));
    // its box reaches into the cell but the ray to the middle of the face passes beside it
    s.tick();
    sent.clear();
    place_far(&mut s);
    assert_eq!((used_item_on(&sent), swings(&sent)), (1, 0), "FAIL: sent, no swing");
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 48: a sapling needs dirt below, `VegetationBlock#canSurvive`)
#[test]
fn a_sapling_swings_on_grass_and_not_on_stone() {
    let _lock = init();
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), BlockPos::new(3, FLOOR_Y, 0)], Vec3::new(0.5, 120., 0.5));
    hold(&mut s, ItemKind::OakSapling, 2);
    sent.clear();
    place_far(&mut s);
    assert_eq!((used_item_on(&sent), swings(&sent)), (1, 0), "stone below: FAIL, no swing");

    s.receive_packet(ClientboundBlockUpdate { pos: BlockPos::new(3, FLOOR_Y, 0), block_state: BlockKind::GrassBlock.into() });
    s.tick();
    sent.clear();
    place_far(&mut s);
    assert_eq!((used_item_on(&sent), swings(&sent)), (1, 1), "grass below: SUCCESS, swing");
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 48: `LivingEntity` blocks building; `ArmorStand` follows its marker flag)
#[test]
fn an_armor_stand_in_the_cell_blocks_the_place_unless_it_is_a_marker() {
    use azalea_entity::{EntityDataItem, EntityDataValue, EntityMetadataItems};
    use azalea_protocol::packets::game::ClientboundSetEntityData;
    let _lock = init();
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), BlockPos::new(3, FLOOR_Y, 0)], Vec3::new(0.5, 120., 0.5));
    s.receive_packet(make_basic_add_entity(EntityKind::ArmorStand, 124, (3.95, 120., 0.5)));
    s.tick();
    sent.clear();
    place_far(&mut s);
    assert_eq!((used_item_on(&sent), swings(&sent)), (1, 0), "no flags received: blocks building, FAIL");

    // the marker flag (client flags, bit 16) arrives: it no longer blocks building
    s.receive_packet(ClientboundSetEntityData {
        id: 124.into(),
        packed_items: EntityMetadataItems(vec![EntityDataItem { index: 15, value: EntityDataValue::Byte(16) }]),
    });
    s.tick();
    sent.clear();
    place_far(&mut s);
    assert_eq!((used_item_on(&sent), swings(&sent)), (1, 1), "marker: the place succeeds, swing");
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 50: the order of the delay swing in one tick)
#[test]
fn a_place_goes_out_before_the_destroy_delay_swing_of_the_same_tick() {
    use azalea_client::mining::MineDelay;
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    look_at(&mut s, floor, Direction::Up);
    // the destroy delay of a break that just ended is counting down, the crosshair is on stone
    s.with_component_mut::<MineDelay>(|mut d| **d = 3);
    sent.clear();
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    // `Minecraft#handleKeybinds`: `startUseItem` (UseItemOn, its swing), then `continueAttack` (the delay swing)
    let kinds: Vec<&str> = sent
        .list
        .lock()
        .iter()
        .filter_map(|p| match p {
            ServerboundGamePacket::UseItemOn(_) => Some("UseItemOn"),
            ServerboundGamePacket::Swing(_) => Some("Swing"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["UseItemOn", "Swing", "Swing"]);
    // and the delay swing still comes in a tick with no click
    sent.clear();
    s.tick();
    let swings = sent.list.lock().iter().filter(|p| matches!(p, ServerboundGamePacket::Swing(_))).count();
    assert_eq!(swings, 1);
}

// Spec: specs/B6-placement-breaking.md §4.10 (a dimension change restarts the sequence)
// A new `ClientLevel` restarts the sequence at 0. EXPECTED: a place pending at the respawn ends as
// refused with `NoAck`, so the next ack (number 1 again) cannot settle it as acked.
#[test]
fn a_pending_place_ends_unacked_at_a_dimension_change_and_a_new_ack_does_not_settle_it() {
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, _sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    look_at(&mut s, floor, Direction::Up);
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    let old = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!((old.outcome, old.seq), (BlockActionOutcome::Pending, Some(1)));

    s.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(0), Identifier::new("azalea:other")));
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    let r = s.component::<BlockActionResults>().block_result(old.id).unwrap();
    assert_eq!(r.outcome, BlockActionOutcome::Refused);
    assert_eq!(r.refusal, Some(BlockActionRefusal::NoAck));
    assert_eq!(r.ack_ticks, None, "an ack of the new sequence is not the ack of the old action");
}

// Spec: specs/B6-placement-breaking.md §4.6 (settle from the ack and the block left behind)
#[test]
fn an_acked_place_that_left_another_block_is_refused() {
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, _sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    let target = floor.up(1);
    look_at(&mut s, floor, Direction::Up);
    insert(&mut s, PlaceBlockQueued { position: floor, direction: Direction::Up, auto_crouch: false });
    s.tick();
    s.receive_packet(ClientboundBlockUpdate { pos: target, block_state: BlockKind::Bedrock.into() });
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    let r = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!(r.outcome, BlockActionOutcome::Refused);
    assert_eq!(r.refusal, Some(BlockActionRefusal::OtherBlock));
}

/// A TNT block (no break time) under the crosshair, broken once: the finishing packet is out.
fn break_tnt(s: &mut Simulation, cell: BlockPos) {
    s.receive_packet(ClientboundBlockUpdate { pos: cell, block_state: BlockKind::Tnt.into() });
    s.tick();
    look_at(s, cell, Direction::Up);
    insert(s, BreakBlockQueued { position: cell });
    s.tick();
    s.tick();
    assert_eq!(s.bot_block_state(cell), Some(BlockState::AIR), "the bot's own prediction");
    assert!(s.component::<BlockActionResults>().last_break.unwrap().seq.is_some(), "the break was sent");
}

// Spec: specs/B6-placement-breaking.md §4.6 (settle from the ack and the block left behind)
#[test]
fn an_acked_break_that_left_another_block_is_refused() {
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, _sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    break_tnt(&mut s, floor);
    let seq = s.component::<BlockActionResults>().last_break.unwrap().seq.unwrap();
    s.receive_packet(ClientboundBlockUpdate { pos: floor, block_state: BlockKind::Bedrock.into() });
    s.receive_packet(ClientboundBlockChangedAck { seq });
    s.tick();
    let r = s.component::<BlockActionResults>().last_break.unwrap();
    assert_eq!(r.outcome, BlockActionOutcome::Refused);
    assert_eq!(r.refusal, Some(BlockActionRefusal::OtherBlock));
}

// Spec: specs/B6-placement-breaking.md §4.6 (the fork's ack timeout: `Refused`, "no ack")
#[test]
fn a_break_with_no_ack_is_refused_while_the_view_shows_the_prediction() {
    let _lock = init();
    let floor = BlockPos::new(1, FLOOR_Y, 0);
    let (mut s, _sent) = setup(&[BlockPos::new(0, FLOOR_Y, 0), floor], Vec3::new(0.5, 120., 0.5));
    break_tnt(&mut s, floor);
    for _ in 0..=ACK_TIMEOUT_TICKS {
        s.tick();
    }
    assert_eq!(s.bot_block_state(floor), Some(BlockState::AIR), "the prediction still shows");
    let r = s.component::<BlockActionResults>().last_break.unwrap();
    assert_eq!(r.outcome, BlockActionOutcome::Refused);
    assert_eq!(r.refusal, Some(BlockActionRefusal::NoAck));
}
