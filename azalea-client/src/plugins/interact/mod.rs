pub mod pick;

use azalea_block::BlockState;
use azalea_core::{
    aabb::Aabb,
    direction::Direction,
    game_type::GameMode,
    hit_result::{BlockHitResult, HitResult},
    position::{BlockPos, Vec3},
    tick::GameTick,
};
use azalea_entity::{
    Attributes, LocalEntity, LookDirection, PlayerAbilities, Position,
    attributes::{
        creative_block_interaction_range_modifier, creative_entity_interaction_range_modifier,
    },
    clamp_look_direction,
    indexing::EntityIdIndex,
    inventory::Inventory,
};
use azalea_inventory::{ItemStack, ItemStackData, components};
use azalea_physics::{
    PhysicsSystems, client_movement::ClientMovementState,
    collision::{BlockShapeAt, entity_collisions::update_last_bounding_box},
};
use azalea_protocol::packets::game::{
    ServerboundInteract, ServerboundUseItem,
    s_interact::{self, InteractionHand},
    s_swing::ServerboundSwing,
    s_use_item_on::ServerboundUseItemOn,
};
use azalea_world::World;
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tracing::warn;

use super::mining::Mining;
use crate::{
    attack::handle_attack_event,
    interact::pick::{
        HitResultComponent, insert_initial_hit_result_component, update_hit_result_component,
    },
    inventory::InventorySystems,
    local_player::{LocalGameMode, PermissionLevel},
    movement::MoveEventsSystems,
    packet::game::SendGamePacketEvent,
    respawn::perform_respawn,
};

/// A plugin that allows clients to interact with blocks in the world.
pub struct InteractPlugin;
impl Plugin for InteractPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<StartUseItemEvent>()
            .add_systems(
                Update,
                (
                    update_attributes_for_gamemode,
                    handle_start_use_item_event,
                    insert_initial_hit_result_component
                        .after(clamp_look_direction)
                        .after(update_last_bounding_box),
                )
                    .after(InventorySystems)
                    .after(MoveEventsSystems)
                    .after(perform_respawn)
                    .after(handle_attack_event)
                    .chain(),
            )
            .add_systems(
                GameTick,
                handle_start_use_item_queued.before(PhysicsSystems),
            )
            // Patch 29: `Minecraft#tick` -> `GameRenderer#pick`, once per tick after the look is
            // turned and before any click is checked.
            .add_systems(
                GameTick,
                update_hit_result_component
                    .after(crate::block_actions::handle_look_at_block_queued)
                    .after(crate::entity_actions::handle_look_at_entity_queued)
                    .before(crate::block_actions::handle_break_block_queued)
                    .before(crate::block_actions::handle_place_block_queued)
                    .before(crate::block_actions::handle_use_block_queued)
                    .before(crate::entity_actions::handle_use_entity_queued)
                    .before(handle_start_use_item_queued)
                    .before(crate::mining::MiningSystems)
                    .before(PhysicsSystems),
            )
            .add_observer(handle_entity_interact)
            .add_observer(handle_swing_arm_trigger);
    }
}

pub use azalea_world::BlockStatePredictionHandler;

/// Settle the pending cells up to `seq` and, when the player's box meets a cell whose shown
/// state changed, return the position the player had when the cell was first retained (vanilla
/// `ClientLevel#syncBlockState`: `Entity#isColliding` then `absSnapTo`). The caller moves the
/// player there. `player_box` is the player's box now; `None` skips the snap-back.
// Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
pub fn end_prediction_up_to(
    handler: &mut BlockStatePredictionHandler,
    seq: u32,
    world: &World,
    player_box: Option<&Aabb>,
) -> Option<Vec3> {
    let mut snap = None;
    for (pos, server_block_state, player_pos) in handler.end_predictions_up_to(seq, world) {
        if let Some(player_box) = player_box {
            let cell = Vec3::new(pos.x as f64, pos.y as f64, pos.z as f64);
            let colliding = server_block_state
                .collision_shape_at(pos)
                .to_aabbs()
                .iter()
                .any(|b| b.move_relative(cell).intersects_aabb(player_box));
            if colliding {
                snap = Some(player_pos);
            }
        }
    }
    snap
}

/// An event that makes one of our clients simulate a right-click.
///
/// This event just inserts the [`StartUseItemQueued`] component on the given
/// entity.
#[doc(alias("right click"))]
#[derive(Message)]
pub struct StartUseItemEvent {
    pub entity: Entity,
    pub hand: InteractionHand,
    /// See [`StartUseItemQueued::force_block`].
    pub force_block: Option<BlockPos>,
}
pub fn handle_start_use_item_event(
    mut commands: Commands,
    mut events: MessageReader<StartUseItemEvent>,
) {
    for event in events.read() {
        commands.entity(event.entity).insert(StartUseItemQueued {
            hand: event.hand,
            force_block: event.force_block,
        });
    }
}

/// A component that makes our client simulate a right-click on the next
/// [`GameTick`]. It's removed after that tick.
///
/// You may find it more convenient to use [`StartUseItemEvent`] instead, which
/// just inserts this component for you.
///
/// [`GameTick`]: azalea_core::tick::GameTick
#[derive(Component, Debug)]
pub struct StartUseItemQueued {
    pub hand: InteractionHand,
    /// Optionally force us to send a [`ServerboundUseItemOn`] on the given
    /// block.
    ///
    /// This is useful if you want to interact with a block without looking at
    /// it, but should be avoided to stay compatible with anticheats.
    pub force_block: Option<BlockPos>,
}
#[allow(clippy::type_complexity)]
pub fn handle_start_use_item_queued(
    mut commands: Commands,
    query: Query<(
        Entity,
        &StartUseItemQueued,
        &mut BlockStatePredictionHandler,
        &HitResultComponent,
        &LookDirection,
        Option<&Mining>,
        &Inventory,
        &crate::local_player::WorldHolder,
        Option<&mut crate::respawn::RespawnLog>,
    )>,
    borders: Query<(&crate::tick_counter::TicksConnected, Option<&azalea_entity::unmodelled::WorldBorderState>)>,
) {
    for (entity, start_use_item, mut prediction_handler, hit_result, look_direction, mining, inventory, world, mut log) in
        query
    {
        commands.entity(entity).remove::<StartUseItemQueued>();

        // Patch 34: the held item's use is one the fork does not model (riptide, other use
        // speed); refuse the whole click. Spec: specs/F5-version-layer.md §4.6
        let held = inventory.get_equipment(match start_use_item.hand {
            InteractionHand::MainHand => components::EquipmentSlot::Mainhand,
            InteractionHand::OffHand => components::EquipmentSlot::Offhand,
        });
        if let Some(why) = held
            .and_then(|s| crate::client_refusals::use_start_refusal(s, &world.shared.read().registries))
        {
            if let Some(mut log) = log {
                log.refuse_input(why);
            }
            continue;
        }

        if mining.is_some() {
            warn!("Got a StartUseItemEvent for a client that was mining");
        }
        crate::input_log::note(entity, crate::input_log::InputEvent::UseClick);

        // TODO: this also skips if LocalPlayer.handsBusy is true, which is used when
        // rowing a boat

        let mut hit_result = (**hit_result).clone();

        if let Some(force_block) = start_use_item.force_block {
            let hit_result_matches = if let HitResult::Block(block_hit_result) = &hit_result {
                block_hit_result.block_pos == force_block
            } else {
                false
            };

            if !hit_result_matches {
                // we're not looking at the block, so make up some numbers
                hit_result = HitResult::Block(BlockHitResult {
                    location: force_block.center(),
                    direction: Direction::Up,
                    block_pos: force_block,
                    inside: false,
                    world_border: false,
                    miss: false,
                });
            }
        }

        // Patch 51: `MultiPlayerGameMode#useItemOn` answers FAIL for a clicked block outside the
        // client's border, before the prediction starts; `Minecraft#startUseItem` then stops.
        if let HitResult::Block(r) = &hit_result
            && !r.miss
            && let Ok((ticks, border)) = borders.get(entity)
            && !crate::block_actions::block_in_world_border(border, ticks.0, r.block_pos)
        {
            if let Some(log) = log.as_mut() {
                log.refuse_input(crate::client_refusals::InputRefusal::OutsideBorder);
            }
            continue;
        }

        match &hit_result {
            HitResult::Block(r) => {
                let seq = prediction_handler.start_predicting();
                if r.miss {
                    commands.trigger(SendGamePacketEvent::new(
                        entity,
                        ServerboundUseItem {
                            hand: start_use_item.hand,
                            seq,
                            x_rot: look_direction.x_rot(),
                            y_rot: look_direction.y_rot(),
                        },
                    ));
                } else {
                    commands.trigger(SendGamePacketEvent::new(
                        entity,
                        ServerboundUseItemOn {
                            hand: start_use_item.hand,
                            block_hit: r.into(),
                            seq,
                        },
                    ));
                    // TODO: depending on the result of useItemOn, this might
                    // also need to send a SwingArmEvent.
                    // basically, this TODO is for simulating block
                    // interactions/placements on the client-side.
                }
            }
            HitResult::Entity(r) => {
                commands.trigger(EntityInteractEvent {
                    client: entity,
                    target: r.entity,
                    location: Some(r.location),
                });
            }
        }
    }
}

/// An ECS `Event` that makes the client tell the server that we right-clicked
/// an entity.
#[derive(Clone, Debug, EntityEvent)]
pub struct EntityInteractEvent {
    #[event_target]
    pub client: Entity,
    pub target: Entity,
    /// The position on the entity that we'll tell the server that we clicked
    /// on, in world coordinates (the packet carries it relative to the entity).
    ///
    /// This doesn't matter for most entities. If it's set to `None` but we're
    /// looking at the target, it'll use the correct value. If it's `None` and
    /// we're not looking at the entity, then it'll arbitrary send the target's
    /// exact position.
    pub location: Option<Vec3>,
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: mounting; the right-click bypass)
/// Sends the INTERACT_AT and INTERACT packets of a right-click on an entity. Patch 34 closed the
/// bypass of the mount refusal: a click that would mount (`entity_actions::entity_use_plan_with`
/// is `WouldMount`, or `NeedsCrouch` while standing) is refused here too, logged as
/// `EntityUseRefusal::WouldMount`, and nothing is sent. Null reaches this event only through
/// `StartUseItemQueued` (the adapter does not use it); `UseEntityQueued` has its own path.
#[allow(clippy::too_many_arguments)]
pub fn handle_entity_interact(
    trigger: On<EntityInteractEvent>,
    mut commands: Commands,
    client_query: Query<(&ClientMovementState, &EntityIdIndex, &HitResultComponent)>,
    target_query: Query<&Position>,
    kinds: Query<(
        &azalea_entity::EntityKindComponent,
        Option<&azalea_entity::metadata::AbstractAgeableBaby>,
        Option<&azalea_entity::metadata::Tamed>,
    )>,
    mut dead: Query<&mut crate::respawn::RespawnLog, With<crate::respawn::DeathScreen>>,
) {
    // Patch 22: no mouse while the death screen is open.
    if let Ok(mut log) = dead.get_mut(trigger.client) {
        log.refuse(crate::respawn::DeadAction::InteractEntity);
        return;
    }
    let Some((physics_state, entity_id_index, hit_result)) = client_query.get(trigger.client).ok()
    else {
        warn!(
            "tried to interact with an entity but the client didn't have the required components"
        );
        return;
    };

    // Patch 34: never send a click that would mount.
    if let Ok((kind, baby, tamed)) = kinds.get(trigger.target) {
        let facts = crate::entity_actions::MountFacts {
            baby: baby.map(|b| b.0),
            tamed: tamed.map(|t| t.0),
        };
        let plan = crate::entity_actions::entity_use_plan_with(
            kind.0,
            physics_state.trying_to_crouch,
            facts,
        );
        if matches!(
            plan,
            crate::entity_actions::EntityUsePlan::WouldMount
                | crate::entity_actions::EntityUsePlan::NeedsCrouch
        ) {
            tracing::info!(
                "refused an entity click that would mount: {}",
                crate::entity_actions::EntityUseRefusal::WouldMount.as_str()
            );
            return;
        }
    }

    // TODO: worldborder check

    let Some(entity_id) = entity_id_index.get_by_ecs_entity(trigger.target) else {
        warn!("tried to interact with an entity that isn't known by the client");
        return;
    };

    let location = if let Some(l) = trigger.location {
        l
    } else {
        // if we're looking at the entity, use that
        if let Some(entity_hit_result) = hit_result.as_entity_hit_result()
            && entity_hit_result.entity == trigger.target
        {
            entity_hit_result.location
        } else {
            // if we're not looking at the entity, make up a value that's good enough by
            // using the entity's position
            let Ok(target_position) = target_query.get(trigger.target) else {
                warn!("tried to look at an entity without the entity having a position");
                return;
            };
            **target_position
        }
    };

    // `MultiPlayerGameMode#interactAt` sends the hit point relative to the entity's position.
    let Ok(target_position) = target_query.get(trigger.target) else {
        warn!("tried to interact with an entity without the entity having a position");
        return;
    };
    let mut interact = ServerboundInteract {
        entity_id,
        action: s_interact::ActionType::InteractAt {
            location: location - **target_position,
            hand: InteractionHand::MainHand,
        },
        using_secondary_action: physics_state.trying_to_crouch,
    };
    commands.trigger(SendGamePacketEvent::new(trigger.client, interact.clone()));

    // TODO: this is true if the interaction failed, which i think can only happen
    // in certain cases when interacting with armor stands
    let consumes_action = false;
    if !consumes_action {
        // but yes, most of the time vanilla really does send two interact packets like
        // this
        interact.action = s_interact::ActionType::Interact {
            hand: InteractionHand::MainHand,
        };
        commands.trigger(SendGamePacketEvent::new(trigger.client, interact));
    }
}

/// Whether we can't interact with the block, based on your gamemode.
///
/// If this is false, then we can interact with the block.
///
/// The world, block position, and inventory are used for the adventure mode
/// check.
pub fn check_is_interaction_restricted(
    world: &World,
    block_pos: BlockPos,
    game_mode: &GameMode,
    inventory: &Inventory,
) -> bool {
    match game_mode {
        GameMode::Adventure => {
            // vanilla checks for abilities.mayBuild here but servers have no
            // way of modifying that

            let held_item = inventory.held_item();
            match &held_item {
                ItemStack::Present(item) => {
                    let block = world.get_block_state(block_pos);
                    let Some(block) = block else {
                        // block isn't loaded so just say that it is restricted
                        return true;
                    };
                    check_block_can_be_broken_by_item_in_adventure_mode(item, &block)
                }
                _ => true,
            }
        }
        GameMode::Spectator => true,
        _ => false,
    }
}

/// Check if the item has the `CanDestroy` tag for the block.
pub fn check_block_can_be_broken_by_item_in_adventure_mode(
    item: &ItemStackData,
    _block: &BlockState,
) -> bool {
    // minecraft caches the last checked block but that's kind of an unnecessary
    // optimization and makes the code too complicated

    if item.get_component::<components::CanBreak>().is_none() {
        // no CanDestroy tag
        return false;
    };

    false

    // for block_predicate in can_destroy {
    //     // TODO
    //     // defined in BlockPredicateArgument.java
    // }

    // true
}

pub fn can_use_game_master_blocks(
    abilities: &PlayerAbilities,
    permission_level: &PermissionLevel,
) -> bool {
    abilities.instant_break && **permission_level >= 2
}

/// Swing your arm.
///
/// This is purely a visual effect and won't interact with anything in the
/// world.
#[derive(Clone, Debug, EntityEvent)]
pub struct SwingArmEvent {
    pub entity: Entity,
}
pub fn handle_swing_arm_trigger(
    swing_arm: On<SwingArmEvent>,
    mut commands: Commands,
    mut dead: Query<&mut crate::respawn::RespawnLog, With<crate::respawn::DeathScreen>>,
) {
    // Patch 22: no mouse while the death screen is open.
    if let Ok(mut log) = dead.get_mut(swing_arm.entity) {
        log.refuse(crate::respawn::DeadAction::Swing);
        return;
    }
    commands.trigger(SendGamePacketEvent::new(
        swing_arm.entity,
        ServerboundSwing {
            hand: InteractionHand::MainHand,
        },
    ));
}

#[allow(clippy::type_complexity)]
fn update_attributes_for_gamemode(
    query: Query<(&mut Attributes, &LocalGameMode), (With<LocalEntity>, Changed<LocalGameMode>)>,
) {
    for (mut attributes, game_mode) in query {
        if game_mode.current == GameMode::Creative {
            attributes
                .block_interaction_range
                .insert(creative_block_interaction_range_modifier());
            attributes
                .entity_interaction_range
                .insert(creative_entity_interaction_range_modifier());
        } else {
            attributes
                .block_interaction_range
                .remove(&creative_block_interaction_range_modifier().id);
            attributes
                .entity_interaction_range
                .remove(&creative_entity_interaction_range_modifier().id);
        }
    }
}

#[cfg(test)]
mod prediction_tests {
    use azalea_core::position::ChunkPos;
    use azalea_registry::builtin::BlockKind;
    use azalea_world::{Chunk, PartialChunkStorage};

    use super::*;

    fn world() -> (World, PartialChunkStorage) {
        let mut world = World::default();
        world.chunks = azalea_world::ChunkStorage::new(384, -64);
        let mut partial = PartialChunkStorage::new(2);
        partial.set(&ChunkPos::new(0, 0), Some(Chunk::default()), &mut world.chunks);
        (world, partial)
    }

    const POS: BlockPos = BlockPos::new(2, 64, 2);
    fn stone() -> BlockState {
        BlockState::from(BlockKind::Stone)
    }
    fn dirt() -> BlockState {
        BlockState::from(BlockKind::Dirt)
    }
    fn far_box() -> Aabb {
        Aabb { min: Vec3::new(10.0, 64.0, 10.0), max: Vec3::new(10.6, 65.8, 10.6) }
    }
    /// Predict `state` at `POS` the way the action systems do: a write into the bot's own world
    /// inside a prediction scope.
    fn predict(h: &mut BlockStatePredictionHandler, world: &World, state: BlockState, at: Vec3) {
        h.predict_block(world, POS, state, at);
    }
    fn shown(world: &World) -> Option<BlockState> {
        world.get_block_state(POS)
    }

    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    #[test]
    fn a_prediction_is_in_this_bots_own_world_only() {
        let (world, _keep) = world();
        let (other_world, _keep_b) = self::world();
        let mut h = BlockStatePredictionHandler::default();
        h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        assert_eq!(shown(&world), Some(stone()));
        // the other bot's world still shows the server's block
        assert_eq!(shown(&other_world), Some(BlockState::AIR));
        // the entry keeps the server's block
        assert_eq!(h.server_block_state(&world, POS), Some(BlockState::AIR));
    }

    #[test]
    fn ack_with_the_same_state_keeps_the_shown_block() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        // the server's update arrives while the cell is pending: it changes the entry only
        h.set_server_verified(&world, &[(POS, stone())]);
        assert_eq!(h.server_block_state(&world, POS), Some(stone()));
        assert_eq!(end_prediction_up_to(&mut h, seq, &world, Some(&far_box())), None);
        assert_eq!(shown(&world), Some(stone()));
        assert_eq!(h.acked_seq(), seq);
        // settled: the cell is not pending any more
        assert!(!h.update_known_server_state(POS, dirt()));
    }

    #[test]
    fn ack_with_a_different_state_rolls_back() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        // no update came: the server refused, so the ack writes the kept state back
        assert_eq!(end_prediction_up_to(&mut h, seq, &world, Some(&far_box())), None);
        assert_eq!(shown(&world), Some(BlockState::AIR));
        // the server put something else there before the ack
        let seq = h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        h.set_server_verified(&world, &[(POS, dirt())]);
        assert_eq!(shown(&world), Some(stone()), "an update for a pending cell does not write the level");
        end_prediction_up_to(&mut h, seq, &world, Some(&far_box()));
        assert_eq!(shown(&world), Some(dirt()));
    }

    #[test]
    fn a_later_write_to_the_same_cell_keeps_the_first_old_state() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let s1 = h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        let s2 = h.start_predicting();
        predict(&mut h, &world, dirt(), Vec3::new(9.0, 9.0, 9.0));
        // an ack for the first sequence leaves the cell pending (its sequence was refreshed)
        end_prediction_up_to(&mut h, s1, &world, None);
        assert_eq!(shown(&world), Some(dirt()));
        end_prediction_up_to(&mut h, s2, &world, None);
        assert_eq!(shown(&world), Some(BlockState::AIR));
    }

    // Same cell twice: the first ack leaves the cell pending, the second settles it against the
    // first old state, and the player position kept is the first one.
    #[test]
    fn the_same_cell_twice_settles_at_the_second_ack_against_the_first_old_state() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let first_pos = Vec3::new(2.5, 66.0, 2.5);
        world.set_block_state(POS, stone());
        let s1 = h.start_predicting();
        predict(&mut h, &world, BlockState::AIR, first_pos);
        let s2 = h.start_predicting();
        predict(&mut h, &world, dirt(), Vec3::new(9.0, 9.0, 9.0));
        assert_eq!(end_prediction_up_to(&mut h, s1, &world, Some(&far_box())), None);
        assert!(h.is_pending(POS), "the first ack does not settle the cell");
        assert_eq!(h.server_block_state(&world, POS), Some(stone()), "the first old state");
        // the second ack: the cell shows dirt, the kept state is stone; a player standing in the
        // kept state's shape is moved to where he stood at the first write
        let inside_stone = Aabb { min: Vec3::new(2.2, 64.2, 2.2), max: Vec3::new(2.8, 66.0, 2.8) };
        assert_eq!(end_prediction_up_to(&mut h, s2, &world, Some(&inside_stone)), Some(first_pos));
        assert!(!h.is_pending(POS));
        assert_eq!(shown(&world), Some(stone()));
    }

    #[test]
    fn rolled_back_block_in_the_players_box_snaps_the_player_back() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        let before = Vec3::new(2.5, 66.0, 2.5);
        // the server says the cell is stone, the bot predicted air: the player at `before` is inside
        world.set_block_state(POS, stone());
        predict(&mut h, &world, BlockState::AIR, before);
        let inside = Aabb { min: Vec3::new(2.2, 64.2, 2.2), max: Vec3::new(2.8, 66.0, 2.8) };
        assert_eq!(end_prediction_up_to(&mut h, seq, &world, Some(&inside)), Some(before));
        assert_eq!(shown(&world), Some(stone()));
        // standing beside it: no snap
        let seq = h.start_predicting();
        predict(&mut h, &world, BlockState::AIR, before);
        assert_eq!(end_prediction_up_to(&mut h, seq, &world, Some(&far_box())), None);
        assert_eq!(shown(&world), Some(stone()));
    }

    // Edge case a (vanilla result): this bot's chunk packet re-sent while a cell is pending.
    // `ClientPacketListener#updateLevelChunk` replaces the chunk, so the packet's block shows until
    // the ack; the ack then writes the kept state over it (`ClientLevel#syncBlockState`).
    #[test]
    fn a_resent_chunk_shows_until_the_ack_which_writes_the_kept_state() {
        let (mut world, mut partial) = world();
        world.set_block_state(POS, dirt());
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        // the chunk packet: the chunk is replaced by one with air at POS
        partial.set(&ChunkPos::new(0, 0), None, &mut world.chunks);
        partial.set(&ChunkPos::new(0, 0), Some(Chunk::default()), &mut world.chunks);
        assert_eq!(shown(&world), Some(BlockState::AIR), "the packet's block shows");
        assert!(h.is_pending(POS), "the entry stays");
        end_prediction_up_to(&mut h, seq, &world, None);
        assert_eq!(shown(&world), Some(dirt()), "the ack writes the kept state over the packet's block");
    }

    // Forgotten chunk: the level reads nothing for the cell, so the ack finds it different, the
    // write does nothing, and the snap test still runs.
    #[test]
    fn a_forgotten_chunk_at_the_ack_writes_nothing_and_the_snap_still_runs() {
        let (mut world, mut partial) = world();
        world.set_block_state(POS, stone());
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        let before = Vec3::new(2.5, 66.0, 2.5);
        predict(&mut h, &world, BlockState::AIR, before);
        partial.set(&ChunkPos::new(0, 0), None, &mut world.chunks);
        assert_eq!(shown(&world), None);
        let inside = Aabb { min: Vec3::new(2.2, 64.2, 2.2), max: Vec3::new(2.8, 66.0, 2.8) };
        assert_eq!(end_prediction_up_to(&mut h, seq, &world, Some(&inside)), Some(before), "the snap runs");
        assert_eq!(shown(&world), None, "the write did nothing: there is no chunk");
        assert!(!h.is_pending(POS));
        // the chunk comes back later with air: the settled cell is not written again
        partial.set(&ChunkPos::new(0, 0), Some(Chunk::default()), &mut world.chunks);
        assert_eq!(shown(&world), Some(BlockState::AIR));
    }

    // Judgement: with a later prediction in the cell, the server's state is the entry's, not the
    // level's.
    #[test]
    fn server_block_state_reads_the_entry_when_a_later_prediction_is_in_the_cell() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        h.start_predicting();
        predict(&mut h, &world, dirt(), Vec3::ZERO);
        assert_eq!(shown(&world), Some(dirt()));
        assert_eq!(h.server_block_state(&world, POS), Some(BlockState::AIR));
        // a cell that is not pending reads the level
        assert_eq!(h.server_block_state(&world, POS.up(1)), Some(BlockState::AIR));
    }

    // Rule 1: a write that changes nothing keeps nothing (`ClientLevel#setBlock` returns false).
    #[test]
    fn a_write_that_changes_nothing_is_not_kept() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        h.start_predicting();
        assert!(!h.predict_block(&world, POS, BlockState::AIR, Vec3::ZERO));
        assert!(!h.is_pending(POS));
    }

    // Edge case d (unit part): a new level starts with a new handler.
    #[test]
    fn reset_drops_pending_cells_and_the_sequence() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        h.start_predicting();
        predict(&mut h, &world, stone(), Vec3::ZERO);
        h.reset();
        assert!(!h.is_pending(POS));
        assert_eq!(h.seq(), 0);
    }
}
