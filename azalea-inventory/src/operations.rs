use std::ops::RangeInclusive;

use azalea_buf::AzBuf;
use azalea_registry::builtin::ItemKind;

use crate::{
    AnvilMenuLocation, BeaconMenuLocation, BlastFurnaceMenuLocation, BrewingStandMenuLocation,
    CartographyTableMenuLocation, Crafter3x3MenuLocation, CraftingMenuLocation,
    EnchantmentMenuLocation, FurnaceMenuLocation, Generic3x3MenuLocation, Generic9x1MenuLocation,
    Generic9x2MenuLocation, Generic9x3MenuLocation, Generic9x4MenuLocation, Generic9x5MenuLocation,
    Generic9x6MenuLocation, GrindstoneMenuLocation, HopperMenuLocation, ItemStack, ItemStackData,
    LecternMenuLocation, LoomMenuLocation, Menu, MenuLocation, MerchantMenuLocation,
    ShulkerBoxMenuLocation, SmithingMenuLocation, SmokerMenuLocation,
    StonecutterMenuLocation,
    components::{EquipmentSlot, Equippable},
    item::MaxStackSizeExt,
};

/// A type of click in a Minecraft inventory.
#[derive(Clone, Debug)]
pub enum ClickOperation {
    Pickup(PickupClick),
    QuickMove(QuickMoveClick),
    Swap(SwapClick),
    Clone(CloneClick),
    Throw(ThrowClick),
    QuickCraft(QuickCraftClick),
    PickupAll(PickupAllClick),
}

#[derive(Clone, Debug)]
pub enum PickupClick {
    /// Left mouse click.
    ///
    /// Note that in the protocol, None is represented as -999.
    Left { slot: Option<u16> },
    /// Right mouse click.
    ///
    /// Note that in the protocol, None is represented as -999.
    Right { slot: Option<u16> },
    /// Drop cursor stack.
    LeftOutside,
    /// Drop cursor single item.
    RightOutside,
}
impl From<PickupClick> for ClickOperation {
    fn from(click: PickupClick) -> Self {
        ClickOperation::Pickup(click)
    }
}

/// Shift click
#[derive(Clone, Debug)]
pub enum QuickMoveClick {
    /// Shift + left mouse click
    Left { slot: u16 },
    /// Shift + right mouse click (identical behavior)
    Right { slot: u16 },
}
impl From<QuickMoveClick> for ClickOperation {
    fn from(click: QuickMoveClick) -> Self {
        ClickOperation::QuickMove(click)
    }
}

/// Used when you press number keys or F in an inventory.
#[derive(Clone, Debug)]
pub struct SwapClick {
    pub source_slot: u16,
    /// 0-8 for hotbar slots, 40 for offhand, everything else is treated as a
    /// slot index.
    pub target_slot: u8,
}

impl From<SwapClick> for ClickOperation {
    fn from(click: SwapClick) -> Self {
        ClickOperation::Swap(click)
    }
}
/// Middle click, only defined for creative players in non-player
/// inventories.
#[derive(Clone, Debug)]
pub struct CloneClick {
    pub slot: u16,
}
impl From<CloneClick> for ClickOperation {
    fn from(click: CloneClick) -> Self {
        ClickOperation::Clone(click)
    }
}
#[derive(Clone, Debug)]
pub enum ThrowClick {
    /// Drop key (Q)
    Single { slot: u16 },
    /// Ctrl + drop key (Q)
    All { slot: u16 },
}
impl From<ThrowClick> for ClickOperation {
    fn from(click: ThrowClick) -> Self {
        ClickOperation::Throw(click)
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickCraftClick {
    pub kind: QuickCraftKind,
    pub status: QuickCraftStatus,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuickCraftKind {
    Left,
    Right,
    Middle,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuickCraftStatusKind {
    /// Starting drag
    Start,
    /// Add slot
    Add,
    /// Ending drag
    End,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuickCraftStatus {
    /// Starting drag
    Start,
    /// Add a slot.
    Add { slot: u16 },
    /// Ending drag
    End,
}
impl From<QuickCraftStatus> for QuickCraftStatusKind {
    fn from(status: QuickCraftStatus) -> Self {
        match status {
            QuickCraftStatus::Start => QuickCraftStatusKind::Start,
            QuickCraftStatus::Add { .. } => QuickCraftStatusKind::Add,
            QuickCraftStatus::End => QuickCraftStatusKind::End,
        }
    }
}

/// Double click.
#[derive(Clone, Debug)]
pub struct PickupAllClick {
    /// The slot that we're double clicking on.
    ///
    /// It should be empty or at least not pickup-able (since the carried item
    /// is used as the filter).
    pub slot: u16,
    /// Impossible in vanilla clients.
    pub reversed: bool,
}
impl From<PickupAllClick> for ClickOperation {
    fn from(click: PickupAllClick) -> Self {
        ClickOperation::PickupAll(click)
    }
}

impl ClickOperation {
    /// Return the slot number that this operation is acting on, if any.
    ///
    /// Note that in the protocol, "None" is represented as -999.
    pub fn slot_num(&self) -> Option<u16> {
        match self {
            ClickOperation::Pickup(pickup) => match pickup {
                PickupClick::Left { slot } => *slot,
                PickupClick::Right { slot } => *slot,
                PickupClick::LeftOutside => None,
                PickupClick::RightOutside => None,
            },
            ClickOperation::QuickMove(quick_move) => match quick_move {
                QuickMoveClick::Left { slot } => Some(*slot),
                QuickMoveClick::Right { slot } => Some(*slot),
            },
            ClickOperation::Swap(swap) => Some(swap.source_slot),
            ClickOperation::Clone(clone) => Some(clone.slot),
            ClickOperation::Throw(throw) => match throw {
                ThrowClick::Single { slot } => Some(*slot),
                ThrowClick::All { slot } => Some(*slot),
            },
            ClickOperation::QuickCraft(quick_craft) => match quick_craft.status {
                QuickCraftStatus::Start => None,
                QuickCraftStatus::Add { slot } => Some(slot),
                QuickCraftStatus::End => None,
            },
            ClickOperation::PickupAll(pickup_all) => Some(pickup_all.slot),
        }
    }

    pub fn button_num(&self) -> u8 {
        match self {
            ClickOperation::Pickup(pickup) => match pickup {
                PickupClick::Left { .. } => 0,
                PickupClick::Right { .. } => 1,
                PickupClick::LeftOutside => 0,
                PickupClick::RightOutside => 1,
            },
            ClickOperation::QuickMove(quick_move) => match quick_move {
                QuickMoveClick::Left { .. } => 0,
                QuickMoveClick::Right { .. } => 1,
            },
            ClickOperation::Swap(swap) => swap.target_slot,
            ClickOperation::Clone(_) => 2,
            ClickOperation::Throw(throw) => match throw {
                ThrowClick::Single { .. } => 0,
                ThrowClick::All { .. } => 1,
            },
            ClickOperation::QuickCraft(quick_craft) => match quick_craft {
                QuickCraftClick {
                    kind: QuickCraftKind::Left,
                    status: QuickCraftStatus::Start,
                } => 0,
                QuickCraftClick {
                    kind: QuickCraftKind::Right,
                    status: QuickCraftStatus::Start,
                } => 4,
                QuickCraftClick {
                    kind: QuickCraftKind::Middle,
                    status: QuickCraftStatus::Start,
                } => 8,
                QuickCraftClick {
                    kind: QuickCraftKind::Left,
                    status: QuickCraftStatus::Add { .. },
                } => 1,
                QuickCraftClick {
                    kind: QuickCraftKind::Right,
                    status: QuickCraftStatus::Add { .. },
                } => 5,
                QuickCraftClick {
                    kind: QuickCraftKind::Middle,
                    status: QuickCraftStatus::Add { .. },
                } => 9,
                QuickCraftClick {
                    kind: QuickCraftKind::Left,
                    status: QuickCraftStatus::End,
                } => 2,
                QuickCraftClick {
                    kind: QuickCraftKind::Right,
                    status: QuickCraftStatus::End,
                } => 6,
                QuickCraftClick {
                    kind: QuickCraftKind::Middle,
                    status: QuickCraftStatus::End,
                } => 10,
            },
            ClickOperation::PickupAll(_) => 0,
        }
    }

    pub fn click_type(&self) -> ClickType {
        match self {
            ClickOperation::Pickup(_) => ClickType::Pickup,
            ClickOperation::QuickMove(_) => ClickType::QuickMove,
            ClickOperation::Swap(_) => ClickType::Swap,
            ClickOperation::Clone(_) => ClickType::Clone,
            ClickOperation::Throw(_) => ClickType::Throw,
            ClickOperation::QuickCraft(_) => ClickType::QuickCraft,
            ClickOperation::PickupAll(_) => ClickType::PickupAll,
        }
    }
}

#[derive(AzBuf, Clone, Copy, Debug, PartialEq)]
pub enum ClickType {
    Pickup = 0,
    QuickMove = 1,
    Swap = 2,
    Clone = 3,
    Throw = 4,
    QuickCraft = 5,
    PickupAll = 6,
}

impl Menu {
    /// Shift-click a slot in this menu.
    ///
    /// Keep in mind that this doesn't send any packets to the server, it just
    /// mutates this specific `Menu`.
    pub fn quick_move_stack(&mut self, slot_index: usize) -> ItemStack {
        let slot = self.slot(slot_index);
        if slot.is_none() {
            return ItemStack::Empty;
        };

        let slot_location = self
            .location_for_slot(slot_index)
            .expect("we just checked to make sure the slot is Some above, so this shouldn't be able to error");
        match slot_location {
            // Vanilla: InventoryMenu#quickMoveStack (armour and offhand included).
            MenuLocation::Player(_) => {
                let equip = self
                    .slot(slot_index)
                    .and_then(|s| s.as_present())
                    .and_then(|s| s.get_component::<Equippable>().map(|e| e.slot));
                let armor_slot = match equip {
                    Some(EquipmentSlot::Head) => Some(5),
                    Some(EquipmentSlot::Chest) => Some(6),
                    Some(EquipmentSlot::Legs) => Some(7),
                    Some(EquipmentSlot::Feet) => Some(8),
                    _ => None,
                };
                let empty = |m: &Self, i: usize| m.slot(i).is_some_and(|s| s.is_empty());
                match slot_index {
                    0 => {
                        // Vanilla: InventoryMenu#quickMoveStack, result slot; a taken result uses up the grid
                        if self.move_item_stack_to(slot_index, 9..=44, true) {
                            self.craft_took_result();
                        }
                        true
                    }
                    1..=8 => self.move_item_stack_to(slot_index, 9..=44, false),
                    _ if armor_slot.is_some_and(|j| empty(self, j)) => {
                        let j = armor_slot.unwrap();
                        self.move_item_stack_to(slot_index, j..=j, false)
                    }
                    _ if equip == Some(EquipmentSlot::Offhand) && empty(self, 45) => {
                        self.move_item_stack_to(slot_index, 45..=45, false)
                    }
                    9..=35 => self.move_item_stack_to(slot_index, 36..=44, false),
                    36..=44 => self.move_item_stack_to(slot_index, 9..=35, false),
                    _ => self.move_item_stack_to(slot_index, 9..=44, false),
                };
            }
            MenuLocation::Generic9x1(l) => match l {
                Generic9x1MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC9X1_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Generic9x2(l) => match l {
                Generic9x2MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC9X2_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Generic9x3(l) => match l {
                Generic9x3MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC9X3_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Generic9x4(l) => match l {
                Generic9x4MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC9X4_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Generic9x5(l) => match l {
                Generic9x5MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC9X5_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Generic9x6(l) => match l {
                Generic9x6MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC9X6_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Generic3x3(l) => match l {
                Generic3x3MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC3X3_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Crafter3x3(l) => match l {
                Crafter3x3MenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::GENERIC3X3_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Anvil(l) => match l {
                AnvilMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::ANVIL_FIRST_SLOT..=Menu::ANVIL_SECOND_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Beacon(l) => match l {
                BeaconMenuLocation::Payment => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
                BeaconMenuLocation::Player => {
                    self.try_move_item_to_slots(
                        slot_index,
                        Menu::BEACON_PAYMENT_SLOT..=Menu::BEACON_PAYMENT_SLOT,
                    );
                }
            },
            MenuLocation::BlastFurnace(l) => match l {
                BlastFurnaceMenuLocation::Player => {
                    self.try_move_item_to_slots(
                        slot_index,
                        Menu::BLAST_FURNACE_INGREDIENT_SLOT..=Menu::BLAST_FURNACE_FUEL_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::BrewingStand(l) => match l {
                BrewingStandMenuLocation::Player => {
                    self.try_move_item_to_slots(
                        slot_index,
                        *Menu::BREWING_STAND_BOTTLES_SLOTS.start()
                            ..=Menu::BREWING_STAND_INGREDIENT_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            // Vanilla: CraftingMenu#quickMoveStack. The result goes to the inventory
            // (last slot first) and uses up the grid; inventory stacks go to the grid first, then
            // across hotbar and main; grid stacks go to the inventory.
            MenuLocation::Crafting(l) => match l {
                CraftingMenuLocation::Player => {
                    if !self.move_item_stack_to(slot_index, Menu::CRAFTING_GRID_SLOTS, false) {
                        let other = if self.is_hotbar_slot(slot_index) {
                            self.player_slots_without_hotbar_range()
                        } else {
                            self.hotbar_slots_range()
                        };
                        self.move_item_stack_to(slot_index, other, false);
                    }
                }
                _ if slot_index == 0 => {
                    let player = self.player_slots_range();
                    if self.move_item_stack_to(slot_index, player, true) {
                        self.craft_took_result();
                    }
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, false);
                }
            },
            MenuLocation::Enchantment(l) => match l {
                EnchantmentMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::ENCHANTMENT_ITEM_SLOT..=Menu::ENCHANTMENT_LAPIS_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Furnace(l) => match l {
                FurnaceMenuLocation::Player => {
                    self.try_move_item_to_slots(
                        slot_index,
                        Menu::FURNACE_INGREDIENT_SLOT..=Menu::FURNACE_FUEL_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Grindstone(l) => match l {
                GrindstoneMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::GRINDSTONE_INPUT_SLOT..=Menu::GRINDSTONE_ADDITIONAL_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Hopper(l) => match l {
                HopperMenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::HOPPER_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Lectern(l) => match l {
                LecternMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::LECTERN_BOOK_SLOT..=Menu::LECTERN_BOOK_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Loom(l) => match l {
                LoomMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::LOOM_BANNER_SLOT..=Menu::LOOM_PATTERN_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Merchant(l) => match l {
                MerchantMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::MERCHANT_PAYMENTS_SLOTS,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::ShulkerBox(l) => match l {
                ShulkerBoxMenuLocation::Player => {
                    self.move_item_stack_to(slot_index, Menu::SHULKER_BOX_CONTENTS_SLOTS, false);
                }
                _ => {
                    let player = self.player_slots_range();
                    self.move_item_stack_to(slot_index, player, true);
                }
            },
            MenuLocation::Smithing(l) => match l {
                SmithingMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::SMITHING_TEMPLATE_SLOT..=Menu::SMITHING_ADDITIONAL_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Smoker(l) => match l {
                SmokerMenuLocation::Player => {
                    self.try_move_item_to_slots(
                        slot_index,
                        Menu::SMOKER_INGREDIENT_SLOT..=Menu::SMOKER_FUEL_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::CartographyTable(l) => match l {
                CartographyTableMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::CARTOGRAPHY_TABLE_MAP_SLOT..=Menu::CARTOGRAPHY_TABLE_ADDITIONAL_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
            MenuLocation::Stonecutter(l) => match l {
                StonecutterMenuLocation::Player => {
                    self.try_move_item_to_slots_or_toggle_hotbar(
                        slot_index,
                        Menu::STONECUTTER_INPUT_SLOT..=Menu::STONECUTTER_INPUT_SLOT,
                    );
                }
                _ => {
                    self.try_move_item_to_slots(slot_index, self.player_slots_range());
                }
            },
        }

        ItemStack::Empty
    }


    /// Whether `slot` is this menu's crafting result (vanilla `ResultSlot`).
    pub fn is_crafting_result(&self, slot: usize) -> bool {
        slot == 0 && matches!(self, Menu::Player(_) | Menu::Crafting { .. })
    }

    /// First grid slot and side length of this menu's crafting grid.
    fn crafting_grid(&self) -> Option<(usize, usize)> {
        match self {
            Menu::Player(_) => Some((1, 2)),
            Menu::Crafting { .. } => Some((1, 3)),
            _ => None,
        }
    }

    // Spec: specs/B3-inventory-crafting.md §4.2 (crafting by clicks)
    /// The client's half of taking the crafting result, vanilla `ResultSlot#onTake`: one item of
    /// each used grid cell goes, and its remainder (an empty bucket) takes its place. The client
    /// has no recipes, so it uses the item defaults (`CraftingRecipe#defaultCraftingReminder`);
    /// it never computes the next result, the server sends that.
    pub fn craft_took_result(&mut self) {
        let Some((first, width)) = self.crafting_grid() else { return };
        let present = |m: &Self, r: usize, c: usize| m.slot(first + r * width + c).is_some_and(|s| s.is_present());
        let rows: Vec<usize> = (0..width).filter(|&r| (0..width).any(|c| present(self, r, c))).collect();
        let cols: Vec<usize> = (0..width).filter(|&c| (0..width).any(|r| present(self, r, c))).collect();
        let (Some(&top), Some(&bottom), Some(&left), Some(&right)) =
            (rows.first(), rows.last(), cols.first(), cols.last())
        else {
            return;
        };
        // vanilla reads the remainders from the whole box first, then edits cell by cell
        let mut remainders = Vec::new();
        for r in top..=bottom {
            for c in left..=right {
                remainders.push(crafting_remainder(self.slot(first + r * width + c).map_or(ItemKind::Air, |s| s.kind())));
            }
        }
        let mut k = 0;
        for r in top..=bottom {
            for c in left..=right {
                let i = first + r * width + c;
                let remainder = remainders[k].take();
                k += 1;
                if let Some(cell) = self.slot_mut(i) {
                    cell.split(1);
                }
                let Some(mut rem) = remainder else { continue };
                match self.slot(i).cloned() {
                    Some(ItemStack::Present(cell)) => {
                        if cell.is_same_item_and_components(&rem) {
                            rem.count += cell.count;
                            *self.slot_mut(i).unwrap() = ItemStack::Present(rem);
                        } else {
                            // vanilla `Inventory#add` (or a drop); approximated hotbar first
                            self.add_to_player_slots(rem);
                        }
                    }
                    _ => *self.slot_mut(i).unwrap() = ItemStack::Present(rem),
                }
            }
        }
    }

    /// Merge into the player's stacks, then the first empty slot (hotbar first, as `Inventory`
    /// orders them). A full inventory drops the item on a real client; nothing to simulate.
    fn add_to_player_slots(&mut self, mut item: ItemStackData) {
        let all = self.player_slots_range();
        let hot = self.hotbar_slots_range();
        let order: Vec<usize> = hot.clone().chain(all.filter(|i| !hot.contains(i))).collect();
        for &i in &order {
            if let Some(ItemStack::Present(t)) = self.slot(i) {
                let max = self.max_stack_size_for(i, t);
                if t.is_same_item_and_components(&item) && t.count < max {
                    let ItemStack::Present(t) = self.slot_mut(i).unwrap() else { unreachable!() };
                    let n = (max - t.count).min(item.count);
                    t.count += n;
                    item.count -= n;
                    if item.count == 0 {
                        return;
                    }
                }
            }
        }
        if let Some(&i) = order.iter().find(|&&i| self.slot(i).is_some_and(|s| s.is_empty())) {
            *self.slot_mut(i).unwrap() = ItemStack::Present(item);
        }
    }

    fn try_move_item_to_slots_or_toggle_hotbar(
        &mut self,
        slot_index: usize,
        target_slot_indexes: RangeInclusive<usize>,
    ) {
        if !self.try_move_item_to_slots(slot_index, target_slot_indexes) {
            self.try_move_item_to_slots(
                slot_index,
                if self.is_hotbar_slot(slot_index) {
                    self.player_slots_without_hotbar_range()
                } else {
                    self.hotbar_slots_range()
                },
            );
        }
    }

    /// Whether the given item could be placed in this slot.
    ///
    /// Null fork: result slots take nothing, armour slots only matching armour, and a shulker
    /// box's own slots no shulker box (vanilla `ResultSlot#mayPlace`, `ArmorSlot#mayPlace`,
    /// `ShulkerBoxSlot#mayPlace`); other slots still return true.
    pub fn may_place(&self, target_slot_index: usize, item: &ItemStackData) -> bool {
        match self {
            Menu::Player(_) => match target_slot_index {
                0 => false,
                5..=8 => {
                    let want = [EquipmentSlot::Head, EquipmentSlot::Chest, EquipmentSlot::Legs, EquipmentSlot::Feet]
                        [target_slot_index - 5];
                    item.get_component::<Equippable>().is_some_and(|e| e.slot == want)
                }
                _ => true,
            },
            Menu::Crafting { .. } => target_slot_index != 0,
            // vanilla `ShulkerBoxSlot#mayPlace`: `Item#canFitInsideContainerItems` is false for
            // shulker boxes only
            Menu::ShulkerBox { .. } if target_slot_index < 27 => {
                !item.kind.to_str().ends_with("shulker_box")
            }
            _ => true,
        }
    }

    /// Whether the item in the given slot could be clicked and picked up.
    ///
    /// TODO: right now this always returns true
    pub fn may_pickup(&self, _source_slot_index: usize) -> bool {
        true
    }

    /// Whether the item in the slot can be picked up and placed.
    pub fn allow_modification(&self, target_slot_index: usize) -> bool {
        if !self.may_pickup(target_slot_index) {
            return false;
        }
        let item = self.slot(target_slot_index).unwrap();
        // the default here probably doesn't matter since we should only be calling this
        // if we already checked that the slot isn't empty
        item.as_present()
            .is_some_and(|item| self.may_place(target_slot_index, item))
    }

    /// Get the maximum number of items that can be placed in this slot.
    ///
    /// Null fork: vanilla containers allow 99 (`Container#getMaxStackSize`) and armour slots 1;
    /// the item's own limit applies on top (see [`Self::max_stack_size_for`]).
    pub fn max_stack_size(&self, target_slot_index: usize) -> i32 {
        match self {
            Menu::Player(_) if (5..=8).contains(&target_slot_index) => 1,
            _ => 99,
        }
    }

    /// Vanilla `Slot#getMaxStackSize(ItemStack)`: the slot's limit and the item's.
    pub fn max_stack_size_for(&self, target_slot_index: usize, item: &ItemStackData) -> i32 {
        self.max_stack_size(target_slot_index).min(item.max_stack_size())
    }

    // Null fork: vanilla `AbstractContainerMenu#moveItemStackTo`. Merge into matching stacks,
    // then put the rest in the first empty slot that takes it; `reverse` walks the range backwards.
    fn move_item_stack_to(&mut self, from: usize, range: RangeInclusive<usize>, reverse: bool) -> bool {
        let Some(ItemStack::Present(mut item)) = self.slot(from).cloned() else {
            return false;
        };
        let order: Vec<usize> = if reverse { range.rev().collect() } else { range.collect() };
        let mut moved = false;
        if item.max_stack_size() > 1 {
            for &i in &order {
                if item.count <= 0 {
                    break;
                }
                let max = match self.slot(i) {
                    Some(ItemStack::Present(t)) if i != from && t.is_same_item_and_components(&item) => {
                        self.max_stack_size_for(i, t)
                    }
                    _ => continue,
                };
                let ItemStack::Present(target) = self.slot_mut(i).unwrap() else { unreachable!() };
                if target.count + item.count <= max {
                    target.count += item.count;
                    item.count = 0;
                    moved = true;
                } else if target.count < max {
                    item.count -= max - target.count;
                    target.count = max;
                    moved = true;
                }
            }
        }
        if item.count > 0 {
            for &i in &order {
                if self.slot(i).is_some_and(|s| s.is_empty()) && self.may_place(i, &item) {
                    let n = item.count.min(self.max_stack_size_for(i, &item));
                    let mut part = item.clone();
                    part.count = n;
                    *self.slot_mut(i).unwrap() = ItemStack::Present(part);
                    item.count -= n;
                    moved = true;
                    break;
                }
            }
        }
        *self.slot_mut(from).unwrap() = if item.count > 0 { ItemStack::Present(item) } else { ItemStack::Empty };
        moved
    }

    /// Try moving an item to a set of slots in this menu.
    ///
    /// Returns the updated item slot.
    fn try_move_item_to_slots(
        &mut self,
        item_slot_index: usize,
        target_slot_indexes: RangeInclusive<usize>,
    ) -> bool {
        let mut item_slot = self.slot(item_slot_index).unwrap().clone();

        // first see if we can stack it with another item
        if item_slot.kind().stackable() {
            for target_slot_index in target_slot_indexes.clone() {
                self.move_item_to_slot_if_stackable(&mut item_slot, target_slot_index);
                if item_slot.is_empty() {
                    break;
                }
            }
        }

        // and if not then just try putting it in an empty slot
        if item_slot.is_present() {
            for target_slot_index in target_slot_indexes {
                self.move_item_to_slot_if_empty(&mut item_slot, target_slot_index);
                if item_slot.is_empty() {
                    break;
                }
            }
        }

        let is_source_slot_now_empty = item_slot.is_empty();

        *self.slot_mut(item_slot_index).unwrap() = item_slot;
        is_source_slot_now_empty
    }

    /// Merge this item slot into the target item slot, only if the target item
    /// slot is present and the same item.
    fn move_item_to_slot_if_stackable(
        &mut self,
        item_slot: &mut ItemStack,
        target_slot_index: usize,
    ) {
        let ItemStack::Present(item) = item_slot else {
            return;
        };
        let target_slot = self.slot(target_slot_index).unwrap();
        if let ItemStack::Present(target_item) = target_slot {
            // the target slot is empty, so we can just move the item there
            if self.may_place(target_slot_index, item)
                && target_item.is_same_item_and_components(item)
            {
                let slot_item_limit = self.max_stack_size(target_slot_index);
                let new_target_slot_data = item.split(i32::min(slot_item_limit, item.count) as u32);

                // get the target slot again but mut this time so we can update it
                let target_slot = self.slot_mut(target_slot_index).unwrap();
                *target_slot = ItemStack::from(new_target_slot_data);

                item_slot.update_empty();
            }
        }
    }

    fn move_item_to_slot_if_empty(
        &mut self,
        source_item: &mut ItemStack,
        target_slot_index: usize,
    ) {
        let ItemStack::Present(source_item_data) = source_item else {
            return;
        };
        let target_slot = self.slot(target_slot_index).unwrap();
        if target_slot.is_empty() && self.may_place(target_slot_index, source_item_data) {
            let slot_item_limit = self.max_stack_size(target_slot_index);
            let new_target_slot_data =
                source_item_data.split(i32::min(slot_item_limit, source_item_data.count) as u32);
            source_item.update_empty();

            let target_slot = self.slot_mut(target_slot_index).unwrap();
            *target_slot = new_target_slot_data.into();
        }
    }
}

/// Vanilla `Item#getCraftingRemainder`: the few items that leave something behind.
fn crafting_remainder(kind: ItemKind) -> Option<ItemStackData> {
    let left = match kind {
        ItemKind::WaterBucket | ItemKind::LavaBucket | ItemKind::MilkBucket => ItemKind::Bucket,
        ItemKind::DragonBreath | ItemKind::HoneyBottle => ItemKind::GlassBottle,
        _ => return None,
    };
    Some(ItemStackData::new(left, 1))
}
