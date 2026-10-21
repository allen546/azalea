#![doc = include_str!("../README.md")]

mod behavior;
mod click_use;
mod place_families;
pub mod block_state;
pub mod fluid_state;
mod generated;
mod range;

use core::fmt::Debug;
use std::{any::Any, collections::HashMap, str::FromStr};

use azalea_registry::builtin::BlockKind;
pub use behavior::BlockBehavior;
pub use click_use::block_uses_click;
pub use place_families::{
    PlaceFamily, indirect_shape_block, place_family, plain_replace_rule, shape_inert,
};
// re-exported for convenience
pub use block_state::BlockState;
pub use generated::{blocks, properties};
pub use range::BlockStates;

/// A trait that's implemented on block structs.
///
/// See the [azalea_block documentation](crate) for details.
pub trait BlockTrait: Debug + Any + Send + Sync {
    /// Convert the block struct to a boxed trait object.
    #[must_use]
    fn boxed(&self) -> Box<dyn BlockTrait>;

    /// Get the [`BlockBehavior`] of this block.
    #[must_use]
    fn behavior(&self) -> BlockBehavior;

    /// Get the Minecraft string ID for this block.
    ///
    /// For example, `stone` or `grass_block`.
    #[must_use]
    fn id(&self) -> &'static str;

    /// Convert the block struct to a [`BlockState`].
    ///
    /// This is a lossless conversion, as [`BlockState`] also contains state
    /// data.
    #[must_use]
    fn as_block_state(&self) -> BlockState;

    /// Convert the block struct to a [`BlockKind`].
    ///
    /// This is a lossy conversion, as [`BlockKind`] doesn't contain any state
    /// data.
    #[must_use]
    fn as_block_kind(&self) -> BlockKind;

    #[deprecated = "renamed to as_block_kind"]
    #[doc(hidden)]
    fn as_registry_block(&self) -> BlockKind {
        self.as_block_kind()
    }

    /// Returns a map of property names on this block to their values as
    /// strings.
    ///
    /// Consider using [`Self::get_property`] if you only need a single
    /// property.
    #[must_use]
    fn property_map(&self) -> HashMap<&'static str, &'static str>;

    /// Get a property's value as a string by its name, or `None` if the block
    /// has no property with that name.
    ///
    /// To get all properties, you may use [`Self::property_map`].
    ///
    /// To set a property, use [`Self::set_property`].
    #[must_use]
    fn get_property(&self, name: &str) -> Option<&'static str>;

    /// Update a property on this block, with the name and value being strings.
    ///
    /// Returns `Ok(())`, if the property name and value are valid, otherwise it
    /// returns `Err(InvalidPropertyError)`.
    ///
    /// To get a property, use [`Self::get_property`].
    fn set_property(&mut self, name: &str, new_value: &str) -> Result<(), InvalidPropertyError>;
}

#[derive(Debug)]
pub struct InvalidPropertyError;

impl dyn BlockTrait {
    pub fn downcast_ref<T: BlockTrait>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref::<T>()
    }
}

pub trait Property: FromStr {
    type Value;

    fn try_from_block_state(state: BlockState) -> Option<Self::Value>;

    /// Convert the value of the property to a string, like "x" or "true".
    fn to_static_str(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use crate::BlockTrait;

    #[test]
    pub fn roundtrip_block_state() {
        let block = crate::blocks::OakTrapdoor {
            facing: crate::properties::FacingCardinal::East,
            half: crate::properties::TopBottom::Bottom,
            open: true,
            powered: false,
            waterlogged: false,
        };
        let block_state = block.as_block_state();
        let block_from_state = *block_state
            .downcast_ref::<crate::blocks::OakTrapdoor>()
            .unwrap();
        assert_eq!(block, block_from_state);
    }

    #[test]
    pub fn test_property_map() {
        let block = crate::blocks::OakTrapdoor {
            facing: crate::properties::FacingCardinal::East,
            half: crate::properties::TopBottom::Bottom,
            open: true,
            powered: false,
            waterlogged: false,
        };

        let property_map = block.property_map();

        assert_eq!(property_map.len(), 5);
        assert_eq!(property_map.get("facing"), Some(&"east"));
        assert_eq!(property_map.get("half"), Some(&"bottom"));
        assert_eq!(property_map.get("open"), Some(&"true"));
        assert_eq!(property_map.get("powered"), Some(&"false"));
        assert_eq!(property_map.get("waterlogged"), Some(&"false"));
    }

    #[test]
    pub fn test_integer_properties() {
        // Test with oak sapling that has an integer-like stage property
        let sapling_stage_0 = crate::blocks::OakSapling {
            stage: crate::properties::Stage::_0,
        };

        let sapling_stage_1 = crate::blocks::OakSapling {
            stage: crate::properties::Stage::_1,
        };

        // Test stage 0
        let properties_0 = sapling_stage_0.property_map();
        assert_eq!(properties_0.len(), 1);
        assert_eq!(properties_0.get("stage"), Some(&"0"));
        assert_eq!(sapling_stage_0.get_property("stage"), Some("0"));

        // Test stage 1
        let properties_1 = sapling_stage_1.property_map();
        assert_eq!(properties_1.len(), 1);
        assert_eq!(properties_1.get("stage"), Some(&"1"));
        assert_eq!(sapling_stage_1.get_property("stage"), Some("1"));

        // Test non-existent property
        assert_eq!(sapling_stage_0.get_property("nonexistent"), None);
    }
}

/// Whether a right-click on this block state runs the block's own action, so a click that should
/// place an item needs a crouch (vanilla `MultiPlayerGameMode#performUseItemOn`). See
/// [`block_uses_click`].
// Spec: specs/B6-placement-breaking.md §4.2
#[must_use]
pub fn state_uses_click(state: BlockState) -> bool {
    block_uses_click(BlockKind::from(state))
}

#[cfg(test)]
mod click_use_tests {
    use super::*;

    #[test]
    fn known_blocks() {
        for k in [
            BlockKind::Chest,
            BlockKind::OakDoor,
            BlockKind::Cake,
            BlockKind::Cauldron,
            BlockKind::NoteBlock,
            BlockKind::Lever,
            BlockKind::CraftingTable,
        ] {
            assert!(block_uses_click(k), "{k:?}");
        }
        for k in [BlockKind::Stone, BlockKind::OakPlanks, BlockKind::Glass, BlockKind::Dirt] {
            assert!(!block_uses_click(k), "{k:?}");
        }
        assert!(state_uses_click(BlockState::from(BlockKind::Chest)));
        assert!(!state_uses_click(BlockState::from(BlockKind::Stone)));
    }
}

#[cfg(test)]
mod place_families_tests {
    use azalea_registry::Registry;

    use super::*;

    fn kinds() -> impl Iterator<Item = BlockKind> {
        (0..4000u32).filter_map(BlockKind::from_u32)
    }

    fn keys(kind: BlockKind) -> Vec<&'static str> {
        let b: Box<dyn BlockTrait> = kind.into();
        let mut k: Vec<_> = b.property_map().keys().copied().collect();
        k.sort_unstable();
        k
    }

    // Spec: specs/B6-placement-breaking.md §4.10 (place prediction: families)
    #[test]
    fn each_family_has_the_properties_its_state_rule_sets() {
        let mut seen = [0usize; 6];
        for kind in kinds() {
            let k = keys(kind);
            match place_family(kind) {
                PlaceFamily::Plain => seen[0] += 1,
                PlaceFamily::Axis => {
                    assert_eq!(k, ["axis"], "{kind:?}");
                    seen[1] += 1;
                }
                PlaceFamily::Waterloggable => {
                    assert!(k.contains(&"waterlogged"), "{kind:?}");
                    seen[2] += 1;
                }
                PlaceFamily::Slab => {
                    assert_eq!(k, ["type", "waterlogged"], "{kind:?}");
                    seen[3] += 1;
                }
                PlaceFamily::HorizontalFacing | PlaceFamily::HorizontalFacingSame => {
                    assert!(k.contains(&"facing"), "{kind:?}");
                    seen[4] += 1;
                }
                PlaceFamily::Unknown => seen[5] += 1,
            }
        }
        assert!(seen.iter().all(|&n| n > 0), "{seen:?}");
    }

    #[test]
    fn known_blocks_are_in_the_expected_family() {
        assert_eq!(place_family(BlockKind::Dirt), PlaceFamily::Plain);
        assert_eq!(place_family(BlockKind::Cobblestone), PlaceFamily::Plain);
        assert_eq!(place_family(BlockKind::OakLog), PlaceFamily::Axis);
        assert_eq!(place_family(BlockKind::OakSlab), PlaceFamily::Slab);
        assert_eq!(place_family(BlockKind::Barrier), PlaceFamily::Waterloggable);
        // placement rules beyond stage 1
        for k in [BlockKind::OakFence, BlockKind::OakStairs, BlockKind::Torch, BlockKind::Chest, BlockKind::GrassBlock, BlockKind::OakDoor, BlockKind::OakLeaves] {
            assert_eq!(place_family(k), PlaceFamily::Unknown, "{k:?}");
        }
        for k in [BlockKind::OakFence, BlockKind::OakStairs, BlockKind::RedstoneWire, BlockKind::OakLeaves, BlockKind::Vine, BlockKind::IronBars] {
            assert!(!shape_inert(k), "{k:?}");
        }
        for k in [BlockKind::Stone, BlockKind::Dirt, BlockKind::Air, BlockKind::OakSlab] {
            assert!(shape_inert(k), "{k:?}");
        }
        assert!(indirect_shape_block(BlockKind::RedstoneWire));
        assert!(!plain_replace_rule(BlockKind::Snow));
        assert!(!plain_replace_rule(BlockKind::OakSlab));
        assert!(plain_replace_rule(BlockKind::Stone));
    }
}
