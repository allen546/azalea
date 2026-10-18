use azalea_chat::{
    FormattedText,
    style::{Style, TextColor},
    text_component::TextComponent,
};
use azalea_core::{
    attribute_modifier_operation::AttributeModifierOperation,
    checksum::get_checksum,
    position::{BlockPos, GlobalPos},
};
use azalea_inventory::{
    ItemStack,
    components::{
        AdventureModePredicate, AttributeModifier, AttributeModifierDisplay, AttributeModifiers,
        AttributeModifiersEntry, BlockPredicate, CanPlaceOn, ChargedProjectiles, CustomData,
        CustomName, EquipmentSlotGroup, Glider, JukeboxPlayable, LodestoneTracker, Lore, MapColor,
        PotDecorations, Rarity,
    },
};
use azalea_registry::builtin::{Attribute, BlockKind, ItemKind};
use simdnbt::owned::{BaseNbt, Nbt, NbtCompound, NbtList, NbtTag};

#[test]
fn test_custom_name_checksum() {
    let c = CustomName {
        name: FormattedText::from("meow"),
    };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 2222287064);
}
#[test]
fn test_custom_name_checksum_2() {
    let c = CustomName {
        name: TextComponent::new("meow")
            .with_style(
                Style::new()
                    .color(Some(TextColor::parse("red").unwrap()))
                    .underlined(true),
            )
            .into(),
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 187682122);
}
#[test]
fn test_map_color_checksum() {
    let c = MapColor { color: 1 };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 1565579036);
}
#[test]
fn test_lore_checksum() {
    let c = Lore {
        lines: vec!["first".into(), "second".into()],
    };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 1545409323);
}
#[test]
fn test_rarity_checksum() {
    let c = Rarity::Rare;
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 2874400570);
}
// #[test]
// fn test_enchantments_checksum() {
//     let mut registry_holder = RegistryHolder::default();
//     registry_holder.append(
//         "enchantment".into(),
//         vec![
//             ("sharpness".into(), Some(NbtCompound::default())),
//             ("knockback".into(), Some(NbtCompound::default())),
//         ],
//     );
//     println!("registry holder: {registry_holder:?}");
//     let c = Enchantments {
//         levels: HashMap::from_iter([(Enchantment::new_raw(0), 5),
// (Enchantment::new_raw(1), 1)]),     };
//     assert_eq!(get_checksum(&c, &registry_holder).unwrap().0, 3717391112);
// }
#[test]
fn test_can_place_on_checksum() {
    let c = CanPlaceOn {
        predicate: AdventureModePredicate {
            predicates: vec![BlockPredicate {
                blocks: Some(vec![BlockKind::GrassBlock].into()),
                properties: None,
                nbt: None,
                components: Default::default(),
            }],
        },
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 227436005);
}

#[test]
fn test_custom_data_nbt() {
    let c = CustomData {
        nbt: Nbt::Some(BaseNbt::new(
            "",
            NbtCompound::from_values(vec![
                ("meow".into(), "mrrp".into()),
                (
                    "nya".into(),
                    NbtList::Compound(vec![
                        NbtTag::Int(1).into(),
                        NbtTag::Int(2).into(),
                        NbtCompound::new(),
                        NbtCompound::from_values(vec![("data".into(), NbtTag::Byte(1))]),
                    ])
                    .into(),
                ),
            ]),
        )),
    };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 1035780974);
}
#[test]
fn test_attribute_modifiers_checksum() {
    let c = AttributeModifiers {
        modifiers: vec![AttributeModifiersEntry {
            kind: Attribute::Scale,
            modifier: AttributeModifier {
                id: "example:grow".into(),
                amount: 4.0,
                operation: AttributeModifierOperation::AddMultipliedBase,
            },
            slot: EquipmentSlotGroup::Hand,
            display: AttributeModifierDisplay::Default,
        }],
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 2501379836);
}

#[test]
fn test_firework_explosion_checksum() {
    let c = AttributeModifiers {
        modifiers: vec![AttributeModifiersEntry {
            kind: Attribute::Scale,
            modifier: AttributeModifier {
                id: "example:grow".into(),
                amount: 4.0,
                operation: AttributeModifierOperation::AddMultipliedBase,
            },
            slot: EquipmentSlotGroup::Hand,
            display: AttributeModifierDisplay::Default,
        }],
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 2501379836);
}

#[test]
fn test_charged_projectile_checksum() {
    let c = ChargedProjectiles {
        items: vec![ItemStack::from(ItemKind::MusicDiscCat)],
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 3435761017);
}

#[test]
fn test_charged_projectile_with_components_checksum() {
    let c = ChargedProjectiles {
        items: vec![
            ItemStack::from(ItemKind::MusicDiscCat)
                .with_component::<JukeboxPlayable>(None)
                .with_component(ChargedProjectiles {
                    items: vec![ItemStack::from(ItemKind::MusicDiscCat)],
                }),
        ],
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 170375255);
}

#[test]
fn test_lodestone_tracker_checksum() {
    let c = LodestoneTracker {
        target: Some(GlobalPos {
            dimension: "meow".into(),
            pos: BlockPos::new(1, 2, 3),
        }),
        tracked: true,
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 4138292505);
}

#[test]
fn test_pot_decorations_checksum() {
    let c = PotDecorations {
        items: vec![
            ItemKind::Stick,
            ItemKind::Brick,
            ItemKind::Brick,
            ItemKind::Brick,
        ],
    };

    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 1951715383);
}

#[test]
fn test_glider_checksum() {
    let c = Glider;
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 3312760008);
}

// Tag keys hash as "#namespace:path" (vanilla `TagKey#hashedCodec`, `HolderSetCodec`). Values from
// vanilla 1.21.11's own HashOps with registry access, as the client's `decoratedHashOpsGenerator`.
fn named<D: azalea_registry::Registry>(key: &str) -> azalea_registry::HolderSet<D, azalea_registry::identifier::Identifier> {
    azalea_registry::HolderSet::Named { key: key.into(), contents: vec![] }
}

#[test]
fn test_damage_resistant_tag_checksum() {
    let c = azalea_inventory::components::DamageResistant { types: "minecraft:is_fire".into() };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 1644159004);
}

#[test]
fn test_provides_banner_patterns_tag_checksum() {
    let c = azalea_inventory::components::ProvidesBannerPatterns { key: "minecraft:pattern_item/flower".into() };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 269714998);
}

#[test]
fn test_blocks_attacks_bypassed_by_checksum() {
    use azalea_inventory::components::{BlocksAttacks, ItemDamageFunction};
    use azalea_registry::{Holder, builtin::SoundEvent};
    let c = BlocksAttacks {
        block_delay_seconds: 1.0,
        item_damage: ItemDamageFunction { threshold: 3.0, base: 1.0, factor: 1.0 },
        bypassed_by: Some("minecraft:bypasses_shield".into()),
        block_sound: Some(Holder::Reference(SoundEvent::ItemShieldBlock)),
        disabled_sound: Some(Holder::Reference(SoundEvent::ItemShieldBreak)),
        ..BlocksAttacks::new()
    };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 3979443873);
}

#[test]
fn test_blocks_attacks_reduction_tag_checksum() {
    use azalea_inventory::components::{BlocksAttacks, DamageReduction};
    let c = BlocksAttacks {
        block_delay_seconds: 0.25,
        damage_reductions: vec![DamageReduction {
            horizontal_blocking_angle: 90.0,
            kind: Some(named("minecraft:is_fire")),
            base: 0.0,
            factor: 1.0,
        }],
        ..BlocksAttacks::new()
    };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 1858893056);
}

#[test]
fn test_tool_rule_tag_checksum() {
    use azalea_inventory::components::{Tool, ToolRule};
    let c = Tool {
        rules: vec![ToolRule { blocks: named("minecraft:mineable/pickaxe"), speed: Some(6.0), correct_for_drops: Some(true) }],
        default_mining_speed: 1.0,
        damage_per_block: 1,
        can_destroy_blocks_in_creative: true,
    };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 2849864817);
}

#[test]
fn test_repairable_tag_checksum() {
    let c = azalea_inventory::components::Repairable { items: named("minecraft:wooden_tool_materials") };
    assert_eq!(get_checksum(&c, &Default::default()).unwrap().0, 3701220917);
}
