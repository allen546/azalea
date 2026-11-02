use azalea_client::{
    interact::BlockStatePredictionHandler, mining::StartMiningBlockEvent, test_utils::prelude::*,
};
use azalea_core::{
    entity_id::MinecraftEntityId,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_entity::{LookDirection, Physics, Position};
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockChangedAck, ClientboundBlockUpdate, ClientboundPlayerPosition},
    },
};
use azalea_registry::builtin::BlockKind;
use bevy_ecs::prelude::*;
use uuid::Uuid;

// Spec: specs/plans/SHARED_WORLD_TEST.md (the code fact)
// Two bots of one swarm share one World. A breaks an instant-break block (tnt) under B and the
// server sends nothing. EXPECTED (the shared-world issue is real in the code): after A's tick,
// B's world reads air at the cell while A's prediction is still pending, and B's own physics
// then starts to fall, although the server (which has not answered) still has the block under B.
// This proves the code fact only, not any harm to the server or to Grim.

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
fn test_bot_b_reads_bot_a_unconfirmed_break_in_the_shared_world() {
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
    // B reads A's unconfirmed air from the shared world.
    assert_eq!(s.get_block_state(CELL), Some(BlockKind::Air.into()), "B's world shows A's prediction");
    s.tick();
    s.tick();
    assert!(b_y(&s, b) < 64.0, "B fell through a block the server still has (y {})", b_y(&s, b));
    assert!(!s.app.world().get::<Physics>(b).unwrap().on_ground());

    // Only A is rolled back when the server acks without an update: B had fallen meanwhile.
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
