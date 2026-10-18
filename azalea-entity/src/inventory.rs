use std::{cmp, collections::HashSet};

use azalea_chat::FormattedText;
use azalea_inventory::{
    ItemStack, ItemStackData, Menu,
    components::EquipmentSlot,
    item::MaxStackSizeExt,
    operations::{
        ClickOperation, CloneClick, PickupAllClick, PickupClick, QuickCraftKind, QuickCraftStatus,
        QuickCraftStatusKind, QuickMoveClick, ThrowClick,
    },
};

use crate::PlayerAbilities;

/// A local player's inventory and related data, including the container that
/// they may have opened.
#[cfg_attr(feature = "bevy_ecs", derive(bevy_ecs::component::Component))]
#[derive(Clone, Debug)]
pub struct Inventory {
    /// The player's inventory menu. This is guaranteed to be a `Menu::Player`.
    ///
    /// We keep it as a [`Menu`] since `Menu` has some useful functions that
    /// bare [`azalea_inventory::Player`] doesn't have.
    pub inventory_menu: azalea_inventory::Menu,

    /// The ID of the container that's currently open.
    ///
    /// Its value is not guaranteed to be anything specific, and it may change
    /// every time you open a container (unless it's 0, in which case it
    /// means that no container is open).
    pub id: i32,
    /// The current container menu that the player has open, or `None` if no
    /// container is open.
    pub container_menu: Option<azalea_inventory::Menu>,
    /// The custom name of the menu that's currently open.
    ///
    /// This can only be `Some` when `container_menu` is `Some`.
    pub container_menu_title: Option<FormattedText>,
    /// The item that is currently held by the cursor, or `Slot::Empty` if
    /// nothing is currently being held.
    ///
    /// This is different from [`Self::selected_hotbar_slot`], which is the
    /// item that's selected in the hotbar.
    pub carried: ItemStack,
    /// An identifier used by the server to track client inventory desyncs.
    ///
    /// This is sent on every container click, and it's only ever updated when
    /// the server sends a new container update.
    pub state_id: u32,

    pub quick_craft_status: QuickCraftStatusKind,
    pub quick_craft_kind: QuickCraftKind,
    /// A set of the indexes of the slots that have been right clicked in
    /// this "quick craft".
    pub quick_craft_slots: HashSet<u16>,

    /// The index of the item in the hotbar that's currently being held by the
    /// player. This must be in the range 0..=8.
    ///
    /// In a vanilla client this is changed by pressing the number keys or using
    /// the scroll wheel.
    pub selected_hotbar_slot: u8,
}

impl Inventory {
    /// Returns a reference to the currently active menu.
    ///
    /// If a container is open then it'll return [`Self::container_menu`],
    /// otherwise [`Self::inventory_menu`].
    ///
    /// Use [`Self::menu_mut`] if you need a mutable reference.
    pub fn menu(&self) -> &azalea_inventory::Menu {
        match &self.container_menu {
            Some(menu) => menu,
            _ => &self.inventory_menu,
        }
    }

    /// Returns a mutable reference to the currently active menu.
    ///
    /// If a container is open then it'll return [`Self::container_menu`],
    /// otherwise [`Self::inventory_menu`].
    ///
    /// Use [`Self::menu`] if you don't need a mutable reference.
    pub fn menu_mut(&mut self) -> &mut azalea_inventory::Menu {
        match &mut self.container_menu {
            Some(menu) => menu,
            _ => &mut self.inventory_menu,
        }
    }

    /// Modify the inventory as if the given operation was performed on it.
    pub fn simulate_click(
        &mut self,
        operation: &ClickOperation,
        player_abilities: &PlayerAbilities,
    ) {
        if let ClickOperation::QuickCraft(quick_craft) = operation {
            let last_quick_craft_status_tmp = self.quick_craft_status.clone();
            self.quick_craft_status = last_quick_craft_status_tmp.clone();
            let last_quick_craft_status = last_quick_craft_status_tmp;

            // no carried item, reset
            if self.carried.is_empty() {
                return self.reset_quick_craft();
            }
            // if we were starting or ending, or now we aren't ending and the status
            // changed, reset
            if (last_quick_craft_status == QuickCraftStatusKind::Start
                || last_quick_craft_status == QuickCraftStatusKind::End
                || self.quick_craft_status != QuickCraftStatusKind::End)
                && (self.quick_craft_status != last_quick_craft_status)
            {
                return self.reset_quick_craft();
            }
            if self.quick_craft_status == QuickCraftStatusKind::Start {
                self.quick_craft_kind = quick_craft.kind.clone();
                if self.quick_craft_kind == QuickCraftKind::Middle && player_abilities.instant_break
                {
                    self.quick_craft_status = QuickCraftStatusKind::Add;
                    self.quick_craft_slots.clear();
                } else {
                    self.reset_quick_craft();
                }
                return;
            }
            if let QuickCraftStatus::Add { slot } = quick_craft.status {
                let slot_item = self.menu().slot(slot as usize);
                if let Some(slot_item) = slot_item
                    && let ItemStack::Present(carried) = &self.carried
                {
                    // minecraft also checks slot.may_place(carried) and
                    // menu.can_drag_to(slot)
                    // but they always return true so they're not relevant for us
                    if can_item_quick_replace(slot_item, &self.carried, true)
                        && (self.quick_craft_kind == QuickCraftKind::Right
                            || carried.count as usize > self.quick_craft_slots.len())
                    {
                        self.quick_craft_slots.insert(slot);
                    }
                }
                return;
            }
            if self.quick_craft_status == QuickCraftStatusKind::End {
                if !self.quick_craft_slots.is_empty() {
                    if self.quick_craft_slots.len() == 1 {
                        // if we only clicked one slot, then turn this
                        // QuickCraftClick into a PickupClick
                        let slot = *self.quick_craft_slots.iter().next().unwrap();
                        self.reset_quick_craft();
                        self.simulate_click(
                            &match self.quick_craft_kind {
                                QuickCraftKind::Left => {
                                    PickupClick::Left { slot: Some(slot) }.into()
                                }
                                QuickCraftKind::Right => {
                                    PickupClick::Left { slot: Some(slot) }.into()
                                }
                                QuickCraftKind::Middle => {
                                    // idk just do nothing i guess
                                    return;
                                }
                            },
                            player_abilities,
                        );
                        return;
                    }

                    let ItemStack::Present(mut carried) = self.carried.clone() else {
                        // this should never happen
                        return self.reset_quick_craft();
                    };

                    let mut carried_count = carried.count;
                    let mut quick_craft_slots_iter = self.quick_craft_slots.iter();

                    loop {
                        let mut slot: &ItemStack;
                        let mut slot_index: u16;
                        let mut item_stack: &ItemStack;

                        loop {
                            let Some(&next_slot) = quick_craft_slots_iter.next() else {
                                carried.count = carried_count;
                                self.carried = ItemStack::Present(carried);
                                return self.reset_quick_craft();
                            };

                            slot = self.menu().slot(next_slot as usize).unwrap();
                            slot_index = next_slot;
                            item_stack = &self.carried;

                            if slot.is_present()
                                    && can_item_quick_replace(slot, item_stack, true)
                                    // this always returns true in most cases
                                    // && slot.may_place(item_stack)
                                    && (
                                        self.quick_craft_kind == QuickCraftKind::Middle
                                        || item_stack.count()  >= self.quick_craft_slots.len() as i32
                                    )
                            {
                                break;
                            }
                        }

                        // get the ItemStackData for the slot
                        let ItemStack::Present(slot) = slot else {
                            unreachable!("the loop above requires the slot to be present to break")
                        };

                        // if self.can_drag_to(slot) {
                        let mut new_carried = carried.clone();
                        let slot_item_count = slot.count;
                        get_quick_craft_slot_count(
                            &self.quick_craft_slots,
                            &self.quick_craft_kind,
                            &mut new_carried,
                            slot_item_count,
                        );
                        let max_stack_size = i32::min(
                            new_carried.kind.max_stack_size(),
                            i32::min(
                                new_carried.kind.max_stack_size(),
                                slot.kind.max_stack_size(),
                            ),
                        );
                        if new_carried.count > max_stack_size {
                            new_carried.count = max_stack_size;
                        }

                        carried_count -= new_carried.count - slot_item_count;
                        // we have to inline self.menu_mut() here to avoid the borrow checker
                        // complaining
                        let menu = match &mut self.container_menu {
                            Some(menu) => menu,
                            _ => &mut self.inventory_menu,
                        };
                        *menu.slot_mut(slot_index as usize).unwrap() =
                            ItemStack::Present(new_carried);
                    }
                }
            } else {
                return self.reset_quick_craft();
            }
        }
        // the quick craft status should always be in start if we're not in quick craft
        // mode
        if self.quick_craft_status != QuickCraftStatusKind::Start {
            return self.reset_quick_craft();
        }

        match operation {
            // left clicking outside inventory
            // Null fork: `LeftOutside`/`RightOutside` are the same clicks (slot -999).
            ClickOperation::Pickup(PickupClick::Left { slot: None } | PickupClick::LeftOutside)
                if self.carried.is_present() =>
            {
                // vanilla has `player.drop`s but they're only used
                // server-side
                // they're included as comments here in case you want to adapt this for a server
                // implementation

                // player.drop(self.carried, true);
                self.carried = ItemStack::Empty;
            }
            ClickOperation::Pickup(PickupClick::Right { slot: None } | PickupClick::RightOutside)
                if self.carried.is_present() =>
            {
                let _item = self.carried.split(1);
                // player.drop(item, true);
            }
            &ClickOperation::Pickup(
                // lol
                ref pickup @ (PickupClick::Left { slot: Some(slot) }
                | PickupClick::Right { slot: Some(slot) }),
            ) => {
                let slot = slot as usize;
                let Some(slot_item) = self.menu().slot(slot) else {
                    return;
                };

                if self.try_item_click_behavior_override(operation, slot) {
                    return;
                }

                let is_left_click = matches!(pickup, PickupClick::Left { .. });

                match slot_item {
                    ItemStack::Empty => {
                        if self.carried.is_present() {
                            let place_count = if is_left_click {
                                self.carried.count()
                            } else {
                                1
                            };
                            self.carried =
                                self.safe_insert(slot, self.carried.clone(), place_count);
                        }
                    }
                    ItemStack::Present(_) => {
                        if !self.menu().may_pickup(slot) {
                            return;
                        }
                        if let ItemStack::Present(carried) = self.carried.clone() {
                            let slot_is_same_item_as_carried = slot_item
                                .as_present()
                                .is_some_and(|s| carried.is_same_item_and_components(s));

                            if self.menu().may_place(slot, &carried) {
                                if slot_is_same_item_as_carried {
                                    let place_count = if is_left_click { carried.count } else { 1 };
                                    self.carried =
                                        self.safe_insert(slot, self.carried.clone(), place_count);
                                } else if carried.count <= self.menu().max_stack_size_for(slot, &carried) {
                                    // swap slot_item and carried
                                    self.carried = slot_item.clone();
                                    let slot_item = self.menu_mut().slot_mut(slot).unwrap();
                                    *slot_item = carried.into();
                                }
                            } else if slot_is_same_item_as_carried
                                && let Some(removed) = self.try_remove(
                                    slot,
                                    slot_item.count(),
                                    carried.max_stack_size() - carried.count,
                                )
                            {
                                self.carried.as_present_mut().unwrap().count += removed.count();
                                // slot.onTake(player, removed);
                            }
                        } else {
                            let pickup_count = if is_left_click {
                                slot_item.count()
                            } else {
                                (slot_item.count() + 1) / 2
                            };
                            if let Some(new_slot_item) =
                                self.try_remove(slot, pickup_count, i32::MAX)
                            {
                                self.carried = new_slot_item;
                                // slot.onTake(player, newSlot);
                            }
                        }
                    }
                }
            }
            &ClickOperation::QuickMove(
                QuickMoveClick::Left { slot } | QuickMoveClick::Right { slot },
            ) => {
                // in vanilla it also tests if QuickMove has a slot index of -999
                // but i don't think that's ever possible so it's not covered here
                let slot = slot as usize;
                loop {
                    let new_slot_item = self.menu_mut().quick_move_stack(slot);
                    let slot_item = self.menu().slot(slot).unwrap();
                    if new_slot_item.is_empty() || slot_item.kind() != new_slot_item.kind() {
                        break;
                    }
                }
            }
            // Null fork: vanilla `AbstractContainerMenu#doClick` SWAP. The button names a player
            // inventory slot (0-8 hotbar, 40 offhand), not a menu slot.
            ClickOperation::Swap(s) => {
                let source_index = s.source_slot as usize;
                let button = s.target_slot as usize;
                if !(button < 9 || button == 40) {
                    return;
                }
                let (Some(source), Some(target)) =
                    (self.menu().slot(source_index).cloned(), self.player_slot(button).cloned())
                else {
                    return;
                };
                match (&source, &target) {
                    (ItemStack::Empty, ItemStack::Empty) => {}
                    (_, ItemStack::Empty) => {
                        if self.menu().may_pickup(source_index) {
                            *self.player_slot_mut(button).unwrap() = source;
                            *self.menu_mut().slot_mut(source_index).unwrap() = ItemStack::Empty;
                        }
                    }
                    (ItemStack::Empty, ItemStack::Present(t)) => {
                        if self.menu().may_place(source_index, t) {
                            let max = self.menu().max_stack_size_for(source_index, t);
                            let mut rest = target.clone();
                            let moved = rest.split(max as u32);
                            *self.menu_mut().slot_mut(source_index).unwrap() = moved;
                            *self.player_slot_mut(button).unwrap() = rest;
                        }
                    }
                    (ItemStack::Present(_), ItemStack::Present(t)) => {
                        if self.menu().may_pickup(source_index) && self.menu().may_place(source_index, t) {
                            let max = self.menu().max_stack_size_for(source_index, t);
                            if t.count > max {
                                // vanilla then adds the old source item anywhere in the
                                // inventory (`Inventory#add`); not simulated, the server corrects it
                                let mut rest = target.clone();
                                *self.menu_mut().slot_mut(source_index).unwrap() = rest.split(max as u32);
                                *self.player_slot_mut(button).unwrap() = rest;
                            } else {
                                *self.player_slot_mut(button).unwrap() = source;
                                *self.menu_mut().slot_mut(source_index).unwrap() = target;
                            }
                        }
                    }
                }
            }
            ClickOperation::Clone(CloneClick { slot }) => {
                if !player_abilities.instant_break || self.carried.is_present() {
                    return;
                }
                let Some(source_slot) = self.menu().slot(*slot as usize) else {
                    return;
                };
                let ItemStack::Present(source_item) = source_slot else {
                    return;
                };
                let mut new_carried = source_item.clone();
                new_carried.count = new_carried.kind.max_stack_size();
                self.carried = ItemStack::Present(new_carried);
            }
            ClickOperation::Throw(c) => {
                if self.carried.is_present() {
                    return;
                }

                let (ThrowClick::Single { slot: slot_index }
                | ThrowClick::All { slot: slot_index }) = c;
                let slot_index = *slot_index as usize;

                let Some(slot) = self.menu_mut().slot_mut(slot_index) else {
                    return;
                };
                let dropping_count = match c {
                    ThrowClick::Single { .. } => 1,
                    ThrowClick::All { .. } => slot.count(),
                };

                // Null fork: `ItemStack::split` empties the slot when the last item goes, so the
                // click reports an empty slot, not a 0-count one.
                let _dropping = slot.split(dropping_count as u32);
                // player.drop(dropping, true);
            }
            // Null fork: vanilla `doClick` PICKUP_ALL. Takes only what fits on the cursor, and
            // never from a crafting result.
            ClickOperation::PickupAll(PickupAllClick {
                slot: source_slot_index,
                reversed,
            }) => {
                let source_slot_index = *source_slot_index as usize;
                let Some(source_slot) = self.menu().slot(source_slot_index) else {
                    return;
                };
                if self.carried.is_empty()
                    || (source_slot.is_present() && self.menu().may_pickup(source_slot_index))
                {
                    return;
                }
                let len = self.menu().len();
                let order: Vec<usize> = if *reversed { (0..len).rev().collect() } else { (0..len).collect() };
                let no_pick_all = match self.menu() {
                    Menu::Player(_) | Menu::Crafting { .. } => Some(0),
                    _ => None,
                };
                for round in 0..2 {
                    for &i in &order {
                        let ItemStack::Present(carried) = &self.carried else { unreachable!() };
                        let room = carried.max_stack_size() - carried.count;
                        if room <= 0 {
                            return;
                        }
                        let checking_slot = self.menu().slot(i).unwrap();
                        if let ItemStack::Present(checking_item) = checking_slot
                            && Some(i) != no_pick_all
                            && can_item_quick_replace(checking_slot, &self.carried, true)
                            && self.menu().may_pickup(i)
                            && (round != 0 || checking_item.count != checking_item.max_stack_size())
                        {
                            let take = checking_item.count.min(room);
                            let taken = self.menu_mut().slot_mut(i).unwrap().split(take as u32);
                            self.carried.as_present_mut().unwrap().count += taken.count();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Null fork: player inventory numbering (vanilla `Inventory`) as the server uses it in swap
    // clicks and `ClientboundSetPlayerInventory`.
    /// The player-menu index of player inventory slot `i`: 0-8 hotbar, 9-35 main, 36-39 feet to
    /// head, 40 offhand.
    pub fn player_menu_index(i: usize) -> Option<usize> {
        Some(match i {
            0..=8 => 36 + i,
            9..=35 => i,
            36..=39 => 8 - (i - 36),
            40 => 45,
            _ => return None,
        })
    }

    /// Player inventory slot `i`, read from the open container's copy for the main inventory and
    /// hotbar (the copy the click changes), else from the player menu.
    pub fn player_slot(&self, i: usize) -> Option<&ItemStack> {
        match (&self.container_menu, i) {
            (Some(menu), 0..=35) => menu.slot(container_player_index(menu, i)),
            _ => self.inventory_menu.slot(Self::player_menu_index(i)?),
        }
    }

    pub fn player_slot_mut(&mut self, i: usize) -> Option<&mut ItemStack> {
        match (&mut self.container_menu, i) {
            (Some(menu), 0..=35) => {
                let j = container_player_index(menu, i);
                menu.slot_mut(j)
            }
            _ => self.inventory_menu.slot_mut(Self::player_menu_index(i)?),
        }
    }

    /// Copies the open container's player slots into the player menu, which then always holds
    /// the inventory (vanilla shares one `Inventory` between both menus).
    pub fn sync_player_slots_from_container(&mut self) {
        if let Some(menu) = &self.container_menu {
            let slots = menu.slots()[menu.player_slots_range()].to_vec();
            for (k, item) in slots.into_iter().enumerate() {
                *self.inventory_menu.slot_mut(9 + k).unwrap() = item;
            }
        }
    }

    fn reset_quick_craft(&mut self) {
        self.quick_craft_status = QuickCraftStatusKind::Start;
        self.quick_craft_slots.clear();
    }

    /// Get the item in the player's hotbar that is currently being held in
    /// their main hand.
    pub fn held_item(&self) -> &ItemStack {
        self.get_equipment(EquipmentSlot::Mainhand)
            .expect("The main hand item should always be present")
    }

    /// TODO: implement bundles
    fn try_item_click_behavior_override(
        &self,
        _operation: &ClickOperation,
        _slot_item_index: usize,
    ) -> bool {
        false
    }

    // Null fork: vanilla `Slot#safeInsert` checks `mayPlace` and the slot's own limit.
    fn safe_insert(&mut self, slot: usize, src_item: ItemStack, take_count: i32) -> ItemStack {
        let ItemStack::Present(mut src_item) = src_item else {
            return src_item;
        };
        if !self.menu().may_place(slot, &src_item) {
            return src_item.into();
        }
        let max = self.menu().max_stack_size_for(slot, &src_item);
        let Some(slot_item) = self.menu_mut().slot_mut(slot) else {
            return src_item.into();
        };

        let take_count = cmp::min(cmp::min(take_count, src_item.count), max - slot_item.count());
        if take_count <= 0 {
            return src_item.into();
        }
        let take_count = take_count as u32;

        if slot_item.is_empty() {
            *slot_item = src_item.split(take_count).into();
        } else if let ItemStack::Present(slot_item) = slot_item
            && slot_item.is_same_item_and_components(&src_item)
        {
            src_item.count -= take_count as i32;
            slot_item.count += take_count as i32;
        }

        src_item.into()
    }

    fn try_remove(&mut self, slot: usize, count: i32, limit: i32) -> Option<ItemStack> {
        if !self.menu().may_pickup(slot) {
            return None;
        }
        let mut slot_item = self.menu().slot(slot)?.clone();
        if !self.menu().allow_modification(slot) && limit < slot_item.count() {
            return None;
        }

        let count = count.min(limit);
        if count <= 0 {
            return None;
        }
        // vanilla calls .remove here but i think it has the same behavior as split?
        let removed = slot_item.split(count as u32);

        // Null fork: the slot keeps what's left (vanilla `Slot#remove`), not just when emptied.
        *self.menu_mut().slot_mut(slot).unwrap() = slot_item;

        Some(removed)
    }

    /// Get the item at the given equipment slot, or `None` if the inventory
    /// can't contain that slot.
    pub fn get_equipment(&self, equipment_slot: EquipmentSlot) -> Option<&ItemStack> {
        let player = self.inventory_menu.as_player();
        let item = match equipment_slot {
            EquipmentSlot::Mainhand => {
                let menu = self.menu();
                let main_hand_slot_idx =
                    *menu.hotbar_slots_range().start() + self.selected_hotbar_slot as usize;
                menu.slot(main_hand_slot_idx)?
            }
            EquipmentSlot::Offhand => &player.offhand,
            EquipmentSlot::Feet => &player.armor[3],
            EquipmentSlot::Legs => &player.armor[2],
            EquipmentSlot::Chest => &player.armor[1],
            EquipmentSlot::Head => &player.armor[0],
            EquipmentSlot::Body => {
                // TODO: when riding entities is implemented, mount/horse inventories should be
                // implemented too. note that horse inventories aren't a normal menu (they're
                // not in MenuKind), maybe they should be a separate field in `Inventory`?
                return None;
            }
            EquipmentSlot::Saddle => {
                // TODO: implement riding entities, see above
                return None;
            }
        };
        Some(item)
    }
}

/// Container-menu index of player inventory slot `i` (0-35); container menus list the main
/// inventory, then the hotbar.
fn container_player_index(menu: &Menu, i: usize) -> usize {
    let start = *menu.player_slots_range().start();
    if i < 9 { start + 27 + i } else { start + i - 9 }
}

fn can_item_quick_replace(
    target_slot: &ItemStack,
    item: &ItemStack,
    ignore_item_count: bool,
) -> bool {
    let ItemStack::Present(target_slot) = target_slot else {
        return false;
    };
    let ItemStack::Present(item) = item else {
        // i *think* this is what vanilla does
        // not 100% sure lol probably doesn't matter though
        return false;
    };

    if !item.is_same_item_and_components(target_slot) {
        return false;
    }
    let count = target_slot.count as u16
        + if ignore_item_count {
            0
        } else {
            item.count as u16
        };
    count <= item.max_stack_size() as u16
}

fn get_quick_craft_slot_count(
    quick_craft_slots: &HashSet<u16>,
    quick_craft_kind: &QuickCraftKind,
    item: &mut ItemStackData,
    slot_item_count: i32,
) {
    item.count = match quick_craft_kind {
        QuickCraftKind::Left => item.count / quick_craft_slots.len() as i32,
        QuickCraftKind::Right => 1,
        QuickCraftKind::Middle => item.kind.max_stack_size(),
    };
    item.count += slot_item_count;
}

impl Default for Inventory {
    fn default() -> Self {
        Inventory {
            inventory_menu: Menu::Player(azalea_inventory::Player::default()),
            id: 0,
            container_menu: None,
            container_menu_title: None,
            carried: ItemStack::Empty,
            state_id: 0,
            quick_craft_status: QuickCraftStatusKind::Start,
            quick_craft_kind: QuickCraftKind::Middle,
            quick_craft_slots: HashSet::new(),
            selected_hotbar_slot: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use azalea_inventory::SlotList;
    use azalea_registry::builtin::ItemKind;

    use super::*;

    #[test]
    fn test_simulate_shift_click_in_crafting_table() {
        let spruce_planks = ItemStack::new(ItemKind::SprucePlanks, 4);

        let mut inventory = Inventory {
            inventory_menu: Menu::Player(azalea_inventory::Player::default()),
            id: 1,
            container_menu: Some(Menu::Crafting {
                result: spruce_planks.clone(),
                // simulate_click won't delete the items from here
                grid: SlotList::default(),
                player: SlotList::default(),
            }),
            container_menu_title: None,
            carried: ItemStack::Empty,
            state_id: 0,
            quick_craft_status: QuickCraftStatusKind::Start,
            quick_craft_kind: QuickCraftKind::Middle,
            quick_craft_slots: HashSet::new(),
            selected_hotbar_slot: 0,
        };

        inventory.simulate_click(
            &ClickOperation::QuickMove(QuickMoveClick::Left { slot: 0 }),
            &PlayerAbilities::default(),
        );

        let new_slots = inventory.menu().slots();
        assert_eq!(&new_slots[0], &ItemStack::Empty);
        assert_eq!(
            &new_slots[*Menu::CRAFTING_PLAYER_SLOTS.start()],
            &spruce_planks
        );
    }

    // Null fork: expected slots follow vanilla 1.21.11 `AbstractContainerMenu#doClick`.
    fn player(slots: &[(usize, ItemKind, i32)]) -> Inventory {
        let mut inv = Inventory::default();
        for &(i, kind, n) in slots {
            *inv.inventory_menu.slot_mut(i).unwrap() = ItemStack::new(kind, n);
        }
        inv
    }

    fn click(inv: &mut Inventory, op: impl Into<ClickOperation>) {
        inv.simulate_click(&op.into(), &PlayerAbilities::default());
    }

    #[test]
    fn shift_click_merges_then_fills_an_empty_slot() {
        let mut inv = player(&[(9, ItemKind::Stone, 32), (36, ItemKind::Stone, 40)]);
        click(&mut inv, QuickMoveClick::Left { slot: 9 });
        let m = &inv.inventory_menu;
        assert_eq!(m.slot(36), Some(&ItemStack::new(ItemKind::Stone, 64)));
        assert_eq!(m.slot(37), Some(&ItemStack::new(ItemKind::Stone, 8)));
        assert_eq!(m.slot(9), Some(&ItemStack::Empty));
    }

    #[test]
    fn shift_click_armour_goes_to_its_slot() {
        let mut inv = player(&[(20, ItemKind::IronHelmet, 1)]);
        click(&mut inv, QuickMoveClick::Left { slot: 20 });
        assert_eq!(inv.inventory_menu.slot(5), Some(&ItemStack::new(ItemKind::IronHelmet, 1)));
        assert_eq!(inv.inventory_menu.slot(20), Some(&ItemStack::Empty));
    }

    #[test]
    fn swap_button_is_a_hotbar_slot() {
        let mut inv = player(&[(9, ItemKind::Dirt, 5), (38, ItemKind::Stone, 7)]);
        click(&mut inv, azalea_inventory::operations::SwapClick { source_slot: 9, target_slot: 2 });
        let m = &inv.inventory_menu;
        assert_eq!(m.slot(9), Some(&ItemStack::new(ItemKind::Stone, 7)));
        assert_eq!(m.slot(38), Some(&ItemStack::new(ItemKind::Dirt, 5)));
        assert_eq!(m.slot(2), Some(&ItemStack::Empty));
    }

    #[test]
    fn swap_button_40_is_the_offhand() {
        let mut inv = player(&[(36, ItemKind::Shield, 1)]);
        click(&mut inv, azalea_inventory::operations::SwapClick { source_slot: 36, target_slot: 40 });
        assert_eq!(inv.inventory_menu.slot(45), Some(&ItemStack::new(ItemKind::Shield, 1)));
        assert_eq!(inv.inventory_menu.slot(36), Some(&ItemStack::Empty));
    }

    #[test]
    fn pickup_all_stops_at_a_full_stack() {
        let mut inv = player(&[(9, ItemKind::Stone, 10), (10, ItemKind::Stone, 10)]);
        inv.carried = ItemStack::new(ItemKind::Stone, 60);
        click(&mut inv, PickupAllClick { slot: 11, reversed: false });
        assert_eq!(inv.carried, ItemStack::new(ItemKind::Stone, 64));
        assert_eq!(inv.inventory_menu.slot(9), Some(&ItemStack::new(ItemKind::Stone, 6)));
        assert_eq!(inv.inventory_menu.slot(10), Some(&ItemStack::new(ItemKind::Stone, 10)));
    }

    #[test]
    fn right_click_takes_half_and_leaves_the_rest() {
        let mut inv = player(&[(9, ItemKind::Cobblestone, 63)]);
        click(&mut inv, PickupClick::Right { slot: Some(9) });
        assert_eq!(inv.carried, ItemStack::new(ItemKind::Cobblestone, 32));
        assert_eq!(inv.inventory_menu.slot(9), Some(&ItemStack::new(ItemKind::Cobblestone, 31)));
    }

    #[test]
    fn throwing_the_last_item_empties_the_slot() {
        let mut inv = player(&[(9, ItemKind::Stone, 1)]);
        click(&mut inv, ThrowClick::Single { slot: 9 });
        assert_eq!(inv.inventory_menu.slot(9), Some(&ItemStack::Empty));
    }

    #[test]
    fn nothing_goes_into_the_crafting_result() {
        let mut inv = player(&[]);
        inv.carried = ItemStack::new(ItemKind::Stone, 3);
        click(&mut inv, PickupClick::Left { slot: Some(0) });
        assert_eq!(inv.carried, ItemStack::new(ItemKind::Stone, 3));
        assert_eq!(inv.inventory_menu.slot(0), Some(&ItemStack::Empty));
    }
}
