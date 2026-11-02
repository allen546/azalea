use azalea_client::{
    block_actions::{BlockActionOutcome, BlockActionRefusal, BlockActionResults, BreakBlockQueued, LookAtBlockQueued, PlaceBlockQueued},
    local_player::LocalGameMode,
    test_utils::prelude::*,
};
use azalea_core::{
    direction::Direction,
    game_type::GameMode,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_entity::LookDirection;
use azalea_inventory::ItemStack;
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{
            ClientboundBlockUpdate, ClientboundContainerSetSlot, ClientboundInitializeBorder,
            ClientboundPlayerPosition, ServerboundGamePacket,
        },
    },
};
use azalea_registry::builtin::{BlockKind, ItemKind};
use bevy_ecs::prelude::*;

const FLOOR_Y: i32 = 119;

/// A bot at (0.5, 120, 0.5) holding dirt, on a stone floor along x = 0..=2, in a border 4 wide
/// centred on the origin (edges -2 and 2): the blocks at x = 0 and 1 are inside, x = 2 is not.
fn setup() -> (Simulation, SentPackets) {
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
    for x in 0..=2 {
        s.receive_packet(ClientboundBlockUpdate { pos: BlockPos::new(x, FLOOR_Y, 0), block_state: BlockKind::Stone.into() });
    }
    s.receive_packet(ClientboundInitializeBorder {
        new_center_x: 0.0,
        new_center_z: 0.0,
        old_size: 4.0,
        new_size: 4.0,
        lerp_time: 0,
        new_absolute_max_size: 29999984,
        warning_blocks: 5,
        warning_time: 15,
    });
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: Vec3::new(0.5, 120., 0.5),
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

fn count(sent: &SentPackets, f: impl Fn(&ServerboundGamePacket) -> bool) -> usize {
    sent.list.lock().iter().filter(|p| f(p)).count()
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 51: `MultiPlayerGameMode#useItemOn` and the border)
#[test]
fn a_place_on_a_block_outside_the_border_is_refused_and_sends_nothing() {
    let _lock = init();
    let (mut s, sent) = setup();
    let outside = BlockPos::new(2, FLOOR_Y, 0);

    look_at(&mut s, outside, Direction::Up);
    sent.clear();
    insert(&mut s, PlaceBlockQueued { position: outside, direction: Direction::Up, auto_crouch: false });
    s.tick();
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::UseItemOn(_))), 0);
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::Swing(_))), 0, "no swing: FAIL stops startUseItem");
    assert_eq!(s.get_block_state(outside.up(1)), Some(azalea_block::BlockState::AIR), "nothing predicted");
    let r = s.component::<BlockActionResults>().last_place.unwrap();
    assert_eq!((r.outcome, r.refusal), (BlockActionOutcome::Refused, Some(BlockActionRefusal::OutsideBorder)));

    // the block just inside the edge is clicked as before
    let inside = BlockPos::new(1, FLOOR_Y, 0);
    look_at(&mut s, inside, Direction::Up);
    sent.clear();
    insert(&mut s, PlaceBlockQueued { position: inside, direction: Direction::Up, auto_crouch: false });
    s.tick();
    assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::UseItemOn(_))), 1);
}

// Spec: specs/B6-placement-breaking.md §4.10 (patch 51: `MultiPlayerGameMode#startDestroyBlock` and the border)
#[test]
fn a_break_of_a_block_outside_the_border_starts_nothing() {
    let _lock = init();
    for creative in [false, true] {
        let (mut s, sent) = setup();
        if creative {
            s.with_component_mut::<LocalGameMode>(|mut m| m.current = GameMode::Creative);
        }
        let outside = BlockPos::new(2, FLOOR_Y, 0);
        look_at(&mut s, outside, Direction::Up);
        sent.clear();
        insert(&mut s, BreakBlockQueued { position: outside });
        s.tick();
        s.tick();
        // a held key (`continueAttack`) swings only when `startDestroyBlock` answered true: nothing goes out
        let actions = count(&sent, |p| matches!(p, ServerboundGamePacket::PlayerAction(_)));
        let swings = count(&sent, |p| matches!(p, ServerboundGamePacket::Swing(_)));
        assert_eq!((actions, swings), (0, 0), "creative: {creative}");
        assert_eq!(s.get_block_state(outside), Some(BlockKind::Stone.into()));
        let r = s.component::<BlockActionResults>().last_break.unwrap();
        assert_eq!((r.outcome, r.refusal), (BlockActionOutcome::Refused, Some(BlockActionRefusal::OutsideBorder)));

        // inside the border the same click starts a break
        let inside = BlockPos::new(1, FLOOR_Y, 0);
        look_at(&mut s, inside, Direction::Up);
        sent.clear();
        insert(&mut s, BreakBlockQueued { position: inside });
        s.tick();
        assert_eq!(count(&sent, |p| matches!(p, ServerboundGamePacket::PlayerAction(_))), 1, "creative: {creative}");
    }
}
