//! `BlockBehaviour.BlockStateBase#isFaceSturdy` with `SupportType.FULL` (Null fork, patch 61).

use azalea_block::BlockState;
use azalea_core::{aabb::Aabb, direction::Direction, position::Vec3};
use azalea_registry::builtin::BlockKind;

use super::blocks::BlockWithShape;

/// The support shape as boxes (`BlockStateBase#getBlockSupportShape`): the collision shape except
/// in the few blocks that override it. Leaves have an empty one, mud and soul sand a full cube, a
/// closed fence gate a thin bar (`FenceGateBlock#getBlockSupportShape`, empty while open). `None`
/// for a block whose override depends on more than its state (snow layers, shulker boxes, chorus
/// flowers, hanging signs, multiface blocks), so the caller does not guess.
fn support_boxes(state: BlockState) -> Option<Vec<Aabb>> {
    let full = || vec![Aabb { min: Vec3::new(0.0, 0.0, 0.0), max: Vec3::new(1.0, 1.0, 1.0) }];
    match BlockKind::from(state) {
        BlockKind::Mud | BlockKind::SoulSand => return Some(full()),
        k if k.to_str().ends_with("_leaves") => return Some(Vec::new()),
        BlockKind::SnowBlock => {}
        BlockKind::Snow
        | BlockKind::ChorusFlower
        | BlockKind::ShulkerBox
        | BlockKind::GlowLichen
        | BlockKind::SculkVein
        | BlockKind::ResinClump => return None,
        k => {
            let name = k.to_str();
            if name.ends_with("shulker_box") || name.ends_with("hanging_sign") {
                return None;
            }
            if name.ends_with("_fence_gate") {
                if state.get_property("open") == Some("true") {
                    return Some(Vec::new());
                }
                // a 4 wide bar from 5/16 to 24/16 high along the panel (the panel of a gate that
                // faces north or south runs east to west)
                let along_x = matches!(state.get_property("facing"), Some("north" | "south"));
                let (lo, hi) = (6.0 / 16.0, 10.0 / 16.0);
                let (min, max) = if along_x {
                    (Vec3::new(0.0, 5.0 / 16.0, lo), Vec3::new(1.0, 1.5, hi))
                } else {
                    (Vec3::new(lo, 5.0 / 16.0, 0.0), Vec3::new(hi, 1.5, 1.0))
                };
                return Some(vec![Aabb { min, max }]);
            }
        }
    }
    Some(state.collision_shape().to_aabbs())
}

/// Whether `face` of the block is sturdy (`isFaceSturdy` FULL: the block's support shape covers
/// the whole square of that face, flush with the cell edge). `None` when the support shape is not
/// modelled.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 61: sturdy face for the place verdict)
pub fn is_face_sturdy(state: BlockState, face: Direction) -> Option<bool> {
    is_face_supported(state, face, SupportType::Full)
}

/// A rectangle in the plane of a face: (min, max) on the first and second plane axes (x then z
/// for up and down, x then y for north and south, z then y for west and east).
pub type FaceRegion = (f64, f64, f64, f64);

/// The whole face.
pub const UNIT: FaceRegion = (0.0, 1.0, 0.0, 1.0);

/// Which `SupportType` to test (`BlockBehaviour.BlockStateBase#isFaceSturdy` with a support type).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportType {
    Full,
    /// `SupportType.CENTER`: the face covers the 2 by 2 square in its middle.
    Center,
    /// `SupportType.RIGID`: the face covers all of it except the 12 by 12 middle (a 2 wide frame).
    Rigid,
}

/// `isFaceSturdy(.., face, supportType)` on the support shape; `None` where it is not modelled.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 71: support types for torches and rails)
pub fn is_face_supported(state: BlockState, face: Direction, support: SupportType) -> Option<bool> {
    let boxes = support_boxes(state)?;
    if boxes.is_empty() {
        return Some(false);
    }
    let regions: &[FaceRegion] = match support {
        SupportType::Full => &[UNIT],
        SupportType::Center => &[(7.0 / 16.0, 9.0 / 16.0, 7.0 / 16.0, 9.0 / 16.0)],
        SupportType::Rigid => &[
            (0.0, 1.0, 0.0, 2.0 / 16.0),
            (0.0, 1.0, 14.0 / 16.0, 1.0),
            (0.0, 2.0 / 16.0, 2.0 / 16.0, 14.0 / 16.0),
            (14.0 / 16.0, 1.0, 2.0 / 16.0, 14.0 / 16.0),
        ],
    };
    Some(face_covered(&boxes, face, regions))
}

/// Whether the face slice of the block's collision shape (`VoxelShape#getFaceShape`) covers the
/// region: used by walls (`WallBlock#updateShape` reads the face of the block above).
// Spec: specs/B6-placement-breaking.md §4.10 (patch 71: walls)
pub fn collision_face_covers(state: BlockState, face: Direction, region: FaceRegion) -> bool {
    let shape = state.collision_shape();
    if shape.is_empty() {
        return false;
    }
    face_covered(&shape.to_aabbs(), face, &[region])
}

/// The boxes that touch the cell edge on `face` cover every one of the regions of that face.
fn face_covered(boxes: &[Aabb], face: Direction, regions: &[FaceRegion]) -> bool {
    const EDGE: f64 = 1.0e-7;
    // (axis of the face, at the max side?, the two other axes)
    let on_face = |b: &Aabb| match face {
        Direction::Down => b.min.y <= EDGE,
        Direction::Up => b.max.y >= 1.0 - EDGE,
        Direction::North => b.min.z <= EDGE,
        Direction::South => b.max.z >= 1.0 - EDGE,
        Direction::West => b.min.x <= EDGE,
        Direction::East => b.max.x >= 1.0 - EDGE,
    };
    let rect = |b: &Aabb| match face {
        Direction::Down | Direction::Up => (b.min.x, b.max.x, b.min.z, b.max.z),
        Direction::North | Direction::South => (b.min.x, b.max.x, b.min.y, b.max.y),
        Direction::West | Direction::East => (b.min.z, b.max.z, b.min.y, b.max.y),
    };
    let rects: Vec<_> = boxes.iter().filter(|b| on_face(b)).map(rect).collect();
    if rects.is_empty() {
        return false;
    }
    // Compress the edges: every cell between two neighbouring edges must lie in some rectangle.
    let mut xs = vec![0.0, 1.0];
    let mut ys = vec![0.0, 1.0];
    for r in regions {
        xs.extend([r.0, r.1]);
        ys.extend([r.2, r.3]);
    }
    for r in &rects {
        xs.extend([r.0, r.1]);
        ys.extend([r.2, r.3]);
    }
    xs.sort_by(f64::total_cmp);
    ys.sort_by(f64::total_cmp);
    for xw in xs.windows(2) {
        for yw in ys.windows(2) {
            if xw[1] - xw[0] < EDGE || yw[1] - yw[0] < EDGE {
                continue;
            }
            let (cx, cy) = ((xw[0] + xw[1]) / 2.0, (yw[0] + yw[1]) / 2.0);
            if !regions.iter().any(|r| r.0 <= cx && cx <= r.1 && r.2 <= cy && cy <= r.3) {
                continue;
            }
            if !rects.iter().any(|r| r.0 <= cx && cx <= r.1 && r.2 <= cy && cy <= r.3) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(kind: BlockKind) -> BlockState {
        BlockState::from(kind)
    }

    #[test]
    fn a_full_cube_is_sturdy_on_every_face() {
        for f in [Direction::Up, Direction::Down, Direction::North, Direction::East] {
            assert_eq!(is_face_sturdy(st(BlockKind::Stone), f), Some(true));
        }
    }

    #[test]
    fn a_bottom_slab_is_sturdy_below_and_not_above() {
        let mut slab: Box<dyn azalea_block::BlockTrait> = BlockKind::StoneSlab.into();
        slab.set_property("type", "bottom").unwrap();
        let s = slab.as_block_state();
        assert_eq!(is_face_sturdy(s, Direction::Up), Some(false));
        assert_eq!(is_face_sturdy(s, Direction::Down), Some(true));
        assert_eq!(is_face_sturdy(s, Direction::North), Some(false), "half height side");
    }

    // FenceGateBlock#getBlockSupportShape: a thin bar when closed (centre support on top, never
    // full), nothing when open
    #[test]
    fn a_fence_gate_supports_the_centre_when_closed_only() {
        let gate = |open: &str| {
            let mut b: Box<dyn azalea_block::BlockTrait> = BlockKind::OakFenceGate.into();
            b.set_property("facing", "north").unwrap();
            b.set_property("open", open).unwrap();
            b.as_block_state()
        };
        assert_eq!(is_face_sturdy(gate("false"), Direction::Up), Some(false));
        assert_eq!(is_face_supported(gate("false"), Direction::Up, SupportType::Center), Some(true));
        assert_eq!(is_face_supported(gate("true"), Direction::Up, SupportType::Center), Some(false));
        assert_eq!(is_face_supported(gate("false"), Direction::Down, SupportType::Center), Some(false));
    }

    #[test]
    fn overrides_of_the_support_shape_are_read() {
        assert_eq!(is_face_sturdy(st(BlockKind::OakLeaves), Direction::Up), Some(false));
        assert_eq!(is_face_sturdy(st(BlockKind::SoulSand), Direction::Up), Some(true));
        assert_eq!(is_face_sturdy(st(BlockKind::Snow), Direction::Up), None);
        assert_eq!(is_face_sturdy(st(BlockKind::Air), Direction::Up), Some(false));
    }
}

#[cfg(test)]
mod support_tests {
    use super::*;

    fn with(kind: BlockKind, props: &[(&str, &str)]) -> BlockState {
        let mut b: Box<dyn azalea_block::BlockTrait> = kind.into();
        for (n, v) in props {
            b.set_property(n, v).unwrap();
        }
        b.as_block_state()
    }

    // SupportType.CENTER on a fence post: the collision column covers the middle square
    #[test]
    fn center_support_of_a_fence_top_and_a_bottom_slab_side() {
        assert_eq!(is_face_supported(BlockState::from(BlockKind::OakFence), Direction::Up, SupportType::Center), Some(true));
        assert_eq!(is_face_sturdy(BlockState::from(BlockKind::OakFence), Direction::Up), Some(false));
        // a bottom slab top is below the cell edge: nothing there
        let slab = with(BlockKind::StoneSlab, &[("type", "bottom")]);
        assert_eq!(is_face_supported(slab, Direction::Up, SupportType::Center), Some(false));
        assert_eq!(is_face_supported(BlockState::from(BlockKind::OakLeaves), Direction::Up, SupportType::Center), Some(false));
    }

    // SupportType.RIGID: a full cube yes, a fence post no (the frame is open)
    #[test]
    fn rigid_support_needs_the_frame() {
        assert_eq!(is_face_supported(BlockState::from(BlockKind::Stone), Direction::Up, SupportType::Rigid), Some(true));
        assert_eq!(is_face_supported(BlockState::from(BlockKind::OakFence), Direction::Up, SupportType::Rigid), Some(false));
        assert_eq!(is_face_supported(BlockState::from(BlockKind::Air), Direction::Up, SupportType::Rigid), Some(false));
    }

    // WallBlock#updateShape: the face of the block above covers the wall arm test shape
    #[test]
    fn collision_face_of_the_block_above() {
        let north_arm = (7.0 / 16.0, 9.0 / 16.0, 0.0, 9.0 / 16.0);
        assert!(collision_face_covers(BlockState::from(BlockKind::Stone), Direction::Down, north_arm));
        assert!(!collision_face_covers(BlockState::from(BlockKind::Air), Direction::Down, north_arm));
        let top_slab = with(BlockKind::StoneSlab, &[("type", "top")]);
        assert!(!collision_face_covers(top_slab, Direction::Down, north_arm), "the top slab does not reach the cell's bottom edge");
        let bottom_slab = with(BlockKind::StoneSlab, &[("type", "bottom")]);
        assert!(collision_face_covers(bottom_slab, Direction::Down, north_arm));
    }
}
