pub mod pick;

use std::collections::HashMap;

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
    collision::{BlockWithShape, entity_collisions::update_last_bounding_box},
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

/// A component that contains information about our local block state
/// predictions.
#[derive(Clone, Component, Debug, Default)]
pub struct BlockStatePredictionHandler {
    /// The total number of changes that this client has made to blocks.
    seq: u32,
    /// The highest sequence number the server has acknowledged with
    /// [`ClientboundBlockChangedAck`].
    ///
    /// [`ClientboundBlockChangedAck`]: azalea_protocol::packets::game::ClientboundBlockChangedAck
    acked_seq: u32,
    server_state: HashMap<BlockPos, ServerVerifiedState>,
}
#[derive(Clone, Debug)]
struct ServerVerifiedState {
    seq: u32,
    block_state: BlockState,
    /// Used for teleporting the player back if we're colliding with the block
    /// that got placed back.
    player_pos: Vec3,
}

impl BlockStatePredictionHandler {
    /// The sequence number of the latest change this client sent.
    pub fn seq(&self) -> u32 {
        self.seq
    }

    /// The highest sequence number the server has acknowledged so far.
    pub fn acked_seq(&self) -> u32 {
        self.acked_seq
    }

    /// Get the next sequence number that we're going to use and increment the
    /// value.
    pub fn start_predicting(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    /// Should be called right before the client updates a block with its
    /// prediction.
    ///
    /// This is used to make sure that we can rollback to this state if the
    /// server acknowledges the sequence number (with
    /// [`ClientboundBlockChangedAck`]) without having sent a block update.
    ///
    /// [`ClientboundBlockChangedAck`]: azalea_protocol::packets::game::ClientboundBlockChangedAck
    pub fn retain_known_server_state(
        &mut self,
        pos: BlockPos,
        old_state: BlockState,
        player_pos: Vec3,
    ) {
        self.server_state
            .entry(pos)
            .and_modify(|s| s.seq = self.seq)
            .or_insert(ServerVerifiedState {
                seq: self.seq,
                block_state: old_state,
                player_pos,
            });
    }

    /// Save this update as the correct server state so when the server sends a
    /// [`ClientboundBlockChangedAck`] we don't roll back this new update.
    ///
    /// This should be used when we receive a block update from the server.
    ///
    /// [`ClientboundBlockChangedAck`]: azalea_protocol::packets::game::ClientboundBlockChangedAck
    pub fn update_known_server_state(&mut self, pos: BlockPos, state: BlockState) -> bool {
        if let Some(s) = self.server_state.get_mut(&pos) {
            s.block_state = state;
            true
        } else {
            false
        }
    }

    /// Settle every retained cell up to `seq`: write back the server's state where it differs
    /// and, when the player's box meets a cell whose state changed, return the position the
    /// player had when the cell was first retained (vanilla `ClientLevel#syncBlockState`:
    /// `Entity#isColliding` then `absSnapTo`). The caller moves the player there.
    ///
    /// `player_box` is the player's box now; `None` skips the snap-back.
    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    pub fn end_prediction_up_to(
        &mut self,
        seq: u32,
        world: &World,
        player_box: Option<&Aabb>,
    ) -> Option<Vec3> {
        self.acked_seq = self.acked_seq.max(seq);
        // oldest first, so the newest prediction decides the snap when several cells collide
        let mut settled: Vec<(BlockPos, ServerVerifiedState)> = self
            .server_state
            .iter()
            .filter(|(_, s)| s.seq <= seq)
            .map(|(p, s)| (*p, s.clone()))
            .collect();
        settled.sort_by_key(|(p, s)| (s.seq, p.x, p.y, p.z));
        let mut snap = None;
        for (pos, state) in settled {
            self.server_state.remove(&pos);

            // syncBlockState
            let client_block_state = world.get_block_state(pos).unwrap_or_default();
            let server_block_state = state.block_state;
            if client_block_state == server_block_state {
                continue;
            }
            world.set_block_state(pos, server_block_state);
            if let Some(player_box) = player_box {
                let cell = Vec3::new(pos.x as f64, pos.y as f64, pos.z as f64);
                let colliding = server_block_state
                    .collision_shape()
                    .to_aabbs()
                    .iter()
                    .any(|b| b.move_relative(cell).intersects_aabb(player_box));
                if colliding {
                    snap = Some(state.player_pos);
                }
            }
        }
        snap
    }
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
) {
    for (entity, start_use_item, mut prediction_handler, hit_result, look_direction, mining, inventory, world, log) in
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
                    let block = world.chunks.get_block_state(block_pos);
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

    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    #[test]
    fn ack_with_the_same_state_keeps_the_prediction() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        let old = world.set_block_state(POS, stone()).unwrap();
        h.retain_known_server_state(POS, old, Vec3::ZERO);
        // the server's update arrives while the cell is retained: known state only, world untouched
        assert!(h.update_known_server_state(POS, stone()));
        assert_eq!(world.get_block_state(POS), Some(stone()));
        assert_eq!(h.end_prediction_up_to(seq, &world, Some(&far_box())), None);
        assert_eq!(world.get_block_state(POS), Some(stone()));
        assert_eq!(h.acked_seq(), seq);
        // settled: the cell is not retained any more, so an update goes straight to the world
        assert!(!h.update_known_server_state(POS, dirt()));
    }

    #[test]
    fn ack_with_a_different_state_rolls_back() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        let old = world.set_block_state(POS, stone()).unwrap();
        h.retain_known_server_state(POS, old, Vec3::ZERO);
        // no update came: the server refused, so the ack restores the old state
        assert_eq!(h.end_prediction_up_to(seq, &world, Some(&far_box())), None);
        assert_eq!(world.get_block_state(POS), Some(old));
        // the server put something else there before the ack
        let seq = h.start_predicting();
        let old = world.set_block_state(POS, stone()).unwrap();
        h.retain_known_server_state(POS, old, Vec3::ZERO);
        h.update_known_server_state(POS, dirt());
        h.end_prediction_up_to(seq, &world, Some(&far_box()));
        assert_eq!(world.get_block_state(POS), Some(dirt()));
    }

    #[test]
    fn a_later_write_to_the_same_cell_keeps_the_first_old_state() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let s1 = h.start_predicting();
        let old = world.set_block_state(POS, stone()).unwrap();
        h.retain_known_server_state(POS, old, Vec3::ZERO);
        let s2 = h.start_predicting();
        let old2 = world.set_block_state(POS, dirt()).unwrap();
        h.retain_known_server_state(POS, old2, Vec3::new(9.0, 9.0, 9.0));
        // an ack for the first sequence leaves the cell retained (its sequence was refreshed)
        h.end_prediction_up_to(s1, &world, None);
        assert_eq!(world.get_block_state(POS), Some(dirt()));
        h.end_prediction_up_to(s2, &world, None);
        assert_eq!(world.get_block_state(POS), Some(old));
    }

    #[test]
    fn rolled_back_block_in_the_players_box_snaps_the_player_back() {
        let (world, _keep) = world();
        let mut h = BlockStatePredictionHandler::default();
        let seq = h.start_predicting();
        let before = Vec3::new(2.5, 66.0, 2.5);
        // the server will say the cell is stone (it was air here), so the player at `before` is inside
        let old = world.set_block_state(POS, BlockState::AIR).unwrap_or_default();
        h.retain_known_server_state(POS, stone(), before);
        let _ = old;
        let inside = Aabb { min: Vec3::new(2.2, 64.2, 2.2), max: Vec3::new(2.8, 66.0, 2.8) };
        assert_eq!(h.end_prediction_up_to(seq, &world, Some(&inside)), Some(before));
        assert_eq!(world.get_block_state(POS), Some(stone()));
        // standing beside it: no snap
        let seq = h.start_predicting();
        world.set_block_state(POS, BlockState::AIR);
        h.retain_known_server_state(POS, stone(), before);
        assert_eq!(h.end_prediction_up_to(seq, &world, Some(&far_box())), None);
        assert_eq!(world.get_block_state(POS), Some(stone()));
    }
}
