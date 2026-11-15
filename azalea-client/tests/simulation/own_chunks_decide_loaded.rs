use azalea_client::test_utils::prelude::*;
use azalea_core::position::ChunkPos;
use azalea_entity::HasClientLoaded;
use azalea_protocol::packets::ConnectionProtocol;
use azalea_registry::{DataRegistry, data::DimensionKind, identifier::Identifier};

// Spec: specs/F5-version-layer.md §4.6 (What Null computes). `LevelLoadStatusManager#tick`: a client
// is loaded when the chunk it stands in is in its own level. After a respawn the bot's own chunks are
// empty (another dimension builds a new level; the same dimension keeps its chunks, patch 77); a chunk
// another bot of the swarm still holds in the old level must not count.
#[test]
fn a_bot_respawned_into_another_dimension_waits_for_its_own_chunk_even_if_another_bot_holds_it() {
    let _lock = init();
    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.receive_packet(default_login_packet());
    simulation.tick();
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    for _ in 0..3 {
        simulation.tick();
    }
    assert!(simulation.has_component::<HasClientLoaded>(), "control: loaded once its chunk arrived");

    // Another bot holds the chunk (a strong reference keeps it in the shared storage).
    let other_bots_chunk = simulation.chunk(ChunkPos::new(0, 0)).expect("chunk");

    simulation.receive_packet(make_basic_respawn_packet(
        DimensionKind::new_raw(0),
        Identifier::new("azalea:other"),
    ));
    for _ in 0..5 {
        simulation.tick();
    }
    assert!(simulation.chunk(ChunkPos::new(0, 0)).is_none(), "the new level has no chunk yet");
    assert!(
        !simulation.has_component::<HasClientLoaded>(),
        "the bot's own chunks are empty after the respawn, so it is not loaded yet"
    );

    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    for _ in 0..3 {
        simulation.tick();
    }
    assert!(simulation.has_component::<HasClientLoaded>(), "loaded once the server sends its chunk");
    drop(other_bots_chunk);
}
