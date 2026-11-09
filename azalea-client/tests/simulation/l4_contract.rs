use azalea_client::test_utils::prelude::*;
use azalea_core::{
    entity_id::MinecraftEntityId,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_entity::{LookDirection, Position};
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockUpdate, ClientboundPlayerPosition},
    },
};
use azalea_registry::builtin::{BlockKind, EntityKind};
use azalea_protocol::packets::game::{ClientboundRemoveEntities, ClientboundTeleportEntity};
use azalea_block::BlockState;
use uuid::Uuid;

// Spec: specs/plans/LOAD_CONTRACT.md (L4, step 1 of the swarm world build order)
// Bot A's packets must not depend on what bot B received. Each test runs one fixed timeline for A
// twice, and only B's inbound packets differ. With one World per bot they pass. On a shared World
// they failed (the negative control, run when the patch was written).

const HEIGHT: usize = (384 + 64) / 16;

fn teleport(x: f64, z: f64) -> ClientboundPlayerPosition {
    ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation { pos: Vec3::new(x, 64., z), delta: Vec3::ZERO, look_direction: LookDirection::default() },
        relative: RelativeMovements::all_absolute(),
    }
}

/// A's sent packets in order, and its final height.
fn outcome(s: &mut Simulation, sent: &SentPackets, ticks: usize) -> (Vec<String>, f64) {
    sent.clear();
    for _ in 0..ticks {
        s.tick();
    }
    let packets = sent.list.lock().iter().map(|p| format!("{p:?}")).collect();
    (packets, s.app.world().get::<Position>(s.entity).unwrap().y)
}

fn block_update_timeline(b_told_first: bool) -> (Vec<String>, f64) {
    const FLOOR: BlockPos = BlockPos::new(8, 63, 8);
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), HEIGHT));
    s.receive_packet(ClientboundBlockUpdate { pos: FLOOR, block_state: BlockKind::Stone.into() });
    s.receive_packet(teleport(8.5, 8.5));
    let b = s.add_client(Uuid::from_u128(5678), "b");
    let mut login = default_login_packet();
    login.player_id = MinecraftEntityId(77);
    s.receive_packet_for(b, login);
    s.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(0, 0), HEIGHT));
    s.receive_packet_for(b, ClientboundBlockUpdate { pos: FLOOR, block_state: BlockKind::Stone.into() });
    s.receive_packet_for(b, teleport(3.5, 3.5));
    for _ in 0..6 {
        s.tick();
    }
    if b_told_first {
        s.receive_packet_for(b, ClientboundBlockUpdate { pos: FLOOR, block_state: BlockState::AIR });
    }
    // A's own update has not arrived in this window.
    outcome(&mut s, &sent, 10)
}

fn chunk_timeline(b_got_chunk: bool) -> (Vec<String>, f64) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), HEIGHT));
    s.receive_packet(teleport(3.5, 3.5));
    let b = s.add_client(Uuid::from_u128(5678), "b");
    let mut login = default_login_packet();
    login.player_id = MinecraftEntityId(77);
    s.receive_packet_for(b, login);
    s.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(0, 0), HEIGHT));
    s.receive_packet_for(b, teleport(3.5, 3.5));
    for _ in 0..6 {
        s.tick();
    }
    // A walks into the air over chunk (1, 1), which A has not received.
    s.receive_packet(teleport(24.5, 24.5));
    if b_got_chunk {
        s.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(1, 1), HEIGHT));
        s.receive_packet_for(b, ClientboundBlockUpdate { pos: BlockPos::new(24, 63, 24), block_state: BlockKind::Stone.into() });
    }
    outcome(&mut s, &sent, 10)
}

#[test]
fn a_ignores_a_block_update_only_b_received() {
    let _lock = init();
    assert_eq!(block_update_timeline(true), block_update_timeline(false));
}

#[test]
fn a_ignores_a_chunk_only_b_received() {
    let _lock = init();
    assert_eq!(chunk_timeline(true), chunk_timeline(false));
}

// Mobs and the entity index. Both bots stand on a stone floor at the same spot; the mob (id 50)
// overlaps them. Only B's inbound packets differ between the two runs. On a shared World, B's
// packet moves, adds or removes the one shared entity and A is pushed differently.

/// A's sent packets and final position.
fn pos_outcome(s: &mut Simulation, sent: &SentPackets, ticks: usize) -> (Vec<String>, [u64; 3]) {
    sent.clear();
    for _ in 0..ticks {
        s.tick();
    }
    let packets = sent.list.lock().iter().map(|p| format!("{p:?}")).collect();
    let p = s.app.world().get::<Position>(s.entity).unwrap();
    (packets, [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()])
}

/// What differs between the two runs of a mob timeline. Both bots always hear about the mob (id
/// 50) first; B is told first, so on a shared World B drives the mob's updates and A's are dropped.
#[derive(Clone, Copy, PartialEq)]
enum Differ {
    /// Neither bot hears anything else (the reference run).
    Nothing,
    /// A is told the mob moved away. B is not told. (Mob updates.)
    AMobMoved,
    /// Only B is told the mob moved to another chunk. (Mob updates and the entity index.)
    BMobMovedChunk,
    /// Only B is told the mob is gone. (Entity index.)
    BMobRemoved,
}

fn mob_timeline(differ: Differ) -> (Vec<String>, [u64; 3]) {
    mob_timeline_with(differ, true)
}

/// `b_hears_mob`: B is told about the mob (first) or never.
fn mob_timeline_with(differ: Differ, b_hears_mob: bool) -> (Vec<String>, [u64; 3]) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    let floor = |s: &mut Simulation, who: Option<bevy_ecs::entity::Entity>| {
        for x in 2..=14 {
            for z in 2..=14 {
                let p = ClientboundBlockUpdate { pos: BlockPos::new(x, 63, z), block_state: BlockKind::Stone.into() };
                match who {
                    Some(b) => s.receive_packet_for(b, p),
                    None => s.receive_packet(p),
                }
            }
        }
    };
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), HEIGHT));
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(1, 0), HEIGHT));
    floor(&mut s, None);
    s.receive_packet(teleport(8.5, 8.5));
    let b = s.add_client(Uuid::from_u128(5678), "b");
    let mut login = default_login_packet();
    login.player_id = MinecraftEntityId(77);
    s.receive_packet_for(b, login);
    s.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(0, 0), HEIGHT));
    s.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(1, 0), HEIGHT));
    floor(&mut s, Some(b));
    s.receive_packet_for(b, teleport(3.5, 3.5));
    for _ in 0..3 {
        s.tick();
    }
    // The mob overlaps A. B hears first, then A.
    let mut add = make_basic_add_entity(EntityKind::Pig, 50, (8.8, 64., 8.5));
    add.uuid = Uuid::from_u128(20_050);
    let moved = |x: f64| ClientboundTeleportEntity {
        id: MinecraftEntityId(50),
        change: PositionMoveRotation { pos: Vec3::new(x, 64., 8.5), delta: Vec3::ZERO, look_direction: LookDirection::default() },
        relative: RelativeMovements::all_absolute(),
        on_ground: true,
    };
    if b_hears_mob {
        s.receive_packet_for(b, add.clone());
        s.tick();
        // B's first update of the mob (to where it already is) makes B its driver on a shared
        // World, so A's own add and updates about it are then dropped.
        s.receive_packet_for(b, moved(8.8));
    }
    s.tick();
    s.receive_packet(add);
    for _ in 0..4 {
        s.tick();
    }
    match differ {
        _ if !b_hears_mob && differ != Differ::AMobMoved => {}
        Differ::Nothing => {}
        Differ::AMobMoved => s.receive_packet(moved(13.5)),
        Differ::BMobMovedChunk => s.receive_packet_for(b, moved(20.5)),
        Differ::BMobRemoved => s.receive_packet_for(b, ClientboundRemoveEntities { entity_ids: vec![MinecraftEntityId(50)] }),
    }
    pos_outcome(&mut s, &sent, 10)
}

// Mobs: on a shared World B drives the mob (B's add and first update came first), so A's own
// update about it is dropped. With B never hearing of the mob, A's update applies. They must agree.
#[test]
fn a_mob_moves_for_a_when_a_is_told_even_if_b_heard_it_first() {
    let _lock = init();
    assert_ne!(mob_timeline(Differ::AMobMoved), mob_timeline(Differ::Nothing), "the move changes A's pushes");
    // The same timeline with B never hearing of the mob gives A the same answer.
    assert_eq!(mob_timeline(Differ::AMobMoved), mob_timeline_with(Differ::AMobMoved, false));
}

#[test]
fn a_ignores_a_mob_move_only_b_received() {
    let _lock = init();
    assert_eq!(mob_timeline(Differ::BMobMovedChunk), mob_timeline(Differ::Nothing));
}

// Not a negative control: a removal told to B only drops B from the mob's `LoadedBy`, and the
// shared mob stays for A, so this holds in both modes. It guards the removal path in own mode.
#[test]
fn a_ignores_a_removal_only_b_received() {
    let _lock = init();
    assert_eq!(mob_timeline(Differ::BMobRemoved), mob_timeline(Differ::Nothing));
}
