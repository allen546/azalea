//! A chunk packet that arrives before a respawn into another dimension belongs to the old level
//! (`ClientPacketListener#handleLevelChunkWithLight` runs in packet order, `#handleRespawn` then
//! builds a new level). Null must not decode it against the new level.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)

use azalea_client::test_utils::prelude::*;
use azalea_core::position::ChunkPos;
use azalea_protocol::packets::{ConnectionProtocol, config::{ClientboundFinishConfiguration, ClientboundRegistryData}};
use azalea_registry::{DataRegistry, data::DimensionKind, identifier::Identifier};
use simdnbt::owned::{NbtCompound, NbtTag};

fn start() -> Simulation {
    let mut simulation = Simulation::new(ConnectionProtocol::Configuration);
    simulation.receive_packet(ClientboundRegistryData {
        registry_id: Identifier::new("minecraft:dimension_type"),
        entries: vec![
            (Identifier::new("azalea:fakedimension"), Some(NbtCompound::from_values(vec![
                ("height".into(), NbtTag::Int(16)), ("min_y".into(), NbtTag::Int(0))]))),
            (Identifier::new("minecraft:overworld"), Some(NbtCompound::from_values(vec![
                ("height".into(), NbtTag::Int(384)), ("min_y".into(), NbtTag::Int(-64))]))),
            (Identifier::new("minecraft:nether"), Some(NbtCompound::from_values(vec![
                ("height".into(), NbtTag::Int(256)), ("min_y".into(), NbtTag::Int(0))]))),
        ]
        .into_iter()
        .collect(),
    });
    simulation.tick();
    simulation.receive_packet(ClientboundFinishConfiguration);
    simulation.tick();
    simulation
}

const OVERWORLD_SECTIONS: usize = (384 + 64) / 16;
const NETHER_SECTIONS: usize = 256 / 16;

#[test]
fn old_level_chunk_queued_before_a_respawn_is_not_decoded_into_the_new_level() {
    let _lock = init();
    let mut simulation = start();
    simulation.receive_packet(make_basic_login_packet(DimensionKind::new_raw(1), Identifier::new("azalea:a")));
    simulation.tick();

    // One tick holds all three packets, in this order: an overworld chunk, the move to the
    // nether, a nether chunk.
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(3, 3), OVERWORLD_SECTIONS));
    simulation.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(2), Identifier::new("azalea:b")));
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(4, 4), NETHER_SECTIONS));
    simulation.tick();

    assert!(simulation.chunk(ChunkPos::new(4, 4)).is_some(), "the nether chunk sent after the respawn is kept");
    assert!(simulation.chunk(ChunkPos::new(3, 3)).is_none(), "the overworld chunk belongs to the old level");
}

#[test]
fn nether_chunk_queued_before_a_respawn_to_the_overworld_is_not_decoded_into_it() {
    let _lock = init();
    let mut simulation = start();
    simulation.receive_packet(make_basic_login_packet(DimensionKind::new_raw(2), Identifier::new("azalea:b")));
    simulation.tick();

    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(3, 3), NETHER_SECTIONS));
    simulation.receive_packet(make_basic_respawn_packet(DimensionKind::new_raw(1), Identifier::new("azalea:a")));
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(4, 4), OVERWORLD_SECTIONS));
    simulation.tick();

    assert!(simulation.chunk(ChunkPos::new(4, 4)).is_some());
    assert!(simulation.chunk(ChunkPos::new(3, 3)).is_none(), "a nether chunk must not appear in the overworld");
}
