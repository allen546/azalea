use azalea_client::test_utils::prelude::*;
use azalea_core::{entity_id::MinecraftEntityId, position::ChunkPos};
use azalea_entity::{LocalEntity, indexing::EntityIdIndex, interpolation::PeerView};
use azalea_protocol::packets::ConnectionProtocol;
use azalea_registry::builtin::EntityKind;
use bevy_ecs::prelude::*;
use uuid::Uuid;

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: the bot-to-bot view)
// Two bots of one swarm log in close together. The server announces the joiner (id 77) to the
// earlier bot before the joiner's own Login registers it, so the earlier bot spawns a plain
// player entity with that id (a twin). The joiner's Login must adopt the twin: one entity for
// the id, the earlier bot's index pointing at the joiner's local entity, and the view holding it.
#[test]
fn test_joiner_login_adopts_the_twin_another_bot_spawned_for_its_id() {
    let _lock = init();
    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.receive_packet(default_login_packet());
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    simulation.tick();
    let a = simulation.entity;
    let a_id = simulation.minecraft_entity_id();
    let a_uuid = simulation.app.world().get::<azalea_entity::EntityUuid>(a).map(|u| **u);

    // The earlier bot hears about the joiner first.
    let b_uuid = Uuid::from_u128(5678);
    let mut add = make_basic_add_entity(EntityKind::Player, 77, (0., 0., 0.));
    add.uuid = b_uuid;
    simulation.receive_packet(add);
    simulation.tick();

    // Then the joiner logs in with that id and hears about the earlier bot.
    let b = simulation.add_client(b_uuid, "b");
    let mut login = default_login_packet();
    login.player_id = MinecraftEntityId(77);
    simulation.receive_packet_for(b, login);
    simulation.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    let mut add_a = make_basic_add_entity(EntityKind::Player, a_id.0, (0., 0., 0.));
    add_a.uuid = a_uuid.unwrap_or(Uuid::from_u128(1234));
    simulation.receive_packet_for(b, add_a);
    for _ in 0..3 {
        simulation.tick();
    }

    let world = simulation.app.world();
    let a_index = world.get::<EntityIdIndex>(a).unwrap();
    assert_eq!(a_index.get_by_minecraft_entity(MinecraftEntityId(77)), Some(b), "A's index holds the joiner's own entity");
    let mut twins = 0;
    let mut q = simulation.app.world_mut().query_filtered::<&MinecraftEntityId, Without<LocalEntity>>();
    for id in q.iter(simulation.app.world()) {
        if id.0 == 77 {
            twins += 1;
        }
    }
    assert_eq!(twins, 0, "a non-local entity still carries the joiner's id");
    let world = simulation.app.world();
    assert!(world.get::<PeerView>(a).unwrap().contains(b), "A's view holds B");
    assert!(world.get::<PeerView>(b).unwrap().contains(a), "B's view holds A");
}
