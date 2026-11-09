//! The position-based shape offset of flowers, grass, bamboo, dripstone and the other blocks with
//! an offset type.
//!
//! The generated shape tables (`blocks.rs`) hold these shapes as they are at block position
//! (0, 0, 0), where the offset is at its minimum. Every use of a shape at a real position has to
//! move it by the difference between that position's offset and the (0, 0, 0) offset.

use std::borrow::Cow;

use azalea_block::BlockState;
use azalea_core::position::{BlockPos, Vec3};
use azalea_registry::builtin::BlockKind;

use super::{BlockWithShape, VoxelShape};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OffsetType {
    Xz,
    Xyz,
}

/// (offset type, max horizontal, max vertical) of every block kind with an offset type.
/// Vanilla: `BlockBehaviour.Properties#offsetType` with `BlockBehaviour#getMaxHorizontalOffset`
/// (0.25; dripstone 0.125) and `getMaxVerticalOffset` (0.2; small dripleaf 0.1). Vanilla holds
/// these as `f32`.
fn offset_params(kind: BlockKind) -> Option<(OffsetType, f32, f32)> {
    use BlockKind as K;
    Some(match kind {
        K::ShortGrass | K::Fern | K::ShortDryGrass | K::TallDryGrass => {
            (OffsetType::Xyz, 0.25, 0.2)
        }
        K::SmallDripleaf => (OffsetType::Xyz, 0.25, 0.1),
        K::PointedDripstone => (OffsetType::Xz, 0.125, 0.2),
        K::TallSeagrass
        | K::Sunflower
        | K::Lilac
        | K::RoseBush
        | K::Peony
        | K::TallGrass
        | K::LargeFern
        | K::PitcherPlant
        | K::WarpedRoots
        | K::NetherSprouts
        | K::CrimsonRoots
        | K::HangingRoots => (OffsetType::Xz, 0.25, 0.2),
        _ if shape_moves(kind) => (OffsetType::Xz, 0.25, 0.2),
        _ => return None,
    })
}

/// Whether vanilla's `getShape` adds the offset: flowers (`FlowerBlock`, `WitherRoseBlock`,
/// `EyeblossomBlock`), bamboo and its sapling, mangrove propagule, pointed dripstone. Grass,
/// ferns, roots, tall plants and small dripleaf have an offset type but a fixed shape (checked
/// against vanilla at ten positions, `tests/vanilla_block_offsets.rs`).
fn shape_moves(kind: BlockKind) -> bool {
    use BlockKind as K;
    matches!(
        kind,
        K::PointedDripstone
            | K::MangrovePropagule
            | K::Dandelion
            | K::Torchflower
            | K::Poppy
            | K::BlueOrchid
            | K::Allium
            | K::AzureBluet
            | K::RedTulip
            | K::OrangeTulip
            | K::WhiteTulip
            | K::PinkTulip
            | K::OxeyeDaisy
            | K::Cornflower
            | K::WitherRose
            | K::LilyOfTheValley
            | K::BambooSapling
            | K::Bamboo
            | K::OpenEyeblossom
            | K::ClosedEyeblossom
    )
}

/// Vanilla `Mth#getSeed(int, int, int)` (its first product wraps as a 32-bit int).
fn pos_seed(x: i32, y: i32, z: i32) -> i64 {
    let mut l = (x.wrapping_mul(3129871) as i64) ^ (z as i64).wrapping_mul(116129781) ^ (y as i64);
    l = l
        .wrapping_mul(l)
        .wrapping_mul(42317861)
        .wrapping_add(l.wrapping_mul(11));
    l >> 16
}

fn offset_of(kind: BlockKind, x: i32, z: i32) -> Option<Vec3> {
    let (ty, max_h, max_v) = offset_params(kind)?;
    let l = pos_seed(x, 0, z);
    let max_h = max_h as f64;
    let horizontal = |bits: i64| {
        ((((bits & 15) as f32 / 15.0f32) as f64 - 0.5) * 0.5).clamp(-max_h, max_h)
    };
    let dx = horizontal(l);
    let dz = horizontal(l >> 8);
    let dy = match ty {
        OffsetType::Xz => 0.0,
        OffsetType::Xyz => (((l >> 4) & 15) as f32 / 15.0f32) as f64 - 1.0,
    };
    let dy = if ty == OffsetType::Xyz { dy * max_v as f64 } else { dy };
    Some(Vec3::new(dx, dy, dz))
}

/// The offset vanilla's `BlockStateBase#getOffset` gives `state` at `pos`; zero for a block with
/// no offset type.
// Spec: specs/F5-version-layer.md §4.6 (shapes at a position, as a vanilla client decides them)
pub fn block_offset(state: BlockState, pos: BlockPos) -> Vec3 {
    offset_of(BlockKind::from(state), pos.x, pos.z).unwrap_or(Vec3::ZERO)
}

/// How far the generated table shape of `state` sits from its true place at `pos`.
fn shift_from_table(state: BlockState, pos: BlockPos) -> Option<Vec3> {
    let kind = BlockKind::from(state);
    if !shape_moves(kind) {
        return None;
    }
    let at = offset_of(kind, pos.x, pos.z)?;
    let baked = offset_of(kind, 0, 0)?;
    Some(Vec3::new(at.x - baked.x, at.y - baked.y, at.z - baked.z))
}

/// Block shapes at a position: the table shape moved by the block's offset
/// (`BlockBehaviour$BlockStateBase#getShape` / `#getCollisionShape` with a real position).
pub trait BlockShapeAt {
    /// Outline shape in block-local coordinates, offset included.
    fn outline_shape_at(&self, pos: BlockPos) -> Cow<'static, VoxelShape>;
    /// Collision shape in block-local coordinates, offset included.
    fn collision_shape_at(&self, pos: BlockPos) -> Cow<'static, VoxelShape>;
}

impl BlockShapeAt for BlockState {
    // Spec: specs/F5-version-layer.md §4.6
    fn outline_shape_at(&self, pos: BlockPos) -> Cow<'static, VoxelShape> {
        match shift_from_table(*self, pos) {
            Some(d) => Cow::Owned(self.outline_shape().move_relative(d)),
            None => Cow::Borrowed(self.outline_shape()),
        }
    }
    // Spec: specs/F5-version-layer.md §4.6
    fn collision_shape_at(&self, pos: BlockPos) -> Cow<'static, VoxelShape> {
        match shift_from_table(*self, pos) {
            Some(d) if !self.collision_shape().is_empty() => {
                Cow::Owned(self.collision_shape().move_relative(d))
            }
            _ => Cow::Borrowed(self.collision_shape()),
        }
    }
}
