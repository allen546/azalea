use azalea_block::BlockState;
use azalea_client::{
    interact::BlockStatePredictionHandler, mining::StartMiningBlockEvent, test_utils::prelude::*,
};
use azalea_registry::{DataRegistry, data::DimensionKind, identifier::Identifier};
use azalea_core::{
    entity_id::MinecraftEntityId,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_entity::{LookDirection, Physics, Position};
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockChangedAck, ClientboundBlockUpdate, ClientboundForgetLevelChunk, ClientboundPlayerPosition},
    },
};
use azalea_registry::builtin::BlockKind;
use bevy_ecs::prelude::*;
use uuid::Uuid;

// Spec: specs/plans/SHARED_WORLD_TEST.md (the code fact; the expected result changed with patch 53)
// Two bots of one swarm. A breaks an instant-break block (tnt) under B and the server sends
// nothing. Before patch 53 B read A's unconfirmed air and fell. EXPECTED now (patch 76): each bot
// has its own World and A's prediction is a write into A's world, so B keeps standing on the
// block and A alone shows air.

const CELL: BlockPos = BlockPos::new(8, 63, 8);

fn setup() -> (Simulation, Entity) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.receive_packet(ClientboundBlockUpdate { pos: CELL, block_state: BlockKind::Tnt.into() });
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation { pos: Vec3::new(3.5, 64., 3.5), delta: Vec3::ZERO, look_direction: LookDirection::default() },
        relative: RelativeMovements::all_absolute(),
    });
    for _ in 0..3 {
        s.tick();
    }
    // B joins the same world and stands on the cell.
    let b = s.add_client(Uuid::from_u128(5678), "b");
    let mut login = default_login_packet();
    login.player_id = MinecraftEntityId(77);
    s.receive_packet_for(b, login);
    s.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    // The server tells B too: with one World per bot, B reads only what B's own packets said.
    s.receive_packet_for(b, ClientboundBlockUpdate { pos: CELL, block_state: BlockKind::Tnt.into() });
    s.receive_packet_for(
        b,
        ClientboundPlayerPosition {
            id: 1,
            change: PositionMoveRotation { pos: Vec3::new(8.5, 64., 8.5), delta: Vec3::ZERO, look_direction: LookDirection::default() },
            relative: RelativeMovements::all_absolute(),
        },
    );
    for _ in 0..6 {
        s.tick();
    }
    (s, b)
}

fn b_y(s: &Simulation, b: Entity) -> f64 {
    s.app.world().get::<Position>(b).unwrap().y
}

#[test]
fn test_bot_b_does_not_read_bot_a_unconfirmed_break() {
    let _lock = init();
    let (mut s, b) = setup();
    let a = s.entity;
    assert_eq!(s.get_block_state(CELL), Some(BlockKind::Tnt.into()), "the server's block is under B");
    assert!(s.app.world().get::<Physics>(b).unwrap().on_ground(), "B stands on the block");
    assert_eq!(b_y(&s, b), 64.0);

    s.write_message(StartMiningBlockEvent { entity: a, position: CELL, force: true });
    s.tick();
    // A predicted air; the server has said nothing, so A's prediction is pending.
    let pending = s.app.world().get::<BlockStatePredictionHandler>(a).unwrap();
    assert!(pending.seq() > pending.acked_seq(), "A's prediction is unconfirmed");
    // A's own world shows the prediction; B's world still holds the server's block.
    assert_eq!(s.get_block_state(CELL), Some(BlockState::AIR), "A's world shows its own break");
    assert_eq!(view(&s, b, CELL), Some(BlockKind::Tnt.into()), "B's world shows the server's block");
    s.tick();
    s.tick();
    assert_eq!(b_y(&s, b), 64.0, "B stays on the block the server still has");
    assert!(s.app.world().get::<Physics>(b).unwrap().on_ground());

    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    assert_eq!(s.get_block_state(CELL), Some(BlockKind::Tnt.into()));
}

// The control: without A's break, B stays put.
#[test]
fn test_bot_b_stays_on_the_block_when_a_predicts_nothing() {
    let _lock = init();
    let (mut s, b) = setup();
    s.tick();
    s.tick();
    s.tick();
    assert_eq!(b_y(&s, b), 64.0);
    assert!(s.app.world().get::<Physics>(b).unwrap().on_ground());
}

// ---------------------------------------------------------------------------------------------
// Patch 53 (reworked by patch 76): A's prediction is read by A alone, because it is a write into
// A's own world.
// Spec: specs/B6-placement-breaking.md §4.10

/// A prediction of `new` at `pos` in `who`'s own world; `old` is what the world must hold now.
fn make_pending(s: &mut Simulation, who: Entity, pos: BlockPos, old: BlockState, new: BlockState) {
    let shared = s.app.world().get::<azalea_client::local_player::WorldHolder>(who).unwrap().shared.clone();
    assert_eq!(shared.read().get_block_state(pos), Some(old));
    let mut h = s.app.world_mut().entity_mut(who);
    let mut h = h.get_mut::<BlockStatePredictionHandler>().unwrap();
    h.start_predicting();
    h.predict_block(&shared.read(), pos, new, Vec3::ZERO);
}

/// What `who` reads at `pos` in its own world.
fn view(s: &Simulation, who: Entity, pos: BlockPos) -> Option<BlockState> {
    let w = s.app.world();
    let holder = w.get::<azalea_client::local_player::WorldHolder>(who).unwrap();
    holder.shared.read().get_block_state(pos)
}

fn teleport_both_over(s: &mut Simulation, b: Entity, id: u32, x: f64, y: f64, z: f64) {
    for who in [s.entity, b] {
        s.receive_packet_for(
            who,
            ClientboundPlayerPosition {
                id,
                change: PositionMoveRotation { pos: Vec3::new(x, y, z), delta: Vec3::ZERO, look_direction: LookDirection::default() },
                relative: RelativeMovements::all_absolute(),
            },
        );
    }
}

// Edge case c: two clients in one World. EXPECTED: A predicts stone under both bots; A stands on
// its own prediction (it collides with it) and B, who has no such prediction, falls through the
// same cell. The shared world still holds air.
#[test]
fn test_a_collides_with_its_own_prediction_and_b_does_not() {
    let _lock = init();
    let (mut s, b) = setup();
    let a = s.entity;
    let floor = BlockPos::new(3, 63, 3);
    make_pending(&mut s, a, floor, BlockState::AIR, BlockKind::Stone.into());
    teleport_both_over(&mut s, b, 2, 3.5, 64., 3.5);
    for _ in 0..5 {
        s.tick();
    }
    assert_eq!(s.server_block_state(floor), Some(BlockState::AIR), "A's kept server state is air");
    assert_eq!(view(&s, a, floor), Some(BlockKind::Stone.into()), "A reads its own prediction");
    assert_eq!(view(&s, b, floor), Some(BlockState::AIR), "B does not");
    assert_eq!(b_y(&s, a), 64.0, "A stands on its predicted block");
    assert!(s.app.world().get::<Physics>(a).unwrap().on_ground());
    assert!(b_y(&s, b) < 64.0, "B has no block there and falls (y {})", b_y(&s, b));
}

// Edge case e: a refused action. A mines the block under B and the server answers with an ack and
// no update. EXPECTED: B never sees it (stays on the block, on the ground, the whole time); A
// reads air while pending and the block again after the ack (the snap-back).
#[test]
fn test_a_refused_break_is_never_seen_by_b_and_a_gets_the_block_back() {
    let _lock = init();
    let (mut s, b) = setup();
    let a = s.entity;
    s.write_message(StartMiningBlockEvent { entity: a, position: CELL, force: true });
    for _ in 0..3 {
        s.tick();
        assert_eq!(view(&s, a, CELL), Some(BlockState::AIR), "A shows its own break while pending");
        assert_eq!(view(&s, b, CELL), Some(BlockKind::Tnt.into()), "B reads the server's block");
        assert_eq!(b_y(&s, b), 64.0);
        assert!(s.app.world().get::<Physics>(b).unwrap().on_ground());
    }
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    assert_eq!(view(&s, a, CELL), Some(BlockKind::Tnt.into()), "the refusal puts the block back for A");
    assert_eq!(b_y(&s, b), 64.0);
}

// A server update reaches the shared world even when A has the cell pending, and the ack then
// keeps A's shown block when it matches.
#[test]
fn test_a_server_update_for_a_pending_cell_writes_the_shared_world() {
    let _lock = init();
    let (mut s, b) = setup();
    let a = s.entity;
    s.write_message(StartMiningBlockEvent { entity: a, position: CELL, force: true });
    s.tick();
    s.receive_packet(ClientboundBlockUpdate { pos: CELL, block_state: BlockState::AIR });
    // The server tells B as well; with one World per bot B reads only its own packets.
    s.receive_packet_for(b, ClientboundBlockUpdate { pos: CELL, block_state: BlockState::AIR });
    s.tick();
    assert_eq!(s.get_block_state(CELL), Some(BlockState::AIR), "the update wrote the shared world");
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    assert_eq!(view(&s, a, CELL), Some(BlockState::AIR));
    assert_eq!(view(&s, b, CELL), Some(BlockState::AIR));
}

fn single_bot_over_air() -> Simulation {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation { pos: Vec3::new(3.5, 64., 3.5), delta: Vec3::ZERO, look_direction: LookDirection::default() },
        relative: RelativeMovements::all_absolute(),
    });
    s
}

// Edge case a: this bot's own chunk packet arrives while a cell is pending. Vanilla
// (`ClientPacketListener#updateLevelChunk`) replaces the chunk, so the packet's block shows; the
// entry stays. EXPECTED (vanilla result, patch 76): the bot stands on its predicted block until the
// chunk is re-sent, then falls; at the ack the kept state (the server's dirt, which the packet
// did not hold) is written over the packet's block.
#[test]
fn test_a_resent_chunk_beats_the_pending_prediction() {
    let _lock = init();
    let mut s = single_bot_over_air();
    let a = s.entity;
    let floor = BlockPos::new(3, 63, 3);
    s.receive_packet(ClientboundBlockUpdate { pos: floor, block_state: BlockKind::Dirt.into() });
    for _ in 0..3 {
        s.tick();
    }
    make_pending(&mut s, a, floor, BlockKind::Dirt.into(), BlockKind::Stone.into());
    s.receive_packet(ClientboundPlayerPosition {
        id: 2,
        change: PositionMoveRotation { pos: Vec3::new(3.5, 64., 3.5), delta: Vec3::ZERO, look_direction: LookDirection::default() },
        relative: RelativeMovements::all_absolute(),
    });
    for _ in 0..4 {
        s.tick();
    }
    assert_eq!(b_y(&s, a), 64.0, "standing on the prediction");
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.tick();
    assert_eq!(view(&s, a, floor), Some(BlockState::AIR), "the packet's block wins");
    assert!(s.component::<BlockStatePredictionHandler>().is_pending(floor), "the entry stays");
    s.tick();
    s.tick();
    assert!(b_y(&s, a) < 64.0, "the bot falls once the packet's block is read");
    // the ack writes the kept state over the packet's block
    s.receive_packet(ClientboundBlockChangedAck { seq: 1 });
    s.tick();
    assert_eq!(s.component::<BlockStatePredictionHandler>().acked_seq(), 1);
    assert_eq!(view(&s, a, floor), Some(BlockKind::Dirt.into()), "the kept state is written at the ack");
}

// Edge case b: a chunk this bot forgets. EXPECTED: its pending cell is not read again, even if
// the chunk comes back (the level holds no block for a chunk the bot lost).
#[test]
fn test_a_forgotten_chunk_does_not_come_back_through_the_prediction() {
    let _lock = init();
    let mut s = single_bot_over_air();
    let a = s.entity;
    for _ in 0..3 {
        s.tick();
    }
    let floor = BlockPos::new(3, 63, 3);
    make_pending(&mut s, a, floor, BlockState::AIR, BlockKind::Stone.into());
    assert_eq!(view(&s, a, floor), Some(BlockKind::Stone.into()));
    s.receive_packet(ClientboundForgetLevelChunk { pos: ChunkPos::new(0, 0) });
    s.tick();
    assert_eq!(view(&s, a, floor), None, "no chunk, no block");
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.tick();
    assert_eq!(view(&s, a, floor), Some(BlockState::AIR), "the chunk is back with the server's block");
}

// Edge case d: `ClientPacketListener#handleRespawn` builds a new `ClientLevel` (with a new
// `BlockStatePredictionHandler`) when the dimension changes, and keeps the level otherwise.
// EXPECTED: a respawn into another dimension clears the pending cells and restarts the sequence at
// 0; a respawn into the same dimension keeps them. Before patch 53 the handler survived both.
#[test]
fn test_a_dimension_change_clears_the_pending_cells_and_a_same_dimension_respawn_keeps_them() {
    let _lock = init();
    let mut s = single_bot_over_air();
    let a = s.entity;
    for _ in 0..3 {
        s.tick();
    }
    let floor = BlockPos::new(3, 63, 3);
    make_pending(&mut s, a, floor, BlockState::AIR, BlockKind::Stone.into());
    s.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(0), Identifier::new("minecraft:overworld")));
    s.tick();
    assert_eq!(s.component::<BlockStatePredictionHandler>().seq(), 1, "same dimension: the level and its handler stay");
    // the cell stays pending and the level, with its chunk, still shows the prediction
    assert!(s.component::<BlockStatePredictionHandler>().is_pending(floor));
    assert_eq!(view(&s, a, floor), Some(BlockKind::Stone.into()), "same dimension: the level keeps the predicted block");
    s.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(0), Identifier::new("azalea:other")));
    s.tick();
    let h = s.component::<BlockStatePredictionHandler>();
    assert_eq!(h.seq(), 0, "another dimension: a new handler, the sequence restarts");
    assert!(!h.is_pending(floor), "the pending cells are gone");
}

// `ClientPacketListener#handleRespawn`: the same dimension keeps the level and its chunks; another
// dimension builds a new level with no chunks (the server sends them again).
// Spec: specs/B6-placement-breaking.md §4.10
#[test]
fn test_chunks_stay_after_a_same_dimension_respawn_and_are_replaced_after_a_dimension_change() {
    let _lock = init();
    let mut s = single_bot_over_air();
    let a = s.entity;
    for _ in 0..3 {
        s.tick();
    }
    let floor = BlockPos::new(3, 63, 3);
    assert_eq!(view(&s, a, floor), Some(BlockState::AIR), "the chunk is loaded");
    s.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(0), Identifier::new("minecraft:overworld")));
    s.tick();
    assert_eq!(view(&s, a, floor), Some(BlockState::AIR), "same dimension: the chunk stays loaded");
    s.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(0), Identifier::new("azalea:other")));
    s.tick();
    assert_eq!(view(&s, a, floor), None, "another dimension: a new level with no chunk");
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.tick();
    assert_eq!(view(&s, a, floor), Some(BlockState::AIR), "the new level takes the resent chunk");
}
