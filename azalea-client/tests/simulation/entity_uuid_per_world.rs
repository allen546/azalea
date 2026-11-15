use azalea_auth::game_profile::GameProfile;
use azalea_client::{player::GameProfileComponent, test_utils::prelude::*};
use azalea_core::{entity_id::MinecraftEntityId, game_type::GameMode, position::ChunkPos};
use azalea_entity::{EntityUuid, indexing::EntityIdIndex};
use azalea_protocol::packets::{
    ConnectionProtocol,
    game::{
        ClientboundPlayerInfoUpdate,
        c_player_info_update::{ActionEnumSet, PlayerInfoEntry},
    },
};
use azalea_registry::builtin::EntityKind;
use bevy_ecs::prelude::*;
use uuid::Uuid;

// Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 1
// Two bots of one swarm, each with its own world. Bot B hears about bot A (it spawns a copy of A)
// and gets a player-info entry for A's uuid. The uuid belongs to two entities in the app: A's own
// entity and B's copy. Every lookup must stay in B's world, so only B's copy gets the profile.
// Vanilla has one level per connection, so `ClientPacketListener#handlePlayerInfoUpdate` can
// only ever reach entities of that connection.
fn setup() -> (Simulation, Entity, Entity, Uuid) {
    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.receive_packet(default_login_packet());
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    simulation.tick();
    let a = simulation.entity;
    let a_id = simulation.minecraft_entity_id();
    let a_uuid = **simulation.app.world().get::<EntityUuid>(a).expect("A has a uuid");

    let b = simulation.add_client(Uuid::from_u128(5678), "b");
    let mut login = default_login_packet();
    login.player_id = MinecraftEntityId(77);
    simulation.receive_packet_for(b, login);
    simulation.receive_packet_for(b, make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    let mut add_a = make_basic_add_entity(EntityKind::Player, a_id.0, (0., 0., 0.));
    add_a.uuid = a_uuid;
    simulation.receive_packet_for(b, add_a);
    for _ in 0..3 {
        simulation.tick();
    }
    let b_copy_of_a = simulation
        .app
        .world()
        .get::<EntityIdIndex>(b)
        .unwrap()
        .get_by_minecraft_entity(a_id)
        .expect("B holds a copy of A");
    assert_ne!(b_copy_of_a, a, "control: B's copy is not A's own entity");
    (simulation, b, b_copy_of_a, a_uuid)
}

#[test]
fn test_a_player_info_packet_to_bot_b_never_writes_onto_bot_a() {
    let _lock = init();
    let (mut simulation, b, b_copy_of_a, a_uuid) = setup();
    let a = simulation.entity;
    let a_name_before = simulation.app.world().get::<GameProfileComponent>(a).map(|p| p.name.clone());

    simulation.receive_packet_for(
        b,
        ClientboundPlayerInfoUpdate {
            actions: ActionEnumSet {
                add_player: true,
                initialize_chat: false,
                update_game_mode: false,
                update_listed: false,
                update_latency: false,
                update_display_name: false,
                update_list_order: false,
                update_hat: false,
            },
            entries: vec![PlayerInfoEntry {
                profile: GameProfile::new(a_uuid, "named-by-b".to_owned()),
                game_mode: GameMode::Survival,
                ..Default::default()
            }],
        },
    );
    for _ in 0..3 {
        simulation.tick();
    }

    let world = simulation.app.world();
    assert_eq!(
        world.get::<GameProfileComponent>(a).map(|p| p.name.clone()),
        a_name_before,
        "B's packet changed A's own entity"
    );
    assert_eq!(
        world.get::<GameProfileComponent>(b_copy_of_a).map(|p| p.name.clone()).as_deref(),
        Some("named-by-b"),
        "B's own copy of A gets the profile"
    );
}

#[test]
fn test_each_bots_world_resolves_the_uuid_to_its_own_entity() {
    let _lock = init();
    let (simulation, b, b_copy_of_a, a_uuid) = setup();
    let a = simulation.entity;
    let a_world = simulation.app.world().get::<azalea_client::local_player::WorldHolder>(a).unwrap().shared.clone();
    let b_world = simulation.app.world().get::<azalea_client::local_player::WorldHolder>(b).unwrap().shared.clone();
    assert_eq!(a_world.read().entity_by_uuid.get(&a_uuid), Some(&a), "A's world: A's own entity");
    assert_eq!(b_world.read().entity_by_uuid.get(&a_uuid), Some(&b_copy_of_a), "B's world: B's copy of A");
}
