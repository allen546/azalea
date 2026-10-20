//! Blocks the bot's box touches that the fork does not model (patch 30).

use azalea_block::BlockState;
use azalea_core::{aabb::Aabb, position::BlockPos};
use azalea_entity::unmodelled::{
    UnmodelledInput, honey_touches_side, is_unmodelled_block,
};
use azalea_registry::builtin::BlockKind;
use azalea_world::World;

/// How far past the box a block still counts as touched. A bot that stands on a slime block or
/// a bed has its feet exactly on the top face, and one that walks along a honey block has its
/// side exactly on the wall; both are contacts.
const TOUCH_MARGIN: f64 = 1e-3;

/// Every block whose cell the box touches (within [`TOUCH_MARGIN`]) that changes motion in a way
/// the fork does not model: bubble column (`BubbleColumnBlock#entityInside`), slime block
/// (`SlimeBlock#stepOn`), bed (`BedBlock#updateEntityMovementAfterFallOn`), scaffolding
/// (`ScaffoldingBlock#getCollisionShape`), moving piston
/// (`PistonMovingBlockEntity#moveCollidedEntities`) and honey touched from the side
/// (`HoneyBlock#entityInside`). Unloaded chunks add nothing.
// Spec: specs/B1-movement.md §4.3
pub fn unmodelled_block_contacts(world: &World, bb: &Aabb) -> Vec<UnmodelledInput> {
    let probe = bb.inflate_all(TOUCH_MARGIN);
    let min = probe.min.to_block_pos_floor();
    let max = probe.max.to_block_pos_floor();
    let mut out = Vec::new();
    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                let pos = BlockPos::new(x, y, z);
                let Some(state) = world.chunks.get_block_state(pos) else {
                    continue;
                };
                let kind = BlockKind::from(BlockState::from(state));
                let touched = is_unmodelled_block(kind)
                    || (kind == BlockKind::HoneyBlock && honey_touches_side(bb, pos));
                if touched {
                    out.push(UnmodelledInput::Block(pos, kind));
                }
            }
        }
    }
    out
}
