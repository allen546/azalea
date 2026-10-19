pub mod equipment_effects;

use azalea_chat::FormattedText;
use azalea_core::tick::GameTick;
use azalea_entity::{
    Crouching, Jumping, PlayerAbilities, inventory::Inventory as Inv, metadata::Sprinting,
};
use azalea_physics::client_movement::{ClientMovementState, WalkDirection};
use azalea_inventory::operations::ClickOperation;
pub use azalea_inventory::*;
use azalea_protocol::packets::game::{
    s_container_click::{HashedStack, ServerboundContainerClick},
    s_container_close::ServerboundContainerClose,
    s_set_carried_item::ServerboundSetCarriedItem,
};
use azalea_registry::builtin::MenuKind;
use azalea_world::{WorldName, Worlds};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use indexmap::IndexMap;
use tracing::warn;

use crate::{
    inventory::equipment_effects::{collect_equipment_changes, handle_equipment_changes},
    mining::Mining,
    movement::ItemInUse,
    packet::game::SendGamePacketEvent,
    respawn::DeathScreen,
};

// TODO: when this is removed, remove the Inv alias above (which just exists to
// avoid conflicting with this pub deprecated type)
#[doc(hidden)]
#[deprecated = "moved to `azalea_entity::inventory::Inventory`."]
pub type Inventory = azalea_entity::inventory::Inventory;

pub struct InventoryPlugin;
impl Plugin for InventoryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            (
                ensure_has_sent_carried_item.after(super::mining::handle_mining_queued),
                collect_equipment_changes
                    .after(super::interact::handle_start_use_item_queued)
                    .before(azalea_physics::ai_step),
                tick_open_screen.after(super::movement::send_position),
            ),
        )
        .add_observer(handle_open_inventory_event)
        .add_observer(handle_client_side_close_container_trigger)
        .add_observer(handle_menu_opened_trigger)
        .add_observer(handle_container_close_event)
        .add_observer(handle_set_container_content_trigger)
        .add_observer(handle_container_click_event)
        // number keys are checked on tick but scrolling can happen outside of ticks, therefore
        // this is fine
        .add_observer(handle_set_selected_hotbar_slot_event)
        .add_observer(handle_equipment_changes);
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, SystemSet)]
pub struct InventorySystems;

/// A Bevy trigger that's fired when our client should show a new screen (like a
/// chest or crafting table).
///
/// To watch for the menu being closed, you could use
/// [`ClientsideCloseContainerEvent`]. To close it manually, use
/// [`CloseContainerEvent`].
#[derive(Clone, Debug, EntityEvent)]
pub struct MenuOpenedEvent {
    pub entity: Entity,
    pub window_id: i32,
    pub menu_type: MenuKind,
    pub title: FormattedText,
}
fn handle_menu_opened_trigger(
    event: On<MenuOpenedEvent>,
    mut commands: Commands,
    mut query: Query<&mut Inv>,
) {
    let mut inventory = query.get_mut(event.entity).unwrap();
    inventory.id = event.window_id;
    inventory.container_menu = Some(Menu::from_kind(event.menu_type));
    inventory.container_menu_title = Some(event.title.clone());
    commands.entity(event.entity).insert(OpenScreen { ticks: 0, kind: Some(event.menu_type) });
}

// Spec: specs/B3-inventory-crafting.md §4.1 (clicks only as a vanilla client makes them)
/// Null fork: a screen is open (the player's inventory or a container), as on a vanilla client.
///
/// While it is open the bot holds still: vanilla releases every key when a screen opens
/// (`Minecraft#setScreen`), so no movement, sprint or sneak input goes out. Clicks wait until the
/// tick after it opened, so the server (and Grim's `MultiActionsC`) has seen that input first.
#[derive(Clone, Component, Debug, Default)]
pub struct OpenScreen {
    /// Ticks since it opened (0 on the tick it opened).
    pub ticks: u32,
    /// The container's menu kind; `None` for the player's own inventory.
    pub kind: Option<MenuKind>,
}

/// Null fork: open the player's own inventory, as vanilla's inventory key does. Vanilla sends no
/// packet for this. Refused (see [`ClickResults::open_refusal`]) while a screen is already open,
/// a block is being mined or an item is in use.
#[derive(EntityEvent)]
pub struct OpenInventoryEvent {
    pub entity: Entity,
}

/// Null fork: why a click or an open was refused on the client. Nothing was sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClickRefusal {
    /// No screen is open; a vanilla client can't click without one.
    NoScreen,
    /// The screen opened this tick; the still input must reach the server first.
    ScreenJustOpened,
    /// Still sprinting or crouching.
    NotStill,
    /// The click names another window than the open one.
    WrongWindow,
    /// A screen is already open (for an open).
    AlreadyOpen,
    /// Mining a block or using an item (for an open).
    Busy,
    /// Dead: the death screen replaced every other screen.
    Dead,
}

impl ClickRefusal {
    pub fn reason(self) -> &'static str {
        match self {
            ClickRefusal::NoScreen => "no screen open",
            ClickRefusal::ScreenJustOpened => "screen opened this tick",
            ClickRefusal::NotStill => "still sprinting or crouching",
            ClickRefusal::WrongWindow => "another window is open",
            ClickRefusal::AlreadyOpen => "a screen is already open",
            ClickRefusal::Busy => "mining or using an item",
            ClickRefusal::Dead => "dead",
        }
    }
}

/// Null fork: the latest container click and what the server said after it.
///
/// After a click, vanilla's server sends a slot only where its own result differs from what the
/// click reported (`ServerGamePacketListenerImpl#handleContainerClick`: the click's slots and
/// hashes become the remote state, then `broadcastChanges`), and the whole menu when the state id
/// was stale. So server updates after a click are corrections: a wrong prediction or a wrong
/// component hash. Server-side changes of their own (a pickup, a crafting result) show up too.
#[derive(Clone, Component, Debug, Default)]
pub struct ClickResults {
    pub last: Option<ClickResult>,
    /// Set when the latest open was refused; cleared by a successful open.
    pub open_refusal: Option<ClickRefusal>,
}

#[derive(Clone, Debug)]
pub struct ClickResult {
    pub window_id: i32,
    /// The clicked slot (-999 for outside the window).
    pub slot_num: i16,
    /// None when the click was sent.
    pub refusal: Option<ClickRefusal>,
    /// Ticks since the click.
    pub ticks: u32,
    /// Slots the server set after the click, in this window's numbering; -1 is the cursor.
    pub corrected_slots: Vec<i16>,
    /// Whole-window resends after the click (a stale state id, or the server resynced).
    pub full_resends: u32,
}

impl ClickResults {
    /// Notes a server update of `slot` (-1 the cursor; None the whole window) after our click.
    pub fn note_server_update(&mut self, slot: Option<i16>) {
        if let Some(r) = self.last.as_mut().filter(|r| r.refusal.is_none()) {
            match slot {
                Some(s) => r.corrected_slots.push(s),
                None => r.full_resends += 1,
            }
        }
    }
}

fn handle_open_inventory_event(
    event: On<OpenInventoryEvent>,
    mut commands: Commands,
    mut query: Query<(&Inv, Has<OpenScreen>, Has<Mining>, Has<ItemInUse>, Has<DeathScreen>, Option<&mut ClickResults>)>,
) {
    let Ok((inventory, open, mining, using, dead, results)) = query.get_mut(event.entity) else {
        return;
    };
    let refusal = if dead {
        Some(ClickRefusal::Dead)
    } else if open || inventory.id != 0 {
        Some(ClickRefusal::AlreadyOpen)
    } else if mining || using {
        Some(ClickRefusal::Busy)
    } else {
        None
    };
    match results {
        Some(mut r) => r.open_refusal = refusal,
        None => {
            commands.entity(event.entity).insert(ClickResults { last: None, open_refusal: refusal });
        }
    }
    if refusal.is_none() {
        commands.entity(event.entity).insert(OpenScreen::default());
    }
}

/// Null fork: no movement, sprint, sneak or jump input while a screen is open (see
/// [`OpenScreen`]). Runs first in the physics tick, after everything that sets input.
pub fn hold_still_while_screen_open(
    mut query: Query<
        (&mut ClientMovementState, &mut Jumping),
        Or<(With<OpenScreen>, With<super::respawn::DeathScreen>)>,
    >,
) {
    for (mut mv, mut jumping) in &mut query {
        mv.move_direction = WalkDirection::None;
        mv.trying_to_sprint = false;
        mv.trying_to_crouch = false;
        **jumping = false;
    }
}

fn tick_open_screen(mut screens: Query<&mut OpenScreen>, mut results: Query<&mut ClickResults>) {
    for mut s in &mut screens {
        s.ticks = s.ticks.saturating_add(1);
    }
    for mut r in &mut results {
        if let Some(last) = r.last.as_mut() {
            last.ticks = last.ticks.saturating_add(1);
        }
    }
}

/// Tell the server that we want to close a container.
///
/// Note that this is also sent when the client closes its own inventory, even
/// though there is no packet for opening its inventory.
#[derive(EntityEvent)]
pub struct CloseContainerEvent {
    pub entity: Entity,
    /// The ID of the container to close. 0 for the player's inventory.
    ///
    /// If this is not the same as the currently open inventory, nothing will
    /// happen.
    pub id: i32,
}
fn handle_container_close_event(
    close_container: On<CloseContainerEvent>,
    mut commands: Commands,
    query: Query<(Entity, &Inv, Has<OpenScreen>)>,
) {
    let (entity, inventory, open) = query.get(close_container.entity).unwrap();
    // Null fork: a vanilla client sends a close only when it closes a screen.
    if !open {
        warn!("Tried to close container {} with no screen open", close_container.id);
        return;
    }
    if close_container.id != inventory.id {
        warn!(
            "Tried to close container with ID {}, but the current container ID is {}",
            close_container.id, inventory.id
        );
        return;
    }

    commands.trigger(SendGamePacketEvent::new(
        entity,
        ServerboundContainerClose {
            container_id: inventory.id,
        },
    ));
    commands.trigger(ClientsideCloseContainerEvent {
        entity: close_container.entity,
    });
}

/// A Bevy event that's fired when our client closed a container.
///
/// This can also be triggered directly to close a container silently without
/// sending any packets to the server. You probably don't want that though, and
/// should instead use [`CloseContainerEvent`].
///
/// If you want to watch for a container being opened, you should use
/// [`MenuOpenedEvent`].
#[derive(Clone, EntityEvent)]
pub struct ClientsideCloseContainerEvent {
    pub entity: Entity,
}
pub fn handle_client_side_close_container_trigger(
    event: On<ClientsideCloseContainerEvent>,
    mut commands: Commands,
    mut query: Query<&mut Inv>,
) {
    let mut inventory = query.get_mut(event.entity).unwrap();
    commands.entity(event.entity).remove::<OpenScreen>();

    // copy the Player part of the container_menu to the inventory_menu
    if let Some(inventory_menu) = inventory.container_menu.take() {
        // this isn't the same as what vanilla does. i believe vanilla synchronizes the
        // slots between inventoryMenu and containerMenu by just having the player slots
        // point to the same ItemStack in memory, but emulating this in rust would
        // require us to wrap our `ItemStack`s as `Arc<Mutex<ItemStack>>` which would
        // have kinda terrible ergonomics.

        // the simpler solution i chose to go with here is to only copy the player slots
        // when the container is closed. this is perfectly fine for vanilla, but it
        // might cause issues if a server modifies id 0 while we have a container
        // open...

        // if we do encounter this issue in the wild then the simplest solution would
        // probably be to just add logic for updating the container_menu when the server
        // tries to modify id 0 for slots within `inventory`. not implemented for now
        // because i'm not sure if that's worth worrying about.

        let new_inventory = inventory_menu.slots()[inventory_menu.player_slots_range()].to_vec();
        let new_inventory = <[ItemStack; 36]>::try_from(new_inventory).unwrap();
        *inventory.inventory_menu.as_player_mut().inventory = new_inventory;
        // Null fork: the cursor stack belongs to a menu in vanilla; the player menu's own is
        // empty. The server puts a stack that was on the container's cursor back in the
        // inventory (`AbstractContainerMenu#removed`) and sends no cursor update for menu 0.
        inventory.carried = ItemStack::Empty;
    }

    inventory.id = 0;
    inventory.container_menu_title = None;
}

#[derive(Debug, EntityEvent)]
pub struct ContainerClickEvent {
    pub entity: Entity,
    pub window_id: i32,
    pub operation: ClickOperation,
}
// Spec: specs/B3-inventory-crafting.md §4.1 (clicks only as a vanilla client makes them)
#[allow(clippy::type_complexity)]
pub fn handle_container_click_event(
    container_click: On<ContainerClickEvent>,
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut Inv,
        Option<&PlayerAbilities>,
        &WorldName,
        Option<&OpenScreen>,
        Option<&Sprinting>,
        Option<&Crouching>,
        Option<&mut ClickResults>,
        Has<DeathScreen>,
    )>,
    worlds: Res<Worlds>,
) {
    let (entity, mut inventory, player_abilities, world_name, screen, sprinting, crouching, results, dead) =
        query.get_mut(container_click.entity).unwrap();
    let slot_num = container_click.operation.slot_num().map(|n| n as i16).unwrap_or(-999);
    // Null fork: refuse what a vanilla client couldn't send (see `OpenScreen`).
    let refusal = if dead {
        Some(ClickRefusal::Dead)
    } else if inventory.id != container_click.window_id {
        Some(ClickRefusal::WrongWindow)
    } else if screen.is_none() {
        Some(ClickRefusal::NoScreen)
    } else if screen.is_some_and(|s| s.ticks == 0) {
        Some(ClickRefusal::ScreenJustOpened)
    } else if sprinting.is_some_and(|s| **s) || crouching.is_some_and(|c| **c) {
        Some(ClickRefusal::NotStill)
    } else {
        None
    };
    let result = ClickResult {
        window_id: container_click.window_id,
        slot_num,
        refusal,
        ticks: 0,
        corrected_slots: Vec::new(),
        full_resends: 0,
    };
    match results {
        Some(mut r) => r.last = Some(result),
        None => {
            commands.entity(entity).insert(ClickResults { last: Some(result), open_refusal: None });
        }
    }
    if let Some(refusal) = refusal {
        warn!("Container click refused on the client: {}", refusal.reason());
        return;
    }

    let Some(world) = worlds.get(world_name) else {
        return;
    };

    let old_slots = inventory.menu().slots();
    inventory.simulate_click(
        &container_click.operation,
        player_abilities.unwrap_or(&PlayerAbilities::default()),
    );
    let new_slots = inventory.menu().slots();
    // Null fork: the player menu always holds the inventory (vanilla shares it between menus).
    inventory.sync_player_slots_from_container();

    let registry_holder = &world.read().registries;

    // see which slots changed after clicking and put them in the map the server
    // uses this to check if we desynced
    let mut changed_slots: IndexMap<u16, HashedStack> = IndexMap::new();
    for (slot_index, old_slot) in old_slots.iter().enumerate() {
        let new_slot = &new_slots[slot_index];
        if old_slot != new_slot {
            changed_slots.insert(
                slot_index as u16,
                HashedStack::from_item_stack(new_slot, registry_holder),
            );
        }
    }
    // An offhand swap from a container changes a slot that window doesn't have; vanilla reports
    // only the window's own slots too.

    commands.trigger(SendGamePacketEvent::new(
        entity,
        ServerboundContainerClick {
            container_id: container_click.window_id,
            state_id: inventory.state_id,
            slot_num,
            button_num: container_click.operation.button_num(),
            click_type: container_click.operation.click_type(),
            changed_slots,
            carried_item: HashedStack::from_item_stack(&inventory.carried, registry_holder),
        },
    ));
}

/// Sent from the server when the contents of a container are replaced.
///
/// Usually triggered by the `ContainerSetContent` packet.
#[derive(EntityEvent)]
pub struct SetContainerContentEvent {
    pub entity: Entity,
    pub slots: Vec<ItemStack>,
    pub container_id: i32,
}
pub fn handle_set_container_content_trigger(
    set_container_content: On<SetContainerContentEvent>,
    mut query: Query<&mut Inv>,
) {
    let mut inventory = query.get_mut(set_container_content.entity).unwrap();

    if set_container_content.container_id != inventory.id {
        warn!(
            "Got SetContainerContentEvent for container with ID {}, but the current container ID is {}",
            set_container_content.container_id, inventory.id
        );
        return;
    }

    let menu = inventory.menu_mut();
    for (i, slot) in set_container_content.slots.iter().enumerate() {
        if let Some(slot_mut) = menu.slot_mut(i) {
            *slot_mut = slot.clone();
        }
    }
    inventory.sync_player_slots_from_container();
}

/// An ECS message to switch our hand to a different hotbar slot.
///
/// This is equivalent to using the scroll wheel or number keys in Minecraft.
#[derive(EntityEvent)]
pub struct SetSelectedHotbarSlotEvent {
    pub entity: Entity,
    /// The hotbar slot to select. This should be in the range 0..=8.
    pub slot: u8,
}
pub fn handle_set_selected_hotbar_slot_event(
    set_selected_hotbar_slot: On<SetSelectedHotbarSlotEvent>,
    mut query: Query<&mut Inv>,
) {
    let mut inventory = query.get_mut(set_selected_hotbar_slot.entity).unwrap();
    inventory.selected_hotbar_slot = set_selected_hotbar_slot.slot;
}

/// The item slot that the server thinks we have selected.
///
/// See [`ensure_has_sent_carried_item`].
#[derive(Component)]
pub struct LastSentSelectedHotbarSlot {
    pub slot: u8,
}
/// A system that makes sure that [`LastSentSelectedHotbarSlot`] is in sync with
/// [`Inv::selected_hotbar_slot`].
///
/// This is necessary to make sure that [`ServerboundSetCarriedItem`] is sent in
/// the right order, since it's not allowed to happen outside of a tick.
pub fn ensure_has_sent_carried_item(
    mut commands: Commands,
    query: Query<(Entity, &Inv, Option<&LastSentSelectedHotbarSlot>)>,
) {
    for (entity, inventory, last_sent) in query.iter() {
        if let Some(last_sent) = last_sent {
            if last_sent.slot == inventory.selected_hotbar_slot {
                continue;
            }

            commands.trigger(SendGamePacketEvent::new(
                entity,
                ServerboundSetCarriedItem {
                    slot: inventory.selected_hotbar_slot as u16,
                },
            ));
        }

        commands.entity(entity).insert(LastSentSelectedHotbarSlot {
            slot: inventory.selected_hotbar_slot,
        });
    }
}
