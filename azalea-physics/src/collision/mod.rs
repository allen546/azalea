mod blocks;
mod discrete_voxel_shape;
pub mod entity_collisions;
mod mergers;
pub mod offset;
mod shape;
mod sturdy;
pub mod world_collisions;

use std::sync::LazyLock;

use azalea_block::{BlockState, fluid_state::FluidState};
use azalea_core::{
    aabb::Aabb,
    direction::Axis,
    math::{self, EPSILON},
    position::{BlockPos, Vec3},
};
use azalea_entity::{
    Attributes, Jumping, LookDirection, OnClimbable, Physics, PlayerAbilities, Pose, Position,
    metadata::Sprinting,
};
use azalea_registry::builtin::BlockKind;
use azalea_world::{BlockView, BotWorld};
use bevy_ecs::{entity::Entity, world::Mut};
pub use blocks::BlockWithShape;
pub use offset::{BlockShapeAt, block_offset};
pub use discrete_voxel_shape::*;
use entity_collisions::{CollidableEntityQuery, get_entity_collisions};
pub use shape::*;
pub use sturdy::{FaceRegion, SupportType, collision_face_covers, is_face_sturdy, is_face_supported};
use tracing::warn;

use self::world_collisions::get_block_collisions;
use crate::{
    client_movement::ClientMovementState, collision::entity_collisions::AabbQuery,
    travel::no_collision,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoverType {
    Own,
    Player,
    Piston,
    ShulkerBox,
    Shulker,
}

// Entity.collide
// Spec: specs/B1-movement.md §4.2
fn collide(ctx: &MoveCtx, movement: Vec3) -> Vec3 {
    let entity_bounding_box = ctx.physics.bounding_box;
    let entity_collisions = get_entity_collisions(
        ctx.world.world,
        &entity_bounding_box.expand_towards(movement),
        Some(ctx.source_entity),
        ctx.aabb_query,
        ctx.collidable_entity_query,
    );
    collide_with(
        &entity_bounding_box,
        ctx.physics.on_ground(),
        get_max_up_step(ctx.attributes),
        movement,
        &entity_collisions,
        &|aabb| get_block_collisions(ctx.world, aabb),
    )
}

/// The body of `Entity#collide`, with the block colliders of a box passed in so a test can
/// supply its own.
// Spec: specs/B1-movement.md §4.2
fn collide_with(
    entity_bounding_box: &Aabb,
    on_ground: bool,
    max_up_step: f32,
    movement: Vec3,
    entity_collisions: &[VoxelShape],
    block_collisions: &dyn Fn(&Aabb) -> Vec<VoxelShape>,
) -> Vec3 {
    let entity_bounding_box = *entity_bounding_box;
    let collided_delta = if movement.length_squared() == 0.0 {
        movement
    } else {
        let mut shapes = entity_collisions.to_vec();
        shapes.extend(block_collisions(&entity_bounding_box.expand_towards(movement)));
        collide_with_shapes(movement, entity_bounding_box, &shapes)
    };

    let x_collision = movement.x != collided_delta.x;
    let y_collision = movement.y != collided_delta.y;
    let z_collision = movement.z != collided_delta.z;
    let pushed_down_onto_ground = y_collision && movement.y < 0.;

    if max_up_step > 0.
        && (pushed_down_onto_ground || on_ground)
        && (x_collision || z_collision)
    {
        // Vanilla steps from the box after the downward part of the move when the entity
        // was pushed down onto ground, and from the start box otherwise. The step is capped
        // at `max_up_step` above that box.
        let step_box = if pushed_down_onto_ground {
            entity_bounding_box.move_relative(Vec3::new(0., collided_delta.y, 0.))
        } else {
            entity_bounding_box
        };
        let mut search_box =
            step_box.expand_towards(Vec3::new(movement.x, max_up_step as f64, movement.z));
        if !pushed_down_onto_ground {
            // vanilla's -1.0E-5F, a float widened to double
            search_box = search_box.expand_towards(Vec3::new(0., -(1.0E-5f32 as f64), 0.));
        }
        let mut step_colliders = entity_collisions.to_vec();
        step_colliders.extend(block_collisions(&search_box));

        let heights = candidate_step_up_heights(
            &step_box,
            &step_colliders,
            max_up_step,
            collided_delta.y as f32,
        );
        for height in heights {
            let stepped = collide_with_shapes(
                Vec3::new(movement.x, height as f64, movement.z),
                step_box,
                &step_colliders,
            );
            if stepped.horizontal_distance_squared() > collided_delta.horizontal_distance_squared()
            {
                let dy = entity_bounding_box.min.y - step_box.min.y;
                return Vec3::new(stepped.x, stepped.y - dy, stepped.z);
            }
        }
    }

    collided_delta
}

/// The heights a step can end at, ascending: each collider's Y coordinate above the box's
/// bottom, not below it, not equal to the move's own vertical result, and at most the step
/// height.
///
/// In Mojmap, this is `Entity#collectCandidateStepUpHeights`.
// Spec: specs/B1-movement.md §4.2
fn candidate_step_up_heights(
    step_box: &Aabb,
    colliders: &[VoxelShape],
    max_up_step: f32,
    collided_y: f32,
) -> Vec<f32> {
    let mut heights: Vec<f32> = Vec::new();
    for shape in colliders {
        for &y in shape.get_coords(Axis::Y) {
            let h = (y - step_box.min.y) as f32;
            if h < 0. || h == collided_y {
                continue;
            }
            if h > max_up_step {
                break;
            }
            if !heights.contains(&h) {
                heights.push(h);
            }
        }
    }
    heights.sort_by(|a, b| a.total_cmp(b));
    heights
}

/// The velocity factor of the block the entity is in or standing on.
///
/// In Mojmap, this is `Entity#getBlockSpeedFactor` (with the `Player`
/// override: no factor while flying).
// Spec: specs/B1-movement.md §4.2
pub fn block_speed_factor(
    world: &BotWorld,
    position: Vec3,
    abilities: Option<&PlayerAbilities>,
) -> f32 {
    if abilities.is_some_and(|a| a.flying) {
        return 1.;
    }
    let block_at = world
        .get_block_state(BlockPos::from(position))
        .unwrap_or_default();
    let factor = block_at.behavior().speed_factor;
    let kind = BlockKind::from(block_at);
    if kind == BlockKind::Water || kind == BlockKind::BubbleColumn {
        return factor;
    }
    if factor != 1. {
        return factor;
    }
    let below = world
        .get_block_state(crate::get_block_pos_below_that_affects_movement(
            Position::new(position),
        ))
        .unwrap_or_default();
    below.behavior().speed_factor
}

pub struct MoveCtx<'world, 'state, 'a, 'b> {
    pub mover_type: MoverType,
    pub world: &'a BotWorld<'a>,
    pub position: Mut<'a, Position>,
    pub physics: &'a mut Physics,
    pub source_entity: Entity,
    pub aabb_query: &'a AabbQuery<'world, 'state, 'b>,
    pub collidable_entity_query: &'a CollidableEntityQuery<'world, 'state>,
    pub physics_state: Option<&'a ClientMovementState>,
    pub attributes: &'a Attributes,
    pub abilities: Option<&'a PlayerAbilities>,

    pub direction: LookDirection,
    pub sprinting: Sprinting,
    pub on_climbable: OnClimbable,
    pub pose: Option<Pose>,
    pub jumping: Jumping,
    /// Whether the feet slot holds leather boots, for
    /// `PowderSnowBlock#canEntityWalkOnPowderSnow`.
    pub wears_leather_boots: bool,
    /// Levitation's amplifier (0 is level I) while the effect is in force
    /// (`LivingEntity#travelInAir`).
    pub levitation: Option<i32>,
    /// Slow Falling is in force (`LivingEntity#getEffectiveGravity`).
    pub slow_falling: bool,
    /// The chunk the bot stands in is in its own level (`InLoadedChunk`, patch 43). Always true
    /// for a mover that is not a local bot.
    pub own_chunk_loaded: bool,
}

/// Move an entity by a given delta, checking for collisions.
///
/// In Mojmap, this is `Entity.move`.
pub fn move_colliding(ctx: &mut MoveCtx, mut movement: Vec3) {
    // TODO: do all these

    // if self.no_physics {
    //     return;
    // };

    // if (var1 == MoverType.PISTON) {
    //     var2 = this.limitPistonMovement(var2);
    //     if (var2.equals(Vec3.ZERO)) {
    //        return;
    //     }
    // }

    // Null fork: Entity#move scales the movement by the stuck multiplier a
    // block set last tick, clears it and zeroes the velocity (not for
    // pistons). Spec: specs/B1-movement.md §4.2
    if ctx.physics.stuck_speed_multiplier.length_squared() > 1.0E-7 {
        if ctx.mover_type != MoverType::Piston {
            let m = ctx.physics.stuck_speed_multiplier;
            movement = Vec3::new(movement.x * m.x, movement.y * m.y, movement.z * m.z);
        }
        ctx.physics.stuck_speed_multiplier = Vec3::ZERO;
        ctx.physics.velocity = Vec3::ZERO;
    }

    movement = maybe_back_off_from_edge(ctx, movement);
    let collide_result = collide(ctx, movement);

    let move_distance_sqr = collide_result.length_squared();

    let position = &mut ctx.position;
    let physics = &mut *ctx.physics;
    let world = ctx.world;

    if move_distance_sqr > EPSILON || movement.length_squared() - move_distance_sqr < EPSILON {
        // TODO: fall damage

        let new_pos = {
            Vec3 {
                x: position.x + collide_result.x,
                y: position.y + collide_result.y,
                z: position.z + collide_result.z,
            }
        };

        // Null fork: `Entity#addMovementThisTick`, walked at the end of the tick.
        physics.movement_this_tick.push((***position, new_pos, movement));

        if new_pos != ***position {
            ***position = new_pos;
        }
    }

    let x_collision = !math::equal(movement.x, collide_result.x);
    let z_collision = !math::equal(movement.z, collide_result.z);
    let horizontal_collision = x_collision || z_collision;
    physics.horizontal_collision = horizontal_collision;

    let vertical_collision = movement.y != collide_result.y;
    physics.vertical_collision = vertical_collision;
    let on_ground = vertical_collision && movement.y < 0.;
    physics.set_on_ground(on_ground);

    // TODO: minecraft checks for a "minor" horizontal collision here

    let block_pos_below = azalea_entity::on_pos_legacy(world, **position);
    let block_state_below = world.get_block_state(block_pos_below).unwrap_or_default();

    check_fall_damage(
        physics,
        collide_result.y,
        block_state_below,
        block_pos_below,
    );

    // if self.isRemoved() { return; }

    if horizontal_collision {
        let delta_movement = &physics.velocity;
        physics.velocity = Vec3 {
            x: if x_collision { 0. } else { delta_movement.x },
            y: delta_movement.y,
            z: if z_collision { 0. } else { delta_movement.z },
        }
    }

    if vertical_collision {
        // blockBelow.updateEntityAfterFallOn(this.level, this);
        // the default implementation of updateEntityAfterFallOn sets the y
        // movement to 0
        physics.velocity.y = 0.;
    }

    if on_ground {
        // blockBelow.stepOn(this.level, blockPosBelow, blockStateBelow,
        // this);
    }

    // sounds

    // Null fork: Entity#getBlockSpeedFactor scales the velocity after the move.
    // `LivingEntity#getBlockSpeedFactor` blends it toward 1 by movement efficiency.
    let factor = block_speed_factor(ctx.world, **ctx.position, ctx.abilities);
    let efficiency = ctx.attributes.movement_efficiency.calculate() as f32;
    let speed_factor = (factor + efficiency * (1. - factor)) as f64;
    ctx.physics.velocity.x *= speed_factor;
    ctx.physics.velocity.z *= speed_factor;

    // if (this.level.getBlockStatesIfLoaded(this.
    // getBoundingBox().deflate(1.0E-6D)).noneMatch((var0) -> {
    //    return var0.is(BlockTags.FIRE) || var0.is(Blocks.LAVA);
    // })) {
    //    if (this.remainingFireTicks <= 0) {
    //       this.setRemainingFireTicks(-this.getFireImmuneTicks());
    //    }

    //    if (this.wasOnFire && (this.isInPowderSnow ||
    // this.isInWaterRainOrBubble())) {       this.
    // playEntityOnFireExtinguishedSound();    }
    // }

    // if (this.isOnFire() && (this.isInPowderSnow ||
    // this.isInWaterRainOrBubble())) {    this.setRemainingFireTicks(-this.
    // getFireImmuneTicks()); }
}

fn check_fall_damage(
    physics: &mut Physics,
    delta_y: f64,
    _block_state_below: BlockState,
    _block_pos_below: BlockPos,
) {
    if !physics.is_in_water() && delta_y < 0. {
        physics.fall_distance -= delta_y as f32 as f64;
    }

    if physics.on_ground() {
        // vanilla calls block.fallOn here but it's not relevant for us

        physics.fall_distance = 0.;
    }
}

fn maybe_back_off_from_edge(move_ctx: &mut MoveCtx, mut movement: Vec3) -> Vec3 {
    let is_staying_on_ground_surface = move_ctx.physics_state.is_some_and(|s| s.trying_to_crouch);
    let max_up_step = get_max_up_step(move_ctx.attributes);

    let fall_ctx = CanFallAtLeastCtx {
        physics: move_ctx.physics,
        world: move_ctx.world,
        source_entity: move_ctx.source_entity,
        aabb_query: move_ctx.aabb_query,
        collidable_entity_query: move_ctx.collidable_entity_query,
    };

    let Some(abilities) = move_ctx.abilities else {
        return movement;
    };

    let is_backing_off = !abilities.flying
        && movement.y <= 0.
        && matches!(move_ctx.mover_type, MoverType::Own | MoverType::Player)
        && is_staying_on_ground_surface
        && is_above_ground(&fall_ctx, max_up_step);
    if !is_backing_off {
        return movement;
    }

    let min_movement = 0.05;
    let min_movement_x = movement.x.signum() * min_movement;
    let min_movement_z = movement.z.signum() * min_movement;

    while movement.x != 0. && can_fall_at_least(&fall_ctx, movement.x, 0., max_up_step as f64) {
        if movement.x.abs() <= min_movement {
            movement.x = 0.;
            break;
        }

        movement.x -= min_movement_x
    }
    while movement.z != 0. && can_fall_at_least(&fall_ctx, 0., movement.z, max_up_step as f64) {
        if movement.z.abs() <= min_movement {
            movement.z = 0.;
            break;
        }

        movement.z -= min_movement_z
    }
    while movement.x != 0.0
        && movement.z != 0.0
        && can_fall_at_least(&fall_ctx, movement.x, movement.z, max_up_step as f64)
    {
        if movement.x.abs() <= min_movement {
            movement.x = 0.;
        } else {
            movement.x -= min_movement_x;
        }
        if movement.z.abs() <= min_movement {
            movement.z = 0.;
        } else {
            movement.z -= min_movement_z;
        }
    }

    movement
}

fn get_max_up_step(attributes: &Attributes) -> f32 {
    // this would be different if we were riding an entity
    attributes.step_height.calculate() as f32
}

fn is_above_ground(ctx: &CanFallAtLeastCtx, max_up_step: f32) -> bool {
    ctx.physics.on_ground()
        && ctx.physics.fall_distance < max_up_step as f64
        && !can_fall_at_least(ctx, 0., 0., max_up_step as f64 - ctx.physics.fall_distance)
}

pub struct CanFallAtLeastCtx<'world, 'state, 'a, 'b> {
    physics: &'a Physics,
    world: &'a BotWorld<'a>,
    source_entity: Entity,
    aabb_query: &'a AabbQuery<'world, 'state, 'b>,
    collidable_entity_query: &'a CollidableEntityQuery<'world, 'state>,
}

fn can_fall_at_least(
    ctx: &CanFallAtLeastCtx,
    delta_x: f64,
    delta_z: f64,
    max_up_step: f64,
) -> bool {
    let aabb = ctx.physics.bounding_box;
    let aabb = Aabb {
        min: Vec3 {
            x: aabb.min.x + EPSILON + delta_x,
            y: aabb.min.y - max_up_step - EPSILON,
            z: aabb.min.z + EPSILON + delta_z,
        },
        max: Vec3 {
            x: aabb.max.x - EPSILON + delta_x,
            y: aabb.min.y,
            z: aabb.max.z - EPSILON + delta_z,
        },
    };
    no_collision(
        ctx.world,
        Some(ctx.source_entity),
        ctx.aabb_query,
        ctx.collidable_entity_query,
        ctx.physics,
        &aabb,
        false,
    )
}

fn collide_with_shapes(
    mut movement: Vec3,
    mut entity_box: Aabb,
    collision_boxes: &[VoxelShape],
) -> Vec3 {
    if collision_boxes.is_empty() {
        return movement;
    }

    if movement.y != 0. {
        movement.y = Shapes::collide(Axis::Y, &entity_box, collision_boxes, movement.y);
        if movement.y != 0. {
            entity_box = entity_box.move_relative(Vec3::new(0., movement.y, 0.));
        }
    }

    // whether the player is moving more in the z axis than x
    // this is done to fix a movement bug, minecraft does this too
    let more_z_movement = movement.x.abs() < movement.z.abs();

    if more_z_movement && movement.z != 0. {
        movement.z = Shapes::collide(Axis::Z, &entity_box, collision_boxes, movement.z);
        if movement.z != 0. {
            entity_box = entity_box.move_relative(Vec3::new(0., 0., movement.z));
        }
    }

    if movement.x != 0. {
        movement.x = Shapes::collide(Axis::X, &entity_box, collision_boxes, movement.x);
        if movement.x != 0. {
            entity_box = entity_box.move_relative(Vec3::new(movement.x, 0., 0.));
        }
    }

    if !more_z_movement && movement.z != 0. {
        movement.z = Shapes::collide(Axis::Z, &entity_box, collision_boxes, movement.z);
    }

    movement
}

/// Get the [`VoxelShape`] for the given fluid state.
///
/// The world and position are required so it can check if the block above is
/// also the same fluid type.
pub fn fluid_shape(fluid: &FluidState, world: &impl BlockView, pos: BlockPos) -> &'static VoxelShape {
    if fluid.amount == 9 {
        let fluid_state_above = world.get_fluid_state(pos.up(1)).unwrap_or_default();
        if fluid_state_above.kind == fluid.kind {
            return &BLOCK_SHAPE;
        }
    }
    if fluid.amount > 9 {
        warn!("Tried to calculate shape for fluid with height > 9: {fluid:?} at {pos}");
        return &EMPTY_SHAPE;
    }

    // pre-calculate these in a LazyLock so this function can return a
    // reference instead

    static FLUID_SHAPES: LazyLock<[VoxelShape; 10]> = LazyLock::new(|| {
        [
            calculate_shape_for_fluid(0),
            calculate_shape_for_fluid(1),
            calculate_shape_for_fluid(2),
            calculate_shape_for_fluid(3),
            calculate_shape_for_fluid(4),
            calculate_shape_for_fluid(5),
            calculate_shape_for_fluid(6),
            calculate_shape_for_fluid(7),
            calculate_shape_for_fluid(8),
            calculate_shape_for_fluid(9),
        ]
    });

    &FLUID_SHAPES[fluid.amount as usize]
}
fn calculate_shape_for_fluid(amount: u8) -> VoxelShape {
    box_shape(0.0, 0.0, 0.0, 1.0, (f32::from(amount) / 9.0) as f64, 1.0)
}

/// Whether the block is treated as "motion blocking".
///
/// This is marked as deprecated in Minecraft.
pub fn legacy_blocks_motion(block: BlockState) -> bool {
    if block == BlockState::AIR {
        // fast path
        return false;
    }

    let registry_block = BlockKind::from(block);
    legacy_calculate_solid(block)
        && registry_block != BlockKind::Cobweb
        && registry_block != BlockKind::BambooSapling
}

pub fn legacy_calculate_solid(block: BlockState) -> bool {
    // force_solid has to be checked before anything else
    if let Some(solid) = block.behavior().force_solid {
        return solid;
    }

    let shape = block.collision_shape();
    if shape.is_empty() {
        return false;
    }
    let bounds = shape.bounds();
    bounds.size() >= 0.7291666666666666 || bounds.get_size(Axis::Y) >= 1.0
}

#[cfg(test)]
mod step_up_tests {
    use super::*;

    fn run(on_ground: bool, bottom: f64, ledge_top: f64, movement: Vec3) -> Vec3 {
        // A floor with top at y 0 and a ledge from x 0.3 on, top at `ledge_top`.
        let floor = box_shape(-5., -1., -5., 5., 0., 5.);
        let ledge = box_shape(0.3, 0., -5., 5., ledge_top, 5.);
        let aabb = Aabb {
            min: Vec3::new(-0.3, bottom, -0.3),
            max: Vec3::new(0.3, bottom + 1.8, 0.3),
        };
        collide_with(&aabb, on_ground, 0.6, movement, &[], &|_| {
            vec![floor.clone(), ledge.clone()]
        })
    }

    // Vanilla steps from the box after the downward part of the move (bottom 0, after the
    // 0.125 drop), so the ledge at 0.6875 is above the 0.6 cap and the move is blocked.
    #[test]
    fn step_cap_counts_from_the_landing_box() {
        let r = run(false, 0.125, 0.6875, Vec3::new(0.1, -0.3, 0.));
        assert_eq!(r.x, 0., "blocked, got {r:?}");
        assert_eq!(r.y, -0.125, "lands on the floor, got {r:?}");
    }

    // Same ledge from the ground: the start box is the landing box, cap 0.6 again.
    #[test]
    fn grounded_ledge_above_cap_blocks() {
        let r = run(true, 0., 0.6875, Vec3::new(0.1, 0., 0.));
        assert_eq!(r.x, 0.0, "{r:?}");
    }

    // A ledge of 0.5 from the landing box (bottom 0 after the drop) is climbed: the result
    // is relative to the start box, so y is 0.5 - 0.125.
    #[test]
    fn landing_box_step_within_cap_climbs() {
        let r = run(false, 0.125, 0.5, Vec3::new(0.1, -0.3, 0.));
        assert_eq!(r.x, 0.1, "{r:?}");
        assert!((r.y - 0.375).abs() < 1e-9, "{r:?}");
    }

    // Start bottom 0.125, ledge top 0.7: 0.575 above the start box, 0.7 above the landing box.
    // The old code climbed it; vanilla caps from the landing box and blocks.
    #[test]
    fn ledge_inside_start_cap_but_outside_landing_cap_blocks() {
        let r = run(false, 0.125, 0.7, Vec3::new(0.1, -0.3, 0.));
        assert_eq!(r.x, 0., "{r:?}");
    }

    // On the ground with no downward move, a 0.5 ledge is climbed.
    #[test]
    fn grounded_low_ledge_climbs() {
        let r = run(true, 0., 0.5, Vec3::new(0.1, 0., 0.));
        assert_eq!(r.x, 0.1, "{r:?}");
        assert!((r.y - 0.5).abs() < 1e-9, "{r:?}");
    }

    // In the air (not on ground, no floor hit) there is no step.
    #[test]
    fn airborne_does_not_step() {
        let r = run(false, 0.4, 0.5, Vec3::new(0.1, 0., 0.));
        assert_eq!(r.x, 0., "{r:?}");
    }
}
