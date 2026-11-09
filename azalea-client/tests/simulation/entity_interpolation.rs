use azalea_client::test_utils::prelude::*;
use azalea_core::{
    delta::PositionDelta8,
    entity_id::MinecraftEntityId,
    position::{ChunkPos, Vec3},
};
use azalea_entity::{
    Position,
    indexing::EntityIdIndex,
};
use azalea_physics::collision::entity_collisions::LastBoundingBox;
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundMoveEntityPos, ClientboundTeleportEntity},
    },
};
use azalea_registry::builtin::EntityKind;
use bevy_ecs::prelude::*;
use uuid::Uuid;

fn setup() -> Simulation {
    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.receive_packet(default_login_packet());
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    simulation.tick();
    simulation
}

fn add(simulation: &mut Simulation, kind: EntityKind, id: i32, pos: impl Into<Vec3>) -> Entity {
    let mut p = make_basic_add_entity(kind, id, pos);
    p.uuid = Uuid::from_u128(10_000 + id as u128);
    simulation.receive_packet(p);
    simulation.tick();
    entity_of(simulation, id)
}

fn entity_of(simulation: &Simulation, id: i32) -> Entity {
    let mut found = None;
    simulation.with_component::<EntityIdIndex>(|ids| {
        found = ids.get_by_minecraft_entity(MinecraftEntityId(id));
    });
    found.expect("entity is loaded")
}

fn x_of(simulation: &Simulation, e: Entity) -> f64 {
    simulation.app.world().get::<Position>(e).unwrap().x
}

fn teleport(simulation: &mut Simulation, id: i32, pos: Vec3) {
    simulation.receive_packet(ClientboundTeleportEntity {
        id: MinecraftEntityId(id),
        change: PositionMoveRotation {
            pos,
            delta: Vec3::ZERO,
            look_direction: Default::default(),
        },
        relative: RelativeMovements::all_absolute(),
        on_ground: true,
    });
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: interpolation of other entities)
#[test]
fn test_entity_moves_in_three_steps_and_its_box_follows() {
    let _lock = init();
    let mut simulation = setup();
    let cow = add(&mut simulation, EntityKind::Cow, 123, (0.5, 64., 0.5));

    teleport(&mut simulation, 123, Vec3::new(3.5, 64., 0.5));
    simulation.tick();
    // 1/3 of the gap in the tick the packet arrives, and the box moved with it in the same tick.
    assert!((x_of(&simulation, cow) - 1.5).abs() < 1e-9);
    let centre = |s: &Simulation| {
        let b = &s.app.world().get::<LastBoundingBox>(cow).unwrap().0;
        (b.min.x + b.max.x) / 2.
    };
    assert!((centre(&simulation) - 1.5).abs() < 1e-6, "box centre {}", centre(&simulation));

    // The same target again is ignored: the run does not restart.
    teleport(&mut simulation, 123, Vec3::new(3.5, 64., 0.5));
    simulation.tick();
    assert!((x_of(&simulation, cow) - 2.5).abs() < 1e-9);
    simulation.tick();
    assert!((x_of(&simulation, cow) - 3.5).abs() < 1e-9);
    assert!((centre(&simulation) - 3.5).abs() < 1e-6);
    // Then it stays.
    simulation.tick();
    assert!((x_of(&simulation, cow) - 3.5).abs() < 1e-9);
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: interpolation of other entities)
#[test]
fn test_relative_move_decodes_against_the_target_not_the_smoothed_position() {
    let _lock = init();
    let mut simulation = setup();
    let cow = add(&mut simulation, EntityKind::Cow, 123, (0.5, 64., 0.5));

    // 3 blocks, then 3 more before the run ends: the second delta is from the first target.
    simulation.receive_packet(ClientboundMoveEntityPos {
        entity_id: MinecraftEntityId(123),
        delta: PositionDelta8 { xa: 12288, ya: 0, za: 0 },
        on_ground: true,
    });
    simulation.tick();
    assert!((x_of(&simulation, cow) - 1.5).abs() < 1e-9);
    simulation.receive_packet(ClientboundMoveEntityPos {
        entity_id: MinecraftEntityId(123),
        delta: PositionDelta8 { xa: 12288, ya: 0, za: 0 },
        on_ground: true,
    });
    simulation.tick();
    // Target 6.5 from 1.5, three fresh steps: +5/3.
    assert!((x_of(&simulation, cow) - (1.5 + 5.0 / 3.0)).abs() < 1e-9);
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: interpolation of other entities)
#[test]
fn test_snaps_over_64_blocks_on_add_and_by_kind() {
    let _lock = init();
    let mut simulation = setup();
    let cow = add(&mut simulation, EntityKind::Cow, 123, (0.5, 64., 0.5));

    // Over 64 blocks: at once.
    teleport(&mut simulation, 123, Vec3::new(100.5, 64., 0.5));
    simulation.update();
    assert_eq!(x_of(&simulation, cow), 100.5);

    // An add packet for an entity already known sets the position at once and ends a run.
    teleport(&mut simulation, 123, Vec3::new(103.5, 64., 0.5));
    simulation.tick();
    assert!((x_of(&simulation, cow) - 101.5).abs() < 1e-9);
    let mut p = make_basic_add_entity(EntityKind::Cow, 123, (50.5, 64., 0.5));
    p.uuid = Uuid::from_u128(10_123);
    simulation.receive_packet(p);
    simulation.tick();
    assert_eq!(x_of(&simulation, cow), 50.5);
    simulation.tick();
    assert_eq!(x_of(&simulation, cow), 50.5);

    // The per-kind table: a handler of 3 steps for living entities (not the shulker), boats,
    // experience orbs, fishing hooks and minecarts; none for items, projectiles and displays.
    let table = [
        (EntityKind::Cow, true),
        (EntityKind::OakBoat, true),
        (EntityKind::ExperienceOrb, true),
        (EntityKind::FishingBobber, true),
        (EntityKind::Minecart, true),
        (EntityKind::Shulker, false),
        (EntityKind::Item, false),
        (EntityKind::Arrow, false),
        (EntityKind::TextDisplay, false),
    ];
    for (i, (kind, smooth)) in table.into_iter().enumerate() {
        let id = 200 + i as i32;
        let e = add(&mut simulation, kind, id, (0.5, 64., 0.5));
        teleport(&mut simulation, id, Vec3::new(3.5, 64., 0.5));
        simulation.tick();
        let x = x_of(&simulation, e);
        let want = if smooth { 1.5 } else { 3.5 };
        assert!((x - want).abs() < 1e-9, "{kind:?}: x {x}, wanted {want}");
    }
}
