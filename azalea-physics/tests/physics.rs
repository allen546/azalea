use std::sync::Arc;

use azalea_block::{
    BlockState, block_state::BlockStateIntegerRepr, fluid_state::to_or_from_legacy_fluid_level,
    properties::WaterLevel,
};
use azalea_core::{
    entity_id::MinecraftEntityId,
    position::{BlockPos, ChunkPos, Vec3},
    registry_holder::RegistryHolder,
    tick::GameTick,
};
use azalea_entity::{EntityBundle, EntityPlugin, HasClientLoaded, LocalEntity, Physics, Position};
use azalea_physics::PhysicsPlugin;
use azalea_registry::builtin::{BlockKind, EntityKind};
use azalea_world::{Chunk, PartialWorld, World, WorldName, Worlds};
use bevy_app::App;
use parking_lot::RwLock;
use uuid::Uuid;

/// You need an app to spawn entities in the world and do updates.
fn make_test_app() -> App {
    let mut app = App::new();
    app.add_plugins((PhysicsPlugin, EntityPlugin))
        .init_resource::<Worlds>();
    app
}

pub fn insert_overworld(app: &mut App) -> Arc<RwLock<World>> {
    app.world_mut().resource_mut::<Worlds>().get_or_insert(
        WorldName::new("minecraft:overworld"),
        384,
        -64,
        &RegistryHolder::default(),
    )
}

#[test]
fn test_gravity() {
    let mut app = make_test_app();
    let world_lock = insert_overworld(&mut app);
    let mut partial_world = PartialWorld::default();
    // the entity has to be in a loaded chunk for physics to work
    partial_world.chunks.set(
        &ChunkPos { x: 0, z: 0 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );

    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 0.,
                    y: 70.,
                    z: 0.,
                },
                EntityKind::Zombie,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();
    {
        let entity_pos = *app.world_mut().get::<Position>(entity).unwrap();
        // y should start at 70
        assert_eq!(entity_pos.y, 70.);
    }
    app.update();
    app.world_mut().run_schedule(GameTick);
    app.update();
    {
        let entity_pos = *app.world_mut().get::<Position>(entity).unwrap();
        // delta is applied before gravity, so the first tick only sets the
        // delta
        assert_eq!(entity_pos.y, 70.);
        let entity_physics = app.world_mut().get::<Physics>(entity).unwrap();
        assert!(entity_physics.velocity.y < 0.);
    }
    app.world_mut().run_schedule(GameTick);
    app.update();
    {
        let entity_pos = *app.world_mut().get::<Position>(entity).unwrap();
        // the second tick applies the delta to the position, so now it should
        // go down
        assert!(
            entity_pos.y < 70.,
            "Entity y ({}) didn't go down after physics steps",
            entity_pos.y
        );
    }
}
#[test]
fn test_collision() {
    let mut app = make_test_app();
    let world_lock = insert_overworld(&mut app);
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: 0, z: 0 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 0.5,
                    y: 70.,
                    z: 0.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();
    let block_state = partial_world.chunks.set_block_state(
        BlockPos { x: 0, y: 69, z: 0 },
        BlockKind::Stone.into(),
        &world_lock.write().chunks,
    );
    assert!(
        block_state.is_some(),
        "BlockKind state should exist, if this fails that means the chunk wasn't loaded and the block didn't get placed"
    );
    app.update();
    app.world_mut().run_schedule(GameTick);
    app.update();
    {
        let entity_pos = *app.world_mut().get::<Position>(entity).unwrap();
        // delta will change, but it won't move until next tick
        assert_eq!(entity_pos.y, 70.);
        let entity_physics = app.world_mut().get::<Physics>(entity).unwrap();
        assert!(entity_physics.velocity.y < 0.);
    }
    app.world_mut().run_schedule(GameTick);
    app.update();
    {
        let entity_pos = *app.world_mut().get::<Position>(entity).unwrap();
        // the second tick applies the delta to the position, but it also does
        // collision
        assert_eq!(entity_pos.y, 70.);
    }
}

#[test]
fn test_slab_collision() {
    let mut app = make_test_app();
    let world_lock = insert_overworld(&mut app);
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: 0, z: 0 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 0.5,
                    y: 71.,
                    z: 0.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();
    let block_state = partial_world.chunks.set_block_state(
        BlockPos { x: 0, y: 69, z: 0 },
        azalea_block::blocks::StoneSlab {
            kind: azalea_block::properties::SlabKind::Bottom,
            waterlogged: false,
        }
        .into(),
        &world_lock.write().chunks,
    );
    assert!(
        block_state.is_some(),
        "BlockKind state should exist, if this fails that means the chunk wasn't loaded and the block didn't get placed"
    );
    // do a few steps so we fall on the slab
    for _ in 0..20 {
        app.world_mut().run_schedule(GameTick);
        app.update();
    }
    let entity_pos = app.world_mut().get::<Position>(entity).unwrap();
    assert_eq!(entity_pos.y, 69.5);
}

#[test]
fn test_top_slab_collision() {
    let mut app = make_test_app();
    let world_lock = insert_overworld(&mut app);
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: 0, z: 0 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 0.5,
                    y: 71.,
                    z: 0.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();
    let block_state = world_lock.write().chunks.set_block_state(
        BlockPos { x: 0, y: 69, z: 0 },
        azalea_block::blocks::StoneSlab {
            kind: azalea_block::properties::SlabKind::Top,
            waterlogged: false,
        }
        .into(),
    );
    assert!(
        block_state.is_some(),
        "BlockKind state should exist, if this fails that means the chunk wasn't loaded and the block didn't get placed"
    );
    // do a few steps so we fall on the slab
    for _ in 0..20 {
        app.world_mut().run_schedule(GameTick);
        app.update();
    }
    let entity_pos = app.world_mut().get::<Position>(entity).unwrap();
    assert_eq!(entity_pos.y, 70.);
}

#[test]
fn test_weird_wall_collision() {
    let mut app = make_test_app();
    let world_lock = app.world_mut().resource_mut::<Worlds>().get_or_insert(
        WorldName::new("minecraft:overworld"),
        384,
        -64,
        &RegistryHolder::default(),
    );
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: 0, z: 0 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 0.5,
                    y: 73.,
                    z: 0.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();
    let block_state = world_lock.write().chunks.set_block_state(
        BlockPos { x: 0, y: 69, z: 0 },
        azalea_block::blocks::CobblestoneWall {
            east: azalea_block::properties::WallEast::Low,
            north: azalea_block::properties::WallNorth::Low,
            south: azalea_block::properties::WallSouth::Low,
            west: azalea_block::properties::WallWest::Low,
            up: false,
            waterlogged: false,
        }
        .into(),
    );
    assert!(
        block_state.is_some(),
        "BlockKind state should exist, if this fails that means the chunk wasn't loaded and the block didn't get placed"
    );
    // do a few steps so we fall on the wall
    for _ in 0..20 {
        app.world_mut().run_schedule(GameTick);
        app.update();
    }

    let entity_pos = app.world_mut().get::<Position>(entity).unwrap();
    assert_eq!(entity_pos.y, 70.5);
}

#[test]
fn test_negative_coordinates_weird_wall_collision() {
    let mut app = make_test_app();
    let world_lock = app.world_mut().resource_mut::<Worlds>().get_or_insert(
        WorldName::new("minecraft:overworld"),
        384,
        -64,
        &RegistryHolder::default(),
    );
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: -1, z: -1 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: -7.5,
                    y: 73.,
                    z: -7.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();
    let block_state = world_lock.write().chunks.set_block_state(
        BlockPos {
            x: -8,
            y: 69,
            z: -8,
        },
        azalea_block::blocks::CobblestoneWall {
            east: azalea_block::properties::WallEast::Low,
            north: azalea_block::properties::WallNorth::Low,
            south: azalea_block::properties::WallSouth::Low,
            west: azalea_block::properties::WallWest::Low,
            up: false,
            waterlogged: false,
        }
        .into(),
    );
    assert!(
        block_state.is_some(),
        "BlockKind state should exist, if this fails that means the chunk wasn't loaded and the block didn't get placed"
    );
    // do a few steps so we fall on the wall
    for _ in 0..20 {
        app.world_mut().run_schedule(GameTick);
        app.update();
    }

    let entity_pos = app.world_mut().get::<Position>(entity).unwrap();
    assert_eq!(entity_pos.y, 70.5);
}

#[test]
fn spawn_and_unload_world() {
    let mut app = make_test_app();
    let world_lock = app.world_mut().resource_mut::<Worlds>().get_or_insert(
        WorldName::new("minecraft:overworld"),
        384,
        -64,
        &RegistryHolder::default(),
    );
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: -1, z: -1 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let _entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: -7.5,
                    y: 73.,
                    z: -7.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();

    // do a tick
    app.world_mut().run_schedule(GameTick);
    app.update();

    // now unload the partial_world and world_lock
    drop(partial_world);
    drop(world_lock);

    // do another tick
    app.world_mut().run_schedule(GameTick);
    app.update();
}

#[test]
fn test_afk_pool() {
    let mut app = make_test_app();
    let world_lock = insert_overworld(&mut app);
    let mut partial_world = PartialWorld::default();

    partial_world.chunks.set(
        &ChunkPos { x: 0, z: 0 },
        Some(Chunk::default()),
        &mut world_lock.write().chunks,
    );
    let setblock = |x: i32, y: i32, z: i32, b: BlockState| {
        world_lock
            .write()
            .chunks
            .set_block_state(BlockPos { x, y, z }, b);
    };

    let stone = azalea_block::blocks::Stone.into();
    let sign = azalea_block::blocks::OakSign {
        rotation: azalea_block::properties::Rotation::_0,
        waterlogged: false,
    }
    .into();
    let water = |level: u8| {
        BlockState::from(azalea_block::blocks::Water {
            level: WaterLevel::from(to_or_from_legacy_fluid_level(level) as BlockStateIntegerRepr),
        })
    };

    let mut y = 69;

    // first layer
    {
        setblock(1, y, 1, stone);
        setblock(2, y, 1, stone);
        setblock(3, y, 1, stone);
        setblock(3, y, 2, stone);
        setblock(3, y, 3, stone);
        setblock(2, y, 3, stone);
        setblock(1, y, 3, stone);
        setblock(1, y, 2, stone);
    }
    // second layer
    y += 1;
    {
        setblock(1, y, 0, stone);
        setblock(2, y, 0, stone);
        setblock(3, y, 0, stone);

        setblock(0, y, 1, stone);
        setblock(0, y, 2, stone);
        setblock(0, y, 3, stone);

        setblock(1, y, 4, stone);
        setblock(2, y, 4, stone);
        setblock(3, y, 4, stone);

        setblock(4, y, 1, stone);
        setblock(4, y, 2, stone);
        setblock(4, y, 3, stone);

        // middle block
        setblock(2, y, 2, stone);

        // sign
        setblock(1, y, 1, sign);

        // water
        setblock(1, y, 2, water(8));
        setblock(1, y, 3, water(7));
        setblock(2, y, 3, water(6));
        setblock(3, y, 3, water(5));
        setblock(3, y, 2, water(4));
        setblock(3, y, 1, water(3));
        setblock(2, y, 1, water(2));
    }
    // third layer
    y += 1;
    {
        setblock(1, y, 1, water(8));
        setblock(2, y, 1, sign);
    }

    let entity = app
        .world_mut()
        .spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 3.5,
                    y: 70.,
                    z: 1.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ))
        .id();

    let mut blocks_visited = Vec::new();
    let mut loops_done = 0;

    for _ in 0..300 {
        app.world_mut().run_schedule(GameTick);
        app.update();

        let entity_pos = app.world_mut().get::<Position>(entity).unwrap();
        let entity_block_pos = BlockPos::from(entity_pos);

        if !blocks_visited.contains(&entity_block_pos) {
            blocks_visited.push(entity_block_pos);

            if blocks_visited.len() == 8 {
                loops_done += 1;
                blocks_visited.clear();
            }
        }
    }

    assert_eq!(
        blocks_visited.into_iter().collect::<Vec<_>>(),
        vec![
            BlockPos::new(3, 70, 2),
            BlockPos::new(3, 70, 1),
            BlockPos::new(2, 70, 1),
            BlockPos::new(1, 70, 1),
        ]
    );
    assert_eq!(loops_done, 1);
}

// Block movement effects (patch 26). Expected values come from the vanilla
// formulas, worked out by hand: full forward input on the ground gives a
// velocity of 0.1 (speed 0.1, stone friction 0.6); a tick on stone keeps
// 0.6 * 0.91 = 0.546 of the velocity. Each test settles the bot on the floor,
// then walks it.
mod block_effects {
    use azalea_entity::{Jumping, inventory::Inventory};
    use azalea_inventory::ItemStack;
    use azalea_registry::builtin::ItemKind;

    use super::*;

    const SPEED: f64 = 0.1; // walking speed on the ground, full input
    const KEEP: f64 = 0.546; // 0.6 block friction times 0.91

    /// A floor of `floor` at y=69 and `feet` at the bot's feet (y=70), the bot
    /// at rest at (0.5, 70, 0.5), forward input held on every tick.
    pub(super) fn setup(
        floor: BlockKind,
        feet: BlockKind,
        jumping: bool,
        boots: bool,
    ) -> (App, bevy_ecs::entity::Entity, (Arc<RwLock<World>>, PartialWorld)) {
        let mut app = make_test_app();
        let world_lock = insert_overworld(&mut app);
        let mut partial_world = PartialWorld::default();
        partial_world.chunks.set(
            &ChunkPos { x: 0, z: 0 },
            Some(Chunk::default()),
            &mut world_lock.write().chunks,
        );
        for x in 0..=3 {
            for z in 0..=6 {
                partial_world.chunks.set_block_state(
                    BlockPos { x, y: 69, z },
                    floor.into(),
                    &world_lock.write().chunks,
                );
            }
        }
        // the feet block is a whole row so the bot stays inside it for the test
        for z in 0..=6 {
            partial_world.chunks.set_block_state(
                BlockPos { x: 0, y: 70, z },
                feet.into(),
                &world_lock.write().chunks,
            );
        }
        let mut e = app.world_mut().spawn((
            EntityBundle::new(
                Uuid::nil(),
                Vec3 {
                    x: 0.5,
                    y: 70.,
                    z: 0.5,
                },
                EntityKind::Player,
                WorldName::new("minecraft:overworld"),
            ),
            MinecraftEntityId(0),
            LocalEntity,
            HasClientLoaded,
        ));
        e.insert((Jumping(jumping), azalea_physics::client_movement::ClientMovementState::default()));
        if boots {
            let mut inv = Inventory::default();
            *inv.inventory_menu.as_player_mut().armor.get_mut(3).unwrap() =
                ItemStack::from(ItemKind::LeatherBoots);
            e.insert(inv);
        }
        let id = e.id();
        // the world only holds weak chunk references, so the caller keeps both
        (app, id, (world_lock, partial_world))
    }

    pub(super) fn tick(app: &mut App, entity: bevy_ecs::entity::Entity, input: f32) {
        app.world_mut()
            .get_mut::<Physics>(entity)
            .unwrap()
            .z_acceleration = input;
        app.update();
        app.world_mut().run_schedule(GameTick);
        app.update();
    }

    /// Three ticks with no input, so the bot is on the ground and any block
    /// effect has taken hold.
    pub(super) fn settle(app: &mut App, entity: bevy_ecs::entity::Entity) {
        for _ in 0..3 {
            tick(app, entity, 0.);
        }
    }

    /// One tick with full forward input; returns the z displacement.
    pub(super) fn walk_dz(app: &mut App, entity: bevy_ecs::entity::Entity) -> f64 {
        let before = app.world_mut().get::<Position>(entity).unwrap().z;
        tick(app, entity, 1.);
        app.world_mut().get::<Position>(entity).unwrap().z - before
    }

    // Inside a web every move is scaled by 0.25 and the velocity is zeroed, so
    // each walking tick moves 0.25 * 0.1 (speed on stone, full input).
    // (Entity#move, WebBlock#entityInside)
    #[test]
    fn cobweb_scales_every_move() {
        let (mut app, e, _world) = setup(BlockKind::Stone, BlockKind::Cobweb, false, false);
        settle(&mut app, e);
        for _ in 0..2 {
            let dz = walk_dz(&mut app, e);
            assert!((dz - 0.25 * SPEED).abs() < 1e-6, "dz {dz}");
        }
    }

    // SweetBerryBushBlock#entityInside: 0.8 (a float constant)
    #[test]
    fn sweet_berry_bush_scales_every_move() {
        let (mut app, e, _world) = setup(BlockKind::Stone, BlockKind::SweetBerryBush, false, false);
        settle(&mut app, e);
        for _ in 0..2 {
            let dz = walk_dz(&mut app, e);
            assert!((dz - 0.8 * SPEED).abs() < 1e-6, "dz {dz}");
        }
    }

    // The factor scales the velocity after the move. Tick 1 moves 0.1 and
    // keeps 0.4 * 0.546 of it; tick 2 adds a fresh 0.1.
    // (Entity#getBlockSpeedFactor)
    #[test]
    fn soul_sand_and_honey_scale_velocity_after_move() {
        for floor in [BlockKind::SoulSand, BlockKind::HoneyBlock] {
            let (mut app, e, _world) = setup(floor, BlockKind::Air, false, false);
            settle(&mut app, e);
            let dz1 = walk_dz(&mut app, e);
            assert!((dz1 - SPEED).abs() < 1e-6, "{floor:?}: dz1 {dz1}");
            let dz2 = walk_dz(&mut app, e);
            let want = 0.4 * SPEED * KEEP + SPEED;
            assert!((dz2 - want).abs() < 1e-6, "{floor:?}: dz2 {dz2} want {want}");
        }
    }

    // Without leather boots the bot does not climb powdered snow. With them it
    // gets the 0.2 climb velocity, then gravity: (0.2 - 0.08) * 0.98.
    // (PowderSnowBlock#canEntityWalkOnPowderSnow)
    #[test]
    fn powder_snow_climb_needs_leather_boots() {
        let (mut app, e, _world) = setup(BlockKind::Stone, BlockKind::PowderSnow, true, false);
        tick(&mut app, e, 0.);
        let vy = app.world_mut().get::<Physics>(e).unwrap().velocity.y;
        assert!(vy < 0., "no boots: vy {vy}");

        let (mut app, e, _world) = setup(BlockKind::Stone, BlockKind::PowderSnow, true, true);
        tick(&mut app, e, 0.);
        let vy = app.world_mut().get::<Physics>(e).unwrap().velocity.y;
        assert!((vy - 0.12 * 0.98).abs() < 1e-4, "boots: vy {vy}");
    }
}

/// Attributes the server sets for the local player (patch 27). Expected values are worked out
/// from the vanilla formulas (`LivingEntity#travel`, `#jumpFromGround`, `Entity#collide`,
/// `#getBlockSpeedFactor`), not from the fork.
mod server_attributes {
    use azalea_core::attribute_modifier_operation::AttributeModifierOperation;
    use azalea_entity::Attributes;
    use azalea_inventory::components::AttributeModifier;
    use azalea_registry::{builtin::Attribute, identifier::Identifier};
    use bevy_ecs::entity::Entity;

    use super::{block_effects::{settle, setup, tick, walk_dz}, *};

    fn set(app: &mut App, e: Entity, attribute: Attribute, base: f64, mods: &[AttributeModifier]) {
        app.world_mut()
            .get_mut::<Attributes>(e)
            .unwrap()
            .apply_server_snapshot(attribute, base, mods);
    }

    fn vy(app: &mut App, e: Entity) -> f64 {
        app.world_mut().get::<Physics>(e).unwrap().velocity.y
    }

    // Free fall from rest: the move adds -gravity, then drag 0.98: vy = -g * 0.98.
    #[test]
    fn gravity_attribute_sets_the_fall() {
        for g in [0.08, 0.04, 0.0] {
            let (mut app, e, _w) = setup(BlockKind::Air, BlockKind::Air, false, false);
            set(&mut app, e, Attribute::Gravity, g, &[]);
            tick(&mut app, e, 0.);
            let v = vy(&mut app, e);
            assert!((v + g * 0.98f32 as f64).abs() < 1e-9, "g {g}: vy {v}");
        }
    }

    // Jump on stone: vy = jump strength, then the same tick's gravity and drag: (J - 0.08) * 0.98.
    #[test]
    fn jump_strength_attribute_sets_the_jump() {
        for j in [0.42f32 as f64, 0.84, 0.21] {
            let (mut app, e, _w) = setup(BlockKind::Stone, BlockKind::Air, true, false);
            set(&mut app, e, Attribute::JumpStrength, j, &[]);
            // `ai_step`, which jumps, only runs for entities with effects
            app.world_mut().entity_mut(e).insert((azalea_entity::ActiveEffects::default(), azalea_entity::metadata::Sprinting(false)));
            settle(&mut app, e);
            // the first jump tick happened during settle; wait for the next jump (delay 10)
            let mut best = f64::MIN;
            for _ in 0..30 {
                tick(&mut app, e, 0.);
                best = best.max(vy(&mut app, e));
            }
            let want = (j - 0.08) * 0.98f32 as f64;
            assert!((best - want).abs() < 1e-6, "J {j}: best vy {best}, want {want}");
        }
    }

    // A full block in front: the default 0.6 cannot step it, a step height of 1.0 can.
    #[test]
    fn step_height_attribute_lets_the_bot_climb() {
        for (step, climbs) in [(0.6, false), (1.0, true)] {
            let (mut app, e, (world_lock, partial)) = setup(BlockKind::Stone, BlockKind::Air, false, false);
            partial.chunks.set_block_state(
                BlockPos { x: 0, y: 70, z: 3 },
                BlockKind::Stone.into(),
                &world_lock.write().chunks,
            );
            set(&mut app, e, Attribute::StepHeight, step, &[]);
            settle(&mut app, e);
            // the highest point reached; the floor ends at z 7, so the bot may fall later
            let mut top = f64::MIN;
            for _ in 0..60 {
                tick(&mut app, e, 1.);
                top = top.max(app.world_mut().get::<Position>(e).unwrap().y);
            }
            assert_eq!(top > 70.5, climbs, "step {step}: top {top}");
        }
    }

    // A speed modifier of -50% (multiplied total) halves the step: 0.1 * 0.5 per tick on stone.
    #[test]
    fn movement_speed_modifier_changes_the_walk() {
        let (mut app, e, _w) = setup(BlockKind::Stone, BlockKind::Air, false, false);
        let m = AttributeModifier {
            id: Identifier::new("test"),
            amount: -0.5,
            operation: AttributeModifierOperation::AddMultipliedTotal,
        };
        set(&mut app, e, Attribute::MovementSpeed, 0.1f32 as f64, &[m]);
        settle(&mut app, e);
        let dz = walk_dz(&mut app, e);
        assert!((dz - 0.05).abs() < 1e-6, "dz {dz}");
    }

    // `LivingEntity#getBlockSpeedFactor`: lerp(efficiency, 0.4, 1.0) on soul sand.
    #[test]
    fn movement_efficiency_softens_the_speed_factor() {
        for eff in [0.0, 0.5, 1.0] {
            let (mut app, e, _w) = setup(BlockKind::SoulSand, BlockKind::Air, false, false);
            set(&mut app, e, Attribute::MovementEfficiency, eff, &[]);
            settle(&mut app, e);
            let _ = walk_dz(&mut app, e);
            let dz2 = walk_dz(&mut app, e);
            let factor = 0.4 + eff * 0.6;
            let want = 0.1 * 0.546 * factor + 0.1;
            assert!((dz2 - want).abs() < 1e-6, "eff {eff}: dz2 {dz2} want {want}");
        }
    }
}

/// Levitation and Slow Falling (patch 30). Expected values are worked out from the vanilla
/// formulas (`LivingEntity#travelInAir`, `#getEffectiveGravity`, `#getFluidFallingAdjustedMovement`),
/// not from the fork: the drag is the float 0.98F widened to a double, and `aiStep` zeroes a
/// vertical speed under 0.003 before the travel.
mod levitation_slow_falling {
    use azalea_entity::{ActiveEffects, MobEffectData};
    use azalea_registry::builtin::MobEffect;
    use bevy_ecs::entity::Entity;

    use super::{block_effects::{setup, tick}, *};

    const DRAG: f64 = 0.9800000190734863;

    fn give(app: &mut App, e: Entity, effect: MobEffect, amplifier: i32) {
        // `ai_step` (the under-0.003 zeroing) runs only for entities with `Sprinting`
        app.world_mut().entity_mut(e).insert(azalea_entity::metadata::Sprinting(false));
        app.world_mut().get_mut::<ActiveEffects>(e).unwrap().insert(
            effect,
            MobEffectData { amplifier, duration: 100, ..Default::default() },
        );
    }

    fn vy(app: &App, e: Entity) -> f64 {
        app.world().get::<Physics>(e).unwrap().velocity.y
    }

    fn set_vy(app: &mut App, e: Entity, v: f64) {
        app.world_mut().get_mut::<Physics>(e).unwrap().velocity.y = v;
    }

    /// The vanilla vertical speed after one air tick, `start` being the speed before it.
    fn air_step(start: f64, levitation: Option<i32>, slow_falling: bool) -> f64 {
        let v = if start.abs() < 0.003 { 0.0 } else { start };
        let d = match levitation {
            Some(a) => v + (0.05 * (a + 1) as f64 - v) * 0.2,
            None => v - if v <= 0.0 && slow_falling { 0.08f64.min(0.01) } else { 0.08 },
        };
        d * DRAG
    }

    fn check_air(levitation: Option<i32>, slow_falling: bool, start: f64) {
        let (mut app, e, _w) = setup(BlockKind::Air, BlockKind::Air, false, false);
        if let Some(a) = levitation {
            give(&mut app, e, MobEffect::Levitation, a);
        }
        if slow_falling {
            give(&mut app, e, MobEffect::SlowFalling, 0);
        }
        set_vy(&mut app, e, start);
        let mut want = start;
        for n in 0..30 {
            tick(&mut app, e, 0.);
            want = air_step(want, levitation, slow_falling);
            let got = vy(&app, e);
            assert!((got - want).abs() < 1e-9, "tick {n}: got {got} want {want}");
        }
    }

    #[test]
    fn levitation_i_and_ii_from_rest() {
        check_air(Some(0), false, 0.0);
        check_air(Some(1), false, 0.0);
    }

    #[test]
    fn levitation_from_a_fall_climbs_toward_the_target() {
        check_air(Some(0), false, -0.8);
        check_air(Some(1), false, -1.5);
    }

    #[test]
    fn slow_falling_caps_gravity_in_air() {
        check_air(None, true, 0.0);
        check_air(None, true, -0.5);
    }

    // With an upward speed gravity is unchanged (the speed is read after the move).
    #[test]
    fn slow_falling_keeps_gravity_on_the_way_up() {
        check_air(None, true, 0.3);
        let (mut app, e, _w) = setup(BlockKind::Air, BlockKind::Air, false, false);
        give(&mut app, e, MobEffect::SlowFalling, 0);
        set_vy(&mut app, e, 0.3);
        tick(&mut app, e, 0.);
        assert!((vy(&app, e) - (0.3 - 0.08) * DRAG).abs() < 1e-9);
    }

    // Fluid falling adjustment: with gravity g and a speed near 0 the bot sinks at -0.003 when
    // |0 - g/16| < 0.003 (g 0.01), and at -g/16 otherwise (g 0.08 gives -0.005).
    #[test]
    fn slow_falling_caps_gravity_in_water_and_levitation_does_not_act() {
        let sink = |slow: bool, lev: bool| {
            let (mut app, e, _w) = setup(BlockKind::Air, BlockKind::Water, false, false);
            if slow {
                give(&mut app, e, MobEffect::SlowFalling, 0);
            }
            if lev {
                give(&mut app, e, MobEffect::Levitation, 0);
            }
            tick(&mut app, e, 0.);
            vy(&app, e)
        };
        assert!((sink(false, false) + 0.005).abs() < 1e-9, "{}", sink(false, false));
        assert!((sink(true, false) + 0.003).abs() < 1e-9, "{}", sink(true, false));
        assert!((sink(false, true) + 0.005).abs() < 1e-9, "{}", sink(false, true));
    }

    // Slow Falling in lava travel: gravity/4 is subtracted after the 0.5 scaling.
    #[test]
    fn effective_gravity_formula() {
        use azalea_physics::travel::{effective_gravity, levitation_vertical_speed};
        assert_eq!(effective_gravity(0.08, 0.0, true), 0.01);
        assert_eq!(effective_gravity(0.08, -1.0, true), 0.01);
        assert_eq!(effective_gravity(0.08, 0.1, true), 0.08);
        assert_eq!(effective_gravity(0.005, -1.0, true), 0.005);
        assert_eq!(effective_gravity(0.08, -1.0, false), 0.08);
        assert!((levitation_vertical_speed(0.0, 0) - 0.01).abs() < 1e-12);
        assert!((levitation_vertical_speed(0.0, 1) - 0.02).abs() < 1e-12);
        assert!((levitation_vertical_speed(-1.0, 1) - (-1.0 + 1.1 * 0.2)).abs() < 1e-12);
    }
}
