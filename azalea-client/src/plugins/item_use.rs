//! Hold and release an item: eat, raise a shield, drink, and so on.
//!
//! [`StartUseItemQueued`](crate::interact::StartUseItemQueued) right-clicks what the crosshair is
//! on, so with a block in view it sends only `UseItemOn`. Vanilla sends `UseItem` as well when the
//! item can be used in the air, and a server starts eating or blocking only on that one. Insert
//! [`UseItemQueued`] to send `UseItem` for a hand whatever the crosshair shows, and
//! [`ReleaseUseItemQueued`] to let go. The components are handled in the same `GameTick` (a message
//! would land a tick late).
//!
//! While a use lasts, the client has an [`ItemInUse`]: the hand, the ticks and the 0.2 movement
//! slow-down. It ends as vanilla ends it. The server finishes or cancels the use by clearing the
//! "using item" flag on our entity (food completes after its consume time, 32 ticks for most);
//! we release by hand; we change the held slot or swap with the offhand; or we die.

use azalea_core::{direction::Direction, game_type::GameMode, position::BlockPos, tick::GameTick};
use azalea_entity::{
    Dead, LookDirection, PlayerAbilities,
    inventory::Inventory,
    metadata::AbstractLivingUsingItem,
};
use azalea_inventory::{ItemStack, components};
use azalea_physics::PhysicsSystems;
use azalea_protocol::packets::game::{
    ServerboundPlayerAction, ServerboundUseItem, s_interact::InteractionHand,
    s_player_action::Action,
};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use tracing::debug;

use crate::{
    interact::{BlockStatePredictionHandler, handle_start_use_item_queued},
    client_refusals::use_start_refusal,
    inventory::ensure_has_sent_carried_item,
    local_player::{Hunger, LocalGameMode, WorldHolder},
    respawn::RespawnLog,
    movement::ItemInUse,
    packet::game::SendGamePacketEvent,
    tick_clock::DroppedTicks,
};

/// A use the server never flags within this many ticks was refused (food that cannot be eaten,
/// say), so we stop slowing down.
const UNCONFIRMED_TIMEOUT_TICKS: u32 = 40;
/// Vanilla's use duration for things held up until released (shields, bows).
pub const HELD_USE_TICKS: u32 = 72_000;

pub struct ItemUsePlugin;
impl Plugin for ItemUsePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            (
                handle_use_item_queued,
                handle_release_use_item_queued,
                handle_swap_offhand_queued,
                tick_item_in_use,
            )
                .chain()
                .after(ensure_has_sent_carried_item)
                // Shares the prediction counter with the other click handlers; run after them.
                .after(handle_start_use_item_queued)
                .before(PhysicsSystems),
        );
    }
}

/// Makes the client send `UseItem` for `hand` on the next `GameTick`, and start an [`ItemInUse`]
/// if vanilla would start using that item. Ignored while an item is already in use.
#[derive(Component, Clone, Copy, Debug)]
pub struct UseItemQueued {
    pub hand: InteractionHand,
}

/// Makes the client release the item in use on the next `GameTick` (sends `ReleaseUseItem` if one
/// is in use).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ReleaseUseItemQueued;

/// Makes the client swap the held item with the offhand on the next `GameTick`; the swap ends any
/// use, as in vanilla.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct SwapOffhandQueued;

fn stack_in_hand(inventory: &Inventory, hand: InteractionHand) -> Option<&ItemStack> {
    use components::EquipmentSlot;
    inventory.get_equipment(match hand {
        InteractionHand::MainHand => EquipmentSlot::Mainhand,
        InteractionHand::OffHand => EquipmentSlot::Offhand,
    })
}

/// Whether vanilla's `Item.use` would start using this stack (so we slow down from this tick).
/// Items not listed (bows, tridents, ...) start only when the server flags them.
fn starts_using(
    stack: &ItemStack,
    hunger: &Hunger,
    abilities: &PlayerAbilities,
    mode: GameMode,
) -> bool {
    if stack.get_component::<components::BlocksAttacks>().is_some() {
        return true;
    }
    if stack.get_component::<components::Consumable>().is_some() {
        // Player.canEat: food needs hunger unless the item always can be eaten.
        return match stack.get_component::<components::Food>() {
            Some(food) => {
                food.can_always_eat || abilities.invulnerable || mode == GameMode::Creative || hunger.food < 20
            }
            None => true,
        };
    }
    false
}

// Spec: specs/B2-survival-upkeep.md §4.1 (eating: UseItem with a block in view, 0.2 slow-down)
#[allow(clippy::type_complexity)]
pub fn handle_use_item_queued(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &UseItemQueued,
        &mut BlockStatePredictionHandler,
        &LookDirection,
        &Inventory,
        &Hunger,
        &PlayerAbilities,
        &LocalGameMode,
        Has<ItemInUse>,
        &WorldHolder,
        Option<&mut RespawnLog>,
    )>,
    dropped: Option<Res<DroppedTicks>>,
) {
    for (entity, queued, mut prediction, look, inventory, hunger, abilities, mode, in_use, world, log) in
        &mut query
    {
        commands.entity(entity).remove::<UseItemQueued>();
        // Vanilla only starts a use when none is running.
        if in_use || mode.current == GameMode::Spectator {
            debug!("ignored a UseItemQueued: item already in use or spectator");
            continue;
        }
        let Some(stack) = stack_in_hand(inventory, queued.hand) else { continue };
        if matches!(stack, ItemStack::Empty) {
            continue;
        }
        // Patch 34: a use the fork does not model (riptide, other use speed) is refused.
        if let Some(why) = use_start_refusal(stack, &world.shared.read().registries) {
            if let Some(mut log) = log {
                log.refuse_input(why);
            }
            continue;
        }
        commands.trigger(SendGamePacketEvent::new(
            entity,
            ServerboundUseItem {
                hand: queued.hand,
                seq: prediction.start_predicting(),
                x_rot: look.x_rot(),
                y_rot: look.y_rot(),
            },
        ));
        if starts_using(stack, hunger, abilities, mode.current) {
            commands.entity(entity).insert(ItemInUse {
                hand: queued.hand,
                slot: inventory.selected_hotbar_slot,
                ticks: 0,
                confirmed: false,
                confirmed_at: None,
                dropped_seen: dropped.as_deref().map_or(0, |d| d.0),
                lost: 0,
            });
        }
    }
}

// Spec: specs/B2-survival-upkeep.md §4.1 (release early: nothing is eaten)
pub fn handle_release_use_item_queued(
    mut commands: Commands,
    query: Query<(Entity, Has<ItemInUse>), With<ReleaseUseItemQueued>>,
) {
    for (entity, in_use) in &query {
        commands.entity(entity).remove::<ReleaseUseItemQueued>();
        if in_use {
            send_player_action(&mut commands, entity, Action::ReleaseUseItem);
            commands.entity(entity).remove::<ItemInUse>();
        }
    }
}

pub fn handle_swap_offhand_queued(
    mut commands: Commands,
    query: Query<Entity, With<SwapOffhandQueued>>,
) {
    for entity in &query {
        send_player_action(&mut commands, entity, Action::SwapItemWithOffhand);
        commands
            .entity(entity)
            .remove::<SwapOffhandQueued>()
            .remove::<ItemInUse>();
    }
}

fn send_player_action(commands: &mut Commands, entity: Entity, action: Action) {
    commands.trigger(SendGamePacketEvent::new(
        entity,
        ServerboundPlayerAction {
            action,
            pos: BlockPos::new(0, 0, 0),
            direction: Direction::Down,
            seq: 0,
        },
    ));
}

/// Counts the ticks of a use and ends it when vanilla would: the server dropped the flag, the
/// held slot changed, we died, or the server never accepted it.
pub fn tick_item_in_use(
    mut commands: Commands,
    mut query: Query<(Entity, &mut ItemInUse, &Inventory, Option<&AbstractLivingUsingItem>, Has<Dead>)>,
    dropped: Option<Res<DroppedTicks>>,
) {
    let dropped_now = dropped.as_deref().map_or(0, |d| d.0);
    for (entity, mut item, inventory, flag, dead) in &mut query {
        // Ticks dropped before this one, once the flag was known, were server ticks of the use
        // (the server's `LivingEntity#updatingUsingItem` runs on its own clock).
        if item.confirmed {
            item.lost += u32::try_from(dropped_now.saturating_sub(item.dropped_seen)).unwrap_or(u32::MAX);
        }
        item.dropped_seen = dropped_now;
        let flagged = flag.is_some_and(|f| f.0);
        if flagged && !item.confirmed {
            item.confirmed = true;
            item.confirmed_at = Some(item.ticks);
        }
        let ended = dead
            || (item.confirmed && !flagged)
            || (!item.confirmed && item.ticks >= UNCONFIRMED_TIMEOUT_TICKS)
            || (item.hand == InteractionHand::MainHand && inventory.selected_hotbar_slot != item.slot);
        if ended {
            commands.entity(entity).remove::<ItemInUse>();
        } else {
            item.ticks += 1;
        }
    }
}
