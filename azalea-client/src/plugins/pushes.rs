//! Pushes on the bot from other entities (Null fork, patch 32).
//!
//! Vanilla 1.21.11: the bot is the local player. Every other living entity pushes it from the end
//! of its own `aiStep` (`LivingEntity#pushEntities`, `Entity#push(Entity)`), a boat from
//! `AbstractBoat#tick` (`AbstractBoat#push`), as the pure rules in `azalea_entity::push` say. The
//! client ticks entities in the order they entered its level (`ClientLevel#tickEntities`), so an
//! entity that ticks before the bot changes this tick's travel, and one that ticks after it
//! changes the next tick's. One function runs twice per tick for that:
//!
//! - pass A, before the whole physics chain: pushers that tick before the bot. Their position is
//!   the one after their own interpolation step of this tick (patch 31 takes that step after the
//!   chain, so the pass peeks at it), and the bot's box is the one from the end of the last tick.
//! - pass B, after the chain, the position packet and patch 31's interpolation: pushers that tick
//!   after the bot, against the bot's box after this tick's move.
//!
//! The server never tells the bot about a push (`ServerEntity#sendChanges` sends it to the other
//! trackers), so the bot computes it alone. Grim: `MovementTicker#handleEntityCollisions` only
//! widens check `Simulation`'s tolerance by the pushable entities nearby; it cannot prove a push.
//! Skipped on purpose: the pusher's own mirror velocity, cramming (server only), the
//! frozen-entity state and the chunk-section ticking state (see `azalea_entity::push`), and
//! passengers on the bot (only the bot riding is tracked, and that disconnects).
//!
//! Spec: specs/B1-movement.md §4.2 (Pushes from entities)

use azalea_core::{entity_id::MinecraftEntityId, game_type::GameMode, position::{ChunkPos, Vec3}, tick::GameTick};
use azalea_entity::{
    Dead, EntityKindComponent, EntityUuid, HasClientLoaded, LocalEntity, OnClimbable, Physics,
    Position,
    dimensions::EntityDimensions,
    indexing::EntityIdIndex,
    interpolation::{EntityInterpolation, PeerView},
    metadata::{AbstractBoat, AbstractLiving, SleepingPos},
    push::{
        PushRecord, PusherClass, boat_height_rule, push_box, push_velocity, pusher_class,
        target_pushable,
    },
    teams::{Scoreboard, TeamRef, team_allows_push},
    unmodelled::RidingVehicle,
    LoadedBy,
};
use azalea_physics::{PhysicsSystems, collision::entity_collisions::update_last_bounding_box};
use azalea_registry::builtin::EntityKind;
use azalea_world::{WorldName, Worlds};
use bevy_app::{App, Plugin};
use bevy_ecs::{prelude::*, query::QueryData};
use std::collections::HashSet;

use crate::{
    local_player::{LocalGameMode, TabList},
    player::GameProfileComponent,
    unmodelled_disconnect::disconnect_on_unmodelled_movement,
};

/// How far from the bot's box the search for pushers reaches. An entity that interpolates moves
/// at most a third of 64 blocks in one step, and the chunk index lags its position by the steps
/// since the last `Update`, so the search looks this far to see a pusher before it arrives.
const SEARCH_MARGIN: f64 = 22.0;

/// Both push passes, so a probe or a later system can order itself after them.
#[derive(Clone, Debug, Eq, Hash, PartialEq, SystemSet)]
pub struct PushSystems;

pub struct PushesPlugin;
impl Plugin for PushesPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            (
                // Vanilla casts the ray and handles the clicks before any entity ticks
                // (`Minecraft#tick`), so those systems come first.
                push_pass::<false>
                    .after(disconnect_on_unmodelled_movement)
                    .after(crate::interact::pick::update_hit_result_component)
                    .after(crate::entity_actions::handle_look_at_entity_queued)
                    .after(crate::entity_actions::handle_use_entity_queued)
                    .after(crate::mining::MiningSystems)
                    .after(crate::loading::update_in_loaded_chunk_of_clients)
                    .before(PhysicsSystems)
                    .in_set(PushSystems),
                push_pass::<true>
                    .after(PhysicsSystems)
                    .after(crate::movement::send_position)
                    .after(update_last_bounding_box)
                    .after(azalea_entity::interpolation::update_peer_views)
                    .in_set(PushSystems),
            ),
        );
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
struct BotData {
    entity: Entity,
    position: &'static Position,
    dimensions: &'static EntityDimensions,
    physics: &'static mut Physics,
    record: &'static mut PushRecord,
    world: &'static WorldName,
    ids: &'static EntityIdIndex,
    game_mode: &'static LocalGameMode,
    on_climbable: &'static OnClimbable,
    sleeping: Option<&'static SleepingPos>,
    riding: Option<&'static RidingVehicle>,
    scoreboard: Option<&'static Scoreboard>,
    peers: Option<&'static PeerView>,
    own_id: Option<&'static MinecraftEntityId>,
    tab_list: Option<&'static TabList>,
    profile: Option<&'static GameProfileComponent>,
}

#[derive(QueryData)]
struct PusherData {
    position: &'static Position,
    dimensions: &'static EntityDimensions,
    kind: &'static EntityKindComponent,
    uuid: &'static EntityUuid,
    loaded_by: Option<&'static LoadedBy>,
    interpolation: Option<&'static EntityInterpolation>,
    profile: Option<&'static GameProfileComponent>,
    living: Has<AbstractLiving>,
    boat: Has<AbstractBoat>,
    local: Has<LocalEntity>,
    mcid: Option<&'static MinecraftEntityId>,
}

/// One pass of the push function. `AFTER` false is pass A (pushers that tick before the bot),
/// true is pass B (pushers that tick after it).
// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[allow(clippy::type_complexity)]
fn push_pass<const AFTER: bool>(
    mut bots: Query<BotData, (With<LocalEntity>, With<HasClientLoaded>, Without<Dead>)>,
    pushers: Query<PusherData>,
    worlds: Res<Worlds>,
) {
    for mut bot in &mut bots {
        if !AFTER {
            bot.record.begin_tick();
        }
        // A rider is a `Movement` entry and disconnects; a spectator or a bot on a climbable is
        // not a target for any pusher.
        if bot.riding.is_some()
            || bot.game_mode.current == GameMode::Spectator
            || **bot.on_climbable
        {
            continue;
        }
        let bot_pos: Vec3 = **bot.position;
        let bot_box = bot.dimensions.make_bounding_box(bot_pos);
        let sleeping = bot.sleeping.is_some_and(|s| s.0.is_some());
        let bot_name = bot.profile.map(|p| p.name.as_str());
        let teams = bot.scoreboard.filter(|s| !s.is_empty());

        let mut hits: Vec<(Entity, (f64, f64))> = Vec::new();
        // Patch 41: each server id pushes at most once per pass, and never the bot's own id.
        // Vanilla holds one entity per id and `Level#getPushableEntities` → `Level#getEntities(this, ..)` leaves `this` out
        // (`LivingEntity#pushEntities`); the shared swarm `World` can hold a second entity with an
        // id (another bot's view of it), so the rule is applied here.
        // The id is claimed before the reach and side checks: the view copy is this client's only copy, so a
        // twin never pushes in its place, and in neither pass (`test_the_view_copy_decides_even_when_out_of_reach`).
        // Spec: specs/F5-version-layer.md §4.6 (What Null computes)
        let own_id = bot.own_id.copied();
        let mut claimed: HashSet<MinecraftEntityId> = HashSet::new();
        let mut consider = |entity: Entity, pos: Vec3, data: &PusherDataItem| {
            if let Some(id) = data.mcid.copied() {
                if Some(id) == own_id || !claimed.insert(id) {
                    return;
                }
            }
            let Some(class) = pusher_class(data.living, data.boat, **data.kind) else { return };
            // A pass handles the pushers on its side of the bot in the tick order.
            let before = bot.ids.tick_order().ticks_before(entity, bot.entity);
            if before == AFTER {
                return;
            }
            let pusher_box = data.dimensions.make_bounding_box(pos);
            if !push_box(class, &pusher_box).intersects_aabb(&bot_box) {
                return;
            }
            // A spectator remote player has `noPhysics` (`Player#tick`).
            if **data.kind == EntityKind::Player
                && bot
                    .tab_list
                    .and_then(|t| t.get(&**data.uuid))
                    .is_some_and(|i| i.gamemode == GameMode::Spectator)
            {
                return;
            }
            if class == PusherClass::Boat && !boat_height_rule(bot_box.min.y, pusher_box.min.y) {
                return;
            }
            if !target_pushable(false, false, false, false, sleeping, class) {
                return;
            }
            if let Some(sb) = teams {
                let uuid_text;
                let name = match (**data.kind == EntityKind::Player, data.profile) {
                    (true, Some(p)) => p.name.as_str(),
                    _ => {
                        uuid_text = data.uuid.to_string();
                        uuid_text.as_str()
                    }
                };
                let mine: Option<TeamRef> = bot_name.and_then(|n| sb.team_ref(n));
                if !team_allows_push(sb.team_ref(name), mine) {
                    return;
                }
            }
            if let Some(dv) = push_velocity(pos.x - bot_pos.x, pos.z - bot_pos.z) {
                hits.push((entity, dv));
            }
        };

        // The other bots this bot sees (patch 31): their smoothed positions, not their own. Read first, so the
        // view entry of a bot wins over any other entity with its id.
        if let Some(view) = bot.peers {
            for (peer, entry) in view.iter() {
                let Ok(data) = pushers.get(peer) else { continue };
                let pos = if AFTER { entry.position } else { entry.peek_position() };
                consider(peer, pos, &data);
            }
        }

        // Entities every bot shares (mobs, boats, remote players).
        if let Some(world) = worlds.get(bot.world) {
            let world = world.read();
            let chunk = |v: f64| (v / 16.0).floor() as i32;
            for cx in chunk(bot_box.min.x - SEARCH_MARGIN)..=chunk(bot_box.max.x + SEARCH_MARGIN) {
                for cz in chunk(bot_box.min.z - SEARCH_MARGIN)..=chunk(bot_box.max.z + SEARCH_MARGIN) {
                    let Some(in_chunk) = world.entities_by_chunk.get(&ChunkPos::new(cx, cz)) else {
                        continue;
                    };
                    for &entity in in_chunk {
                        if entity == bot.entity {
                            continue;
                        }
                        let Ok(data) = pushers.get(entity) else { continue };
                        if !data.loaded_by.is_some_and(|l| l.contains(&bot.entity))
                            || !bot.ids.contains_ecs_entity(entity)
                        {
                            continue;
                        }
                        // Another bot is read from this bot's own view below, never from its own
                        // exact position.
                        if data.local {
                            continue;
                        }
                        let pos: Vec3 = **data.position;
                        let pos = match (AFTER, data.interpolation) {
                            (false, Some(i)) => i.0.peek(pos),
                            _ => pos,
                        };
                        consider(entity, pos, &data);
                    }
                }
            }
        }
        hits.sort_by_key(|h| h.0);
        for (_, dv) in hits {
            bot.physics.velocity.x += dv.0;
            bot.physics.velocity.z += dv.1;
            bot.record.record(dv);
        }
    }
}
