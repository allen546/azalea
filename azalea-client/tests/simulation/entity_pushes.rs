//! Pushes on the bot from other entities, the team store and the per-bot tick order (patch 32).
//! The expected numbers are written from the vanilla text, not read back from the fork.

use azalea_auth::game_profile::GameProfile;
use azalea_chat::FormattedText;
use azalea_client::{
    local_player::{LocalGameMode, TabList},
    player::{GameProfileComponent, PlayerInfo},
    test_utils::prelude::*,
};
use azalea_core::{
    delta::LpVec3,
    entity_id::MinecraftEntityId,
    game_type::GameMode,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_entity::{
    Dead, LocalEntity, LookDirection, Physics, Position,
    indexing::EntityIdIndex,
    interpolation::PeerView,
    metadata::{Health, SleepingPos},
    push::PushRecord,
    teams::{CollisionRule, Scoreboard},
};
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{
            ClientboundBlockUpdate, ClientboundPlayerPosition, ClientboundRemoveEntities,
            ClientboundSetEntityMotion, ClientboundSetPlayerTeam, ClientboundTeleportEntity,
            c_set_player_team::{self, Method, Parameters},
        },
    },
};
use azalea_registry::{DataRegistry, builtin::{BlockKind, EntityKind}, data::DimensionKind, identifier::Identifier};
use bevy_ecs::prelude::*;
use uuid::Uuid;

/// The bot stands at (8.5, 64, 8.5) on a stone floor. A pusher at x offset `d` has its centre at
/// 8.5 + d.
const BX: f64 = 8.5;
const BZ: f64 = 8.5;

fn setup() -> Simulation {
    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.receive_packet(default_login_packet());
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    for x in 2..=14 {
        for z in 2..=14 {
            simulation.receive_packet(ClientboundBlockUpdate {
                pos: BlockPos::new(x, 63, z),
                block_state: BlockKind::Stone.into(),
            });
        }
    }
    simulation.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: Vec3::new(BX, 64., BZ),
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    for _ in 0..4 {
        simulation.tick();
    }
    simulation
}

fn add_at(simulation: &mut Simulation, kind: EntityKind, id: i32, pos: impl Into<Vec3>) -> Entity {
    let mut p = make_basic_add_entity(kind, id, pos);
    p.uuid = Uuid::from_u128(20_000 + id as u128);
    simulation.receive_packet(p);
    simulation.update();
    entity_of(simulation, id)
}

fn entity_of(simulation: &Simulation, id: i32) -> Entity {
    let mut found = None;
    simulation.with_component::<EntityIdIndex>(|ids| {
        found = ids.get_by_minecraft_entity(MinecraftEntityId(id));
    });
    found.expect("entity is loaded")
}

fn velocity(simulation: &Simulation) -> Vec3 {
    simulation.component::<Physics>().velocity
}
fn bot_pos(simulation: &Simulation) -> Vec3 {
    *simulation.component::<Position>()
}
fn record(simulation: &Simulation) -> PushRecord {
    simulation.component::<PushRecord>()
}

/// Add an entity with the given x offset from the bot and tick once (the add and the first
/// push, if any, share that tick). Returns the velocity change in that tick.
fn push_dx(kind: EntityKind, id: i32, dx: f64, dz: f64) -> (f64, f64) {
    let _lock = init();
    let mut simulation = setup();
    assert_eq!(velocity(&simulation).x, 0.0, "the bot is at rest");
    add_at(&mut simulation, kind, id, (BX + dx, 64., BZ + dz));
    simulation.tick();
    let v = velocity(&simulation);
    (v.x, v.z)
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[test]
fn test_a_cow_after_the_bot_pushes_after_the_bot_moved() {
    let _lock = init();
    let mut simulation = setup();
    let x0 = bot_pos(&simulation).x;
    // The cow joins after the bot, so it ticks after it: the push lands after this tick's move.
    add_at(&mut simulation, EntityKind::Cow, 50, (BX + 0.3, 64., BZ));
    simulation.tick();
    let want = -(0.05f32 as f64) * 0.3f64.sqrt();
    assert!((velocity(&simulation).x - want).abs() < 1e-9, "{}", velocity(&simulation).x);
    assert_eq!(bot_pos(&simulation).x, x0, "no move yet: the push came after the bot's tick");
    assert_eq!(record(&simulation).total, 1);
    // The next tick's travel uses it.
    simulation.tick();
    assert!((bot_pos(&simulation).x - (x0 + want)).abs() < 1e-9, "{}", bot_pos(&simulation).x - x0);
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[test]
fn test_a_cow_before_the_bot_pushes_into_this_ticks_travel() {
    let _lock = init();
    let mut simulation = setup();
    let x0 = bot_pos(&simulation).x;
    add_at(&mut simulation, EntityKind::Cow, 50, (BX + 0.3, 64., BZ));
    // Make the bot the last entry, as a respawn in the same dimension does.
    let bot = simulation.entity;
    simulation.app.world_mut().get_mut::<EntityIdIndex>(bot).unwrap().tick_order_mut().assign(bot);
    simulation.tick();
    let want = -(0.05f32 as f64) * 0.3f64.sqrt();
    assert!((bot_pos(&simulation).x - (x0 + want)).abs() < 1e-9, "moved in the same tick: {}", bot_pos(&simulation).x - x0);
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[test]
fn test_push_amount_bands_through_the_whole_stack() {
    // Offsets 0.005 (nothing), 0.01, 0.3, 0.74 with a cow (strict box test: 0.75 is the edge);
    // 1.0 and 2.2 with a ghast (4 blocks wide).
    let a = (0.05f32 as f64, 0.01f32 as f64);
    let cases = [
        (EntityKind::Cow, 0.005, 0.0),
        (EntityKind::Cow, 0.01, -0.1 * a.0),
        (EntityKind::Cow, 0.3, -a.0 * 0.3f64.sqrt()),
        (EntityKind::Cow, 0.74, -a.0 * 0.74f64.sqrt()),
        (EntityKind::Ghast, 1.0, -a.0),
        (EntityKind::Ghast, 2.2, -a.0),
    ];
    for (i, (kind, dx, want)) in cases.into_iter().enumerate() {
        let (vx, vz) = push_dx(kind, 60 + i as i32, dx, 0.0);
        assert!((vx - want).abs() < 1e-9, "{kind:?} at {dx}: {vx}, wanted {want}");
        assert_eq!(vz, 0.0);
    }
    // Each axis and a diagonal: away from the pusher, the larger axis sets the scale.
    let (vx, vz) = push_dx(EntityKind::Cow, 70, 0.0, -0.3);
    assert_eq!(vx, 0.0);
    assert!((vz - a.0 * 0.3f64.sqrt()).abs() < 1e-9, "{vz}");
    let (vx, vz) = push_dx(EntityKind::Cow, 71, 0.4, -0.2);
    let s = a.0 / 0.4f64.sqrt();
    assert!((vx + 0.4 * s).abs() < 1e-9 && (vz - 0.2 * s).abs() < 1e-9, "{vx} {vz}");
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[test]
fn test_exceptions_and_non_pushers() {
    let none = [
        EntityKind::ArmorStand,
        EntityKind::Bat,
        EntityKind::Parrot,
        EntityKind::Vex,
        EntityKind::EnderDragon,
        EntityKind::Minecart,
        EntityKind::Item,
        EntityKind::Arrow,
        EntityKind::ItemFrame,
    ];
    for (i, kind) in none.into_iter().enumerate() {
        let (vx, _) = push_dx(kind, 80 + i as i32, 0.3, 0.0);
        assert_eq!(vx, 0.0, "{kind:?} must not push");
    }
    // A horse (ridden or not: the rider is not tracked) and a shulker push like any living entity.
    for (i, kind) in [EntityKind::Horse, EntityKind::Shulker, EntityKind::Player].into_iter().enumerate() {
        let (vx, _) = push_dx(kind, 100 + i as i32, 0.3, 0.0);
        assert!(vx < -0.02, "{kind:?} pushes: {vx}");
    }
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[test]
fn test_a_dying_mob_still_pushes_and_a_spectator_remote_player_does_not() {
    let _lock = init();
    let mut simulation = setup();
    let cow = add_at(&mut simulation, EntityKind::Cow, 110, (BX + 0.3, 64., BZ));
    simulation.app.world_mut().entity_mut(cow).insert((Health(0.0), Dead));
    simulation.tick();
    assert!(velocity(&simulation).x < -0.02, "a dying mob pushes until the server removes it");

    // A remote player in spectator mode has noPhysics.
    let _lock2 = ();
    let mut simulation = setup();
    let uuid = Uuid::from_u128(20_111);
    simulation.with_component_mut::<TabList>(|t| {
        t.insert(
            uuid,
            PlayerInfo {
                profile: GameProfile::new(uuid, "ghost".to_owned()),
                uuid,
                gamemode: GameMode::Spectator,
                latency: 0,
                display_name: None::<Box<FormattedText>>,
            },
        );
    });
    add_at(&mut simulation, EntityKind::Player, 111, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0);
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[test]
fn test_target_conditions_spectator_dead_climbable_sleeping() {
    // The bot is a spectator.
    let _lock = init();
    let mut simulation = setup();
    simulation.with_component_mut::<LocalGameMode>(|g| g.current = GameMode::Spectator);
    add_at(&mut simulation, EntityKind::Cow, 120, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0, "spectator");

    // Sleeping: not pushed by a living entity, pushed by a boat.
    let mut simulation = setup();
    simulation.app.world_mut().entity_mut(simulation.entity).insert(SleepingPos(Some(BlockPos::new(8, 64, 8))));
    add_at(&mut simulation, EntityKind::Cow, 121, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0, "sleeping vs a living pusher");
    let mut simulation = setup();
    simulation.app.world_mut().entity_mut(simulation.entity).insert(SleepingPos(Some(BlockPos::new(8, 64, 8))));
    add_at(&mut simulation, EntityKind::OakBoat, 122, (BX + 1.0, 64., BZ));
    simulation.tick();
    assert!(velocity(&simulation).x < 0.0, "sleeping vs a boat: {}", velocity(&simulation).x);

    // On a climbable: a ladder where the bot stands.
    let mut simulation = setup();
    simulation.receive_packet(ClientboundBlockUpdate {
        pos: BlockPos::new(8, 64, 8),
        block_state: BlockKind::Ladder.into(),
    });
    simulation.tick();
    add_at(&mut simulation, EntityKind::Cow, 123, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0, "on a climbable");
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: boats)
#[test]
fn test_boat_band_and_height_rule() {
    // Boat half-width 0.6875; the bot's box ends at x = 8.8. A gap of g puts the boat centre at
    // 8.8 + g + 0.6875.
    let at = |gap: f64| BX + 0.3 + gap + 0.6875;
    let run = |id: i32, gap: f64, y: f64| {
        let _lock = init();
        let mut simulation = setup();
        add_at(&mut simulation, EntityKind::OakBoat, id, (at(gap), y, BZ));
        simulation.tick();
        velocity(&simulation).x
    };
    // Away from the boat means -x, since the boat is at +x.
    assert!(run(131, 0.15, 64.0) < -0.02, "gap 0.15: pushed");
    assert_eq!(run(132, 0.25, 64.0), 0.0, "gap 0.25: outside the band");
    assert!(run(133, -0.1, 64.0) < -0.02, "overlap: pushed");
    assert!(run(134, 0.1, 64.2) < -0.02, "boat bottom above the bot's feet: pushed");
    assert_eq!(run(135, 0.1, 63.5), 0.0, "bot's feet above the boat's bottom: not pushed");
}

fn team_packet(name: &str, method: Method) -> ClientboundSetPlayerTeam {
    ClientboundSetPlayerTeam { name: name.to_owned(), method }
}
fn params(rule: c_set_player_team::CollisionRule) -> Parameters {
    Parameters {
        display_name: FormattedText::default(),
        options: 0,
        nametag_visibility: c_set_player_team::NameTagVisibility::Always,
        collision_rule: rule,
        color: azalea_chat::style::ChatFormatting::White,
        player_prefix: FormattedText::default(),
        player_suffix: FormattedText::default(),
    }
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: teams)
#[test]
fn test_team_packets_fill_the_scoreboard() {
    use c_set_player_team::CollisionRule as W;
    let _lock = init();
    let mut simulation = setup();
    let board = |s: &Simulation| s.component::<Scoreboard>();

    // An action on an unknown team is ignored.
    simulation.receive_packet(team_packet("t", Method::Join(vec!["a".into()])));
    simulation.receive_packet(team_packet("t", Method::Change(params(W::Never))));
    simulation.receive_packet(team_packet("t", Method::Remove));
    simulation.update();
    assert!(board(&simulation).is_empty());

    simulation.receive_packet(team_packet("t", Method::Add((params(W::PushOwnTeam), vec!["a".into()]))));
    simulation.receive_packet(team_packet("u", Method::Add((params(W::Always), vec![]))));
    simulation.update();
    assert_eq!(board(&simulation).team_ref("a").unwrap().rule, CollisionRule::PushOwnTeam);
    // Join replaces the old team.
    simulation.receive_packet(team_packet("u", Method::Join(vec!["a".into()])));
    simulation.update();
    assert_eq!(board(&simulation).team_of("a"), Some("u"));
    assert_eq!(board(&simulation).members_of("t"), 0);
    // Leave of a name that is on another team is ignored.
    simulation.receive_packet(team_packet("t", Method::Leave(vec!["a".into()])));
    simulation.update();
    assert_eq!(board(&simulation).team_of("a"), Some("u"));
    simulation.receive_packet(team_packet("u", Method::Leave(vec!["a".into()])));
    simulation.update();
    assert_eq!(board(&simulation).team_of("a"), None);
    // Change sets the rule, Remove drops the team and its members.
    simulation.receive_packet(team_packet("u", Method::Join(vec!["b".into()])));
    simulation.receive_packet(team_packet("u", Method::Change(params(W::Never))));
    simulation.update();
    assert_eq!(board(&simulation).team_ref("b").unwrap().rule, CollisionRule::Never);
    simulation.receive_packet(team_packet("u", Method::Remove));
    simulation.update();
    assert_eq!(board(&simulation).team_of("b"), None);
    assert!(!board(&simulation).has_team("u"));
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: teams)
#[test]
fn test_collision_rules_decide_who_pushes_the_bot() {
    use c_set_player_team::CollisionRule as W;
    let _lock = init();
    let mut simulation = setup();
    let cow_name = Uuid::from_u128(20_140).to_string();
    // The cow is in a team that never collides: no push. Mobs are keyed by UUID text.
    simulation.receive_packet(team_packet("t", Method::Add((params(W::Never), vec![cow_name.clone()]))));
    add_at(&mut simulation, EntityKind::Cow, 140, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0, "pusher on a never team");
    // Leave: pushes resume.
    simulation.receive_packet(team_packet("t", Method::Leave(vec![cow_name.clone()])));
    simulation.tick();
    assert!(velocity(&simulation).x < -0.02, "after leave");

    // Only the bot (keyed by name) on a never team: no push either.
    let mut simulation = setup();
    simulation.receive_packet(team_packet("t", Method::Add((params(W::Never), vec!["azalea".into()]))));
    add_at(&mut simulation, EntityKind::Cow, 141, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0, "target on a never team");

    // Both in one pushOwnTeam team: allied, no push. In two teams of that rule: they push.
    let mut simulation = setup();
    let cow2 = Uuid::from_u128(20_142).to_string();
    simulation.receive_packet(team_packet("t", Method::Add((params(W::PushOwnTeam), vec!["azalea".into(), cow2.clone()]))));
    add_at(&mut simulation, EntityKind::Cow, 142, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0, "allied and pushOwnTeam");
    simulation.receive_packet(team_packet("t", Method::Leave(vec![cow2.clone()])));
    simulation.receive_packet(team_packet("v", Method::Add((params(W::PushOwnTeam), vec![cow2]))));
    simulation.tick();
    assert!(velocity(&simulation).x < -0.02, "different teams, pushOwnTeam only blocks allies");
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: tick order)
#[test]
fn test_tick_order_respawn_dimension_change_and_re_add() {
    let _lock = init();
    let mut simulation = setup();
    let bot = simulation.entity;
    let cow = add_at(&mut simulation, EntityKind::Cow, 150, (BX + 3., 64., BZ));
    let order = |s: &Simulation, e: Entity| -> Option<u64> {
        s.app.world().get::<EntityIdIndex>(s.entity).unwrap().tick_order().number(e)
    };
    assert_eq!(order(&simulation, bot), Some(0), "the bot is first at login");
    assert!(order(&simulation, cow).unwrap() > 0);

    // Remove and add again: the cow moves to the end.
    let other = add_at(&mut simulation, EntityKind::Pig, 151, (BX + 4., 64., BZ));
    let before = order(&simulation, cow).unwrap();
    assert!(order(&simulation, other).unwrap() > before);
    simulation.receive_packet(ClientboundRemoveEntities { entity_ids: vec![MinecraftEntityId(150)] });
    simulation.update();
    assert_eq!(order(&simulation, cow), None);
    let cow = add_at(&mut simulation, EntityKind::Cow, 150, (BX + 3., 64., BZ));
    assert!(order(&simulation, cow).unwrap() > order(&simulation, other).unwrap());

    // A respawn in the same dimension: the bot goes to the end of the numbers given so far. (The
    // fork unloads every entity at a respawn and the server sends them again afterwards, so
    // they come after the bot.)
    let newest = order(&simulation, cow).unwrap().max(order(&simulation, other).unwrap());
    simulation.receive_packet(make_basic_respawn_packet(
        DimensionKind::new_raw(0),
        Identifier::new("minecraft:overworld"),
    ));
    simulation.update();
    assert!(order(&simulation, bot).unwrap() > newest);
    let cow = add_at(&mut simulation, EntityKind::Cow, 150, (BX + 3., 64., BZ));
    assert!(order(&simulation, cow).unwrap() > order(&simulation, bot).unwrap(), "re-sent after the bot");

    // Another dimension: a new level with an empty list, the bot first again.
    simulation.receive_packet(make_basic_respawn_packet(
        DimensionKind::new_raw(0),
        Identifier::new("azalea:b"),
    ));
    simulation.update();
    assert_eq!(order(&simulation, bot), Some(0));
    assert_eq!(order(&simulation, cow), None);
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: velocity packets)
#[test]
fn test_a_motion_packet_overwrites_an_earlier_push_and_two_pushers_add_up() {
    let _lock = init();
    let mut simulation = setup();
    add_at(&mut simulation, EntityKind::Cow, 160, (BX + 0.3, 64., BZ));
    add_at(&mut simulation, EntityKind::Pig, 161, (BX + 0.4, 64., BZ));
    simulation.tick();
    let want = -(0.05f32 as f64) * (0.3f64.sqrt() + 0.4f64.sqrt());
    assert!((velocity(&simulation).x - want).abs() < 1e-9, "two pushers add: {}", velocity(&simulation).x);
    assert_eq!(record(&simulation).max_in_tick, 2);

    // The packet is applied before the tick, so the earlier push is gone.
    simulation.receive_packet(ClientboundSetEntityMotion {
        id: simulation.minecraft_entity_id(),
        delta: LpVec3::from(Vec3::new(0.5, 0., 0.)),
    });
    simulation.update();
    assert!((velocity(&simulation).x - 0.5).abs() < 1e-3, "{}", velocity(&simulation).x);
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: the pusher's step in this tick)
#[test]
fn test_a_pusher_before_the_bot_is_taken_after_its_own_interpolation_step() {
    let _lock = init();
    let mut simulation = setup();
    // A cow 2 blocks away does not touch the bot; the server moves it to -2.5, so its first step
    // (1/3 of the way) ends 0.5 from the bot, in contact.
    let cow = add_at(&mut simulation, EntityKind::Cow, 170, (BX + 2.0, 64., BZ));
    simulation.tick();
    assert_eq!(velocity(&simulation).x, 0.0);
    let bot = simulation.entity;
    simulation.app.world_mut().get_mut::<EntityIdIndex>(bot).unwrap().tick_order_mut().assign(bot);
    simulation.receive_packet(ClientboundTeleportEntity {
        id: MinecraftEntityId(170),
        change: PositionMoveRotation {
            pos: Vec3::new(BX - 2.5, 64., BZ),
            delta: Vec3::ZERO,
            look_direction: Default::default(),
        },
        relative: RelativeMovements::all_absolute(),
        on_ground: true,
    });
    let x0 = bot_pos(&simulation).x;
    simulation.tick();
    let cow_x = simulation.app.world().get::<Position>(cow).unwrap().x;
    assert!((cow_x - (BX + 0.5)).abs() < 1e-9, "cow at {cow_x}");
    // The bot felt the push in this tick's travel: the cow, 0.5 away on +x, pushes it to -x.
    assert!(bot_pos(&simulation).x < x0 - 0.02, "moved {}", bot_pos(&simulation).x - x0);
}

// Spec: specs/B1-movement.md §4.2 (Pushes from entities: the bot-to-bot case)
#[test]
fn test_another_bot_pushes_from_this_bots_view_of_it() {
    let _lock = init();
    let mut simulation = setup();
    let mut p = make_basic_add_entity(EntityKind::Player, 180, (BX + 0.3, 64., BZ));
    p.uuid = Uuid::from_u128(20_180);
    simulation.receive_packet(p);
    simulation.update();
    let peer = entity_of(&simulation, 180);
    simulation.app.world_mut().entity_mut(peer).insert(LocalEntity);
    simulation.tick();
    let mut seen = Vec3::ZERO;
    simulation.with_component::<PeerView>(|v| seen = v.get(peer).expect("in the view").position);
    // The other bot's own position moves away; the push reads this bot's view of it.
    simulation.app.world_mut().entity_mut(peer).insert(Position::new(Vec3::new(BX + 9., 64., BZ)));
    let before = record(&simulation).total;
    simulation.tick();
    assert!(record(&simulation).total > before, "pushed by the view of the other bot");
    let (want, _) = azalea_entity::push::push_velocity(seen.x - bot_pos(&simulation).x, seen.z - bot_pos(&simulation).z).unwrap();
    assert!(want < -0.02);
    assert!((record(&simulation).tick_delta.0 - want).abs() < 1e-12, "{} vs {want}", record(&simulation).tick_delta.0);
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: one push per server id, never the own id)
// A shared swarm `World` can hold a second entity that carries a server id (another bot's view of
// it). Vanilla holds one entity per id and `LivingEntity#pushEntities` takes its candidates from
// `Level#getEntities(this, ..)`, which leaves the entity itself out.
#[test]
fn test_no_push_from_an_entity_with_the_bots_own_id() {
    let _lock = init();
    let mut simulation = setup();
    let own = simulation.minecraft_entity_id();
    // Control: a player with another id, 0.3 away, pushes.
    let other = add_at(&mut simulation, EntityKind::Player, 190, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert!(velocity(&simulation).x < -0.02, "control: another id pushes: {}", velocity(&simulation).x);
    // The same entity given the bot's own id no longer pushes it.
    simulation.app.world_mut().entity_mut(other).insert(own);
    let before = record(&simulation).total;
    simulation.tick();
    assert_eq!(record(&simulation).total, before, "the bot was pushed by an entity with its own id");
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: one push per server id, never the own id)
#[test]
fn test_one_push_per_server_id_when_a_view_and_a_second_entity_share_it() {
    let _lock = init();
    let mut simulation = setup();
    // Another bot (id 191): its local entity is read through this bot's view of it ...
    let peer = add_at(&mut simulation, EntityKind::Player, 191, (BX + 0.3, 64., BZ));
    simulation.app.world_mut().entity_mut(peer).insert(LocalEntity);
    simulation.tick();
    simulation.tick();
    assert_eq!(record(&simulation).max_in_tick, 1, "control: one entity, one push");
    // ... and a second, non-local entity carries the same id at the same place.
    let twin = add_at(&mut simulation, EntityKind::Player, 192, (BX + 0.3, 64., BZ));
    simulation.app.world_mut().entity_mut(twin).insert(MinecraftEntityId(191));
    simulation.tick();
    assert_eq!(record(&simulation).max_in_tick, 1, "the same id pushed twice in one tick");
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: one push per server id, never the own id)
// This bot's view of another bot is its client's only copy of that id (patch 31). A second entity with
// the id is another bot's view, at a place this client never has, so it must not push even when the
// view copy is out of reach.
#[test]
fn test_the_view_copy_decides_even_when_out_of_reach() {
    let _lock = init();
    let mut simulation = setup();
    let peer = add_at(&mut simulation, EntityKind::Player, 191, (BX + 1.2, 64., BZ));
    simulation.app.world_mut().entity_mut(peer).insert(LocalEntity);
    simulation.tick();
    simulation.tick();
    let before = record(&simulation).total;
    let twin = add_at(&mut simulation, EntityKind::Player, 192, (BX + 0.3, 64., BZ));
    simulation.tick();
    assert!(record(&simulation).total > before, "control: the twin pushes under its own id");
    simulation.app.world_mut().entity_mut(twin).insert(MinecraftEntityId(191));
    let before = record(&simulation).total;
    simulation.tick();
    assert_eq!(record(&simulation).total, before, "an entity sharing the view copy's id pushed");
}
