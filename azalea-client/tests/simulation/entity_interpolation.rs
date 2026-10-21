use azalea_client::test_utils::prelude::*;
use azalea_core::{
    delta::PositionDelta8,
    entity_id::MinecraftEntityId,
    position::{ChunkPos, Vec3},
};
use azalea_entity::{
    LocalEntity, Position,
    indexing::EntityIdIndex,
    interpolation::{PEER_CAP, PeerView},
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

// Add players and mark them as bots (local entities). A bot falls under its own physics in this
// empty world, so tests add them in one tick and keep the ticks after that few.
fn make_bots(simulation: &mut Simulation, bots: &[(i32, Vec3)]) -> Vec<Entity> {
    let origin = *simulation.component::<Position>();
    for (id, offset) in bots {
        let mut p = make_basic_add_entity(EntityKind::Player, *id, origin + *offset);
        p.uuid = Uuid::from_u128(10_000 + *id as u128);
        simulation.receive_packet(p);
    }
    simulation.tick();
    let found: Vec<Entity> = bots.iter().map(|(id, _)| entity_of(simulation, *id)).collect();
    for e in &found {
        simulation.app.world_mut().entity_mut(*e).insert(LocalEntity);
    }
    found
}

fn view_len(simulation: &Simulation) -> usize {
    let mut n = 0;
    simulation.with_component::<PeerView>(|v| n = v.len());
    n
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: the bot-to-bot view)
#[test]
fn test_bot_to_bot_view_is_built_from_packets_and_does_not_touch_the_other_bot() {
    let _lock = init();
    let mut simulation = setup();
    let bot = make_bots(&mut simulation, &[(300, Vec3::new(2., 0., 0.))])[0];
    simulation.tick();
    let truth = *simulation.app.world().get::<Position>(bot).unwrap();

    simulation.with_component::<PeerView>(|v| {
        assert_eq!(v.len(), 1);
        // Seeded from the other bot's simulated position, rounded as the server's packets round.
        assert!((v.get(bot).unwrap().position.x - truth.x).abs() < 1e-3);
    });

    // The server moves it 3 blocks: this bot's view moves in steps, the other bot's own position
    // (written by its own client) is not touched by the packet.
    let start = simulation.with_peer(bot);
    teleport(&mut simulation, 300, start + Vec3::new(3., 0., 0.));
    for want in [1.0, 2.0, 3.0] {
        simulation.tick();
        let x = simulation.with_peer(bot).x;
        assert!((x - (start.x + want)).abs() < 1e-3, "view x {x}, wanted {}", start.x + want);
    }
}

trait WithPeer {
    fn with_peer(&self, bot: Entity) -> Vec3;
}
impl WithPeer for Simulation {
    fn with_peer(&self, bot: Entity) -> Vec3 {
        let mut p = Vec3::ZERO;
        self.with_component::<PeerView>(|v| p = v.get(bot).expect("in the view").position);
        p
    }
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: the bot-to-bot view)
#[test]
fn test_bot_view_radius_cap_aim_target_and_counter() {
    let _lock = init();
    let mut simulation = setup();

    // A bot 20 blocks away is not kept.
    let far = make_bots(&mut simulation, &[(400, Vec3::new(20., 0., 0.))])[0];
    simulation.tick();
    assert_eq!(view_len(&simulation), 0);

    // It comes within 16 blocks and is kept; going out drops it.
    let here = *simulation.component::<Position>();
    let far_now = *simulation.app.world().get::<Position>(far).unwrap();
    simulation.app.world_mut().entity_mut(far).insert(Position::new(Vec3::new(here.x + 10., far_now.y, far_now.z)));
    simulation.tick();
    assert_eq!(view_len(&simulation), 1);
    simulation.app.world_mut().entity_mut(far).insert(Position::new(Vec3::new(here.x + 17., far_now.y, far_now.z)));
    simulation.tick();
    assert_eq!(view_len(&simulation), 0);

    // 40 bots within 15 blocks: the view holds the 32 nearest and the aim target.
    let specs: Vec<(i32, Vec3)> =
        (0..40).map(|i| (500 + i, Vec3::new(1.0 + i as f64 * 0.35, 0., 0.))).collect();
    let bots = make_bots(&mut simulation, &specs);
    let farthest = *bots.last().unwrap();
    simulation.with_component_mut::<PeerView>(|v| v.set_aim_target(farthest));
    simulation.tick();
    simulation.with_component::<PeerView>(|v| {
        assert_eq!(v.len(), PEER_CAP);
        assert_eq!(v.peak(), PEER_CAP);
        assert_eq!(v.cap_hits(), 1, "the cap is logged once per episode");
        assert!(v.contains(farthest), "the aim target is kept");
        assert!(v.contains(bots[0]), "the nearest is kept");
        assert!(!v.contains(bots[38]), "a far one is out");
    });
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: the crosshair ray reads the observer's view)
#[test]
fn test_ray_hits_a_bot_where_this_bot_sees_it() {
    use azalea_client::interact::pick::HitResultComponent;
    use azalea_core::hit_result::HitResult;

    let _lock = init();
    let mut simulation = setup();
    let bot = make_bots(&mut simulation, &[(600, Vec3::new(0., 0., 3.))])[0];
    simulation.tick();
    let hit = |s: &Simulation| s.component::<HitResultComponent>();
    assert!(matches!(*hit(&simulation), HitResult::Entity(ref r) if r.entity == bot), "straight ahead");

    // The server moves the bot 5 blocks sideways; its own position (and box) stay, but this bot
    // sees it 1/3 of the way there, which is off the ray.
    let at = simulation.with_peer(bot);
    teleport(&mut simulation, 600, at + Vec3::new(5., 0., 0.));
    simulation.tick();
    simulation.tick();
    let truth = simulation.app.world().get::<Position>(bot).unwrap().x;
    assert!((truth - (*simulation.component::<Position>()).x).abs() < 1e-6, "the bot itself did not move");
    assert!(
        !matches!(*hit(&simulation), HitResult::Entity(ref r) if r.entity == bot),
        "the ray must not hit the bot at its own position"
    );
}
