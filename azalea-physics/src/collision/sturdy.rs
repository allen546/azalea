//! `BlockBehaviour.BlockStateBase#isFaceSturdy` with `SupportType.FULL` (Null fork, patch 61).

use azalea_block::BlockState;
use azalea_core::{aabb::Aabb, direction::Direction};
use azalea_registry::builtin::BlockKind;

use super::blocks::BlockWithShape;

/// Whether `face` of the block is sturdy (`isFaceSturdy` FULL: the block's support shape covers
/// the whole square of that face, flush with the cell edge). The support shape is the collision
/// shape except in the few blocks that override `getBlockSupportShape`: leaves (empty), mud and
/// soul sand (a full block). `None` for a block whose override depends on more than its state
/// (snow layers, fence gates, shulker boxes, chorus flowers, hanging signs, multiface blocks), so
/// the caller does not guess.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 61: sturdy face for the place verdict)
pub fn is_face_sturdy(state: BlockState, face: Direction) -> Option<bool> {
    match BlockKind::from(state) {
        BlockKind::Mud | BlockKind::SoulSand => return Some(true),
        k if k.to_str().ends_with("_leaves") => return Some(false),
        BlockKind::SnowBlock => {}
        BlockKind::Snow
        | BlockKind::ChorusFlower
        | BlockKind::ShulkerBox
        | BlockKind::GlowLichen
        | BlockKind::SculkVein
        | BlockKind::ResinClump => return None,
        k => {
            let name = k.to_str();
            if name.ends_with("_fence_gate")
                || name.ends_with("shulker_box")
                || name.ends_with("hanging_sign")
            {
                return None;
            }
        }
    }
    let shape = state.collision_shape();
    if shape.is_empty() {
        return Some(false);
    }
    let boxes: Vec<Aabb> = shape.to_aabbs();
    Some(face_covered(&boxes, face))
}

/// The boxes that touch the cell edge on `face` cover the unit square of that face.
fn face_covered(boxes: &[Aabb], face: Direction) -> bool {
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
            if !(0.0..=1.0).contains(&cx) || !(0.0..=1.0).contains(&cy) {
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

    #[test]
    fn overrides_of_the_support_shape_are_read() {
        assert_eq!(is_face_sturdy(st(BlockKind::OakLeaves), Direction::Up), Some(false));
        assert_eq!(is_face_sturdy(st(BlockKind::SoulSand), Direction::Up), Some(true));
        assert_eq!(is_face_sturdy(st(BlockKind::Snow), Direction::Up), None);
        assert_eq!(is_face_sturdy(st(BlockKind::Air), Direction::Up), Some(false));
    }
}
