use azalea_block::{BlockState, fluid_state::FluidKind};
use azalea_client::test_utils::prelude::*;
use azalea_core::position::{BlockPos, ChunkPos, Vec3};
use azalea_entity::{FluidOnEyes, InLoadedChunk, LookDirection, ReadsInLoadedChunk};
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockUpdate, ClientboundPlayerPosition},
    },
};
use azalea_registry::builtin::{BlockKind, EntityKind};

// Spec: specs/plans/LOAD_CONTRACT.md "Where the added CPU goes"
// The fluid-on-eyes and in-loaded-chunk systems run only where a reader exists: the bot's own
// entity for the fluid, and entities marked `ReadsInLoadedChunk` for the chunk flag. A copy (here
// a pig) standing in water gets neither. Each skip has its control in the same test.
#[test]
fn copies_skip_the_fluid_and_chunk_systems_unless_a_reader_is_marked() {
    let _lock = init();
    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    for x in 1..=6 {
        for z in 1..=6 {
            for y in 63..=68 {
                s.receive_packet(ClientboundBlockUpdate { pos: BlockPos::new(x, y, z), block_state: BlockState::from(BlockKind::Water) });
            }
        }
    }
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation { pos: Vec3::new(3.5, 64., 3.5), delta: Vec3::ZERO, look_direction: LookDirection::default() },
        relative: RelativeMovements::all_absolute(),
    });
    let mut pig = make_basic_add_entity(EntityKind::Pig, 50, (4.5, 64., 4.5));
    pig.uuid = uuid::Uuid::from_u128(20_050);
    s.receive_packet(pig);
    for _ in 0..4 {
        s.tick();
    }
    let me = s.entity;
    let world = s.app.world();
    let pig = world
        .get::<azalea_entity::indexing::EntityIdIndex>(me)
        .unwrap()
        .get_by_minecraft_entity(azalea_core::entity_id::MinecraftEntityId(50))
        .expect("the pig is loaded");
    assert_eq!(**world.get::<FluidOnEyes>(me).unwrap(), FluidKind::Water, "control: the bot's own eyes are in water");
    assert_eq!(**world.get::<FluidOnEyes>(pig).unwrap(), FluidKind::Empty, "a copy does not run the fluid system");
    assert!(world.get::<InLoadedChunk>(pig).is_none(), "an unmarked copy gets no chunk flag");

    s.app.world_mut().entity_mut(pig).insert(ReadsInLoadedChunk);
    s.tick();
    assert!(s.app.world().get::<InLoadedChunk>(pig).is_some(), "control: a marked entity gets the flag");
}
