//! Slot rules of container menus (vanilla `ShulkerBoxSlot#mayPlace`).

use azalea_inventory::{ItemStackData, Menu};
use azalea_registry::builtin::{ItemKind, MenuKind};

#[test]
fn a_shulker_box_takes_everything_but_shulker_boxes() {
    let menu = Menu::from_kind(MenuKind::ShulkerBox);
    for shulker in [ItemKind::ShulkerBox, ItemKind::RedShulkerBox] {
        let item = ItemStackData::from(shulker);
        assert!(!menu.may_place(0, &item), "{shulker:?} fits in slot 0");
        assert!(!menu.may_place(26, &item));
        // the player's own slots below the container take them
        assert!(menu.may_place(27, &item));
    }
    assert!(menu.may_place(0, &ItemStackData::from(ItemKind::Stone)));
    assert!(menu.may_place(0, &ItemStackData::from(ItemKind::Bundle)));
}

#[test]
fn a_chest_takes_shulker_boxes() {
    let menu = Menu::from_kind(MenuKind::Generic9x3);
    assert!(menu.may_place(0, &ItemStackData::from(ItemKind::ShulkerBox)));
}
