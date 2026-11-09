use azalea_client::test_utils::prelude::*;
use azalea_core::{entity_id::MinecraftEntityId, position::ChunkPos};
use azalea_entity::{LocalEntity, indexing::EntityIdIndex};
use azalea_protocol::packets::ConnectionProtocol;
use azalea_registry::builtin::EntityKind;
use uuid::Uuid;

// Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 1
// Two bots of one swarm log in close together. The server announces the joiner (id 77) to the
// earlier bot before the joiner's own Login. Each bot has its own world, as a vanilla client has
// one level per connection: the earlier bot keeps its own plain player entity for id 77, the
// joiner's Login does not touch it, and the joiner holds its own copy of the earlier bot.
#[test]
fn test_joiner_login_leaves_the_copy_another_bot_spawned_for_its_id() {
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
    let a_copy = world
        .get::<EntityIdIndex>(a)
        .unwrap()
        .get_by_minecraft_entity(MinecraftEntityId(77))
        .expect("A holds an entity for id 77");
    assert_ne!(a_copy, b, "A's entity for id 77 is its own copy, not B's local entity");
    assert!(world.get::<LocalEntity>(a_copy).is_none(), "A's copy is a plain entity");
    assert!(world.get::<LocalEntity>(b).is_some());
    let b_copy = world.get::<EntityIdIndex>(b).unwrap().get_by_minecraft_entity(a_id);
    assert!(b_copy.is_some_and(|e| e != a), "B holds its own copy of A");
}
