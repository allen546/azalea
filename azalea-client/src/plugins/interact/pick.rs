use azalea_core::{
    aabb::Aabb,
    direction::Direction,
    entity_id::MinecraftEntityId,
    hit_result::{BlockHitResult, EntityHitResult, HitResult},
    position::Vec3,
};
use azalea_entity::{
    Attributes, Dead, LocalEntity, LookDirection, Physics, Position,
    dimensions::EntityDimensions,
    interpolation::PeerView,
    metadata::{
        AbstractArrow, AbstractBoat, AbstractLiving, AbstractMinecart, ArmorStand,
        ArmorStandMarker, EndCrystal, FallingBlock, InGround, Interaction, ShulkerBullet, Tnt,
    },
    view_vector,
};
use azalea_physics::{
    clip::{BlockShapeType, ClipContext, FluidPickType},
    collision::VoxelShape,
    collision::entity_collisions::{AabbQuery, get_entities},
};
use azalea_world::{World, WorldName, Worlds};
use bevy_ecs::prelude::*;
use derive_more::{Deref, DerefMut};

/// A component that contains the block or entity that the player is currently
/// looking at.
#[doc(alias("looking at", "looking at block", "crosshair"))]
#[derive(Clone, Component, Debug, Deref, DerefMut)]
pub struct HitResultComponent(HitResult);

/// Cast the crosshair ray once per `GameTick`, at the point where vanilla casts it.
///
/// Vanilla 1.21.11 (Mojang names): `Minecraft#tick` calls `GameRenderer#pick(1.0F)` near the top
/// of every client tick, after this tick's packets and the mouse look are in, and before
/// `MultiPlayerGameMode#tick` and `Minecraft#handleKeybinds` (`startAttack`, `startUseItem`,
/// `continueAttack`), so every click of a tick is judged against that tick's ray. The other
/// `pick` call, in `GameRenderer#renderLevel`, only feeds the frame (outline, debug); Null skips
/// display, so there is no per-`Update` pick any more. After a stall, one `Update` runs up to 10
/// `GameTick`s back to back (patch 23); each one picks again, so a click never sees a ray older
/// than its own tick's look.
///
/// Grim checks this must satisfy: `RotationPlace` and `PositionPlace` (scaffolding) test the
/// placed face against the player's look and eye when the place packet arrives, and
/// `Reach` / `Hitboxes` (combat) do the same for entity interaction, so the click has to come
/// from a look whose ray really hits the target at that tick.
///
/// Ordered after the systems that turn the look (`handle_look_at_block_queued`,
/// `handle_look_at_entity_queued`) and before every system that checks a click against the hit.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes); specs/B6-placement-breaking.md §4.2
#[allow(clippy::type_complexity)]
pub fn update_hit_result_component(
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            Option<&mut HitResultComponent>,
            &Position,
            &EntityDimensions,
            &LookDirection,
            &WorldName,
            &Physics,
            &Attributes,
            Option<&PeerView>,
        ),
        With<LocalEntity>,
    >,
    worlds: Res<Worlds>,
    aabb_query: AabbQuery,
    pickable_query: MaybePickableEntityQuery,
) {
    for (entity, hit_result_ref, position, dimensions, look_direction, world_name, physics, attributes, peers) in
        &mut query
    {
        let Some(hit_result) = cast_hit_result(
            entity,
            position,
            dimensions,
            look_direction,
            world_name,
            physics,
            attributes,
            peers,
            &worlds,
            &aabb_query,
            &pickable_query,
        ) else {
            continue;
        };
        if let Some(mut hit_result_ref) = hit_result_ref {
            **hit_result_ref = hit_result;
        } else {
            commands
                .entity(entity)
                .insert(HitResultComponent(hit_result));
        }
    }
}

/// Give a client its first [`HitResultComponent`] in `Update`, so code that reads it before the
/// first `GameTick` (the `Update`-side mining event handler) finds one. Later updates are made by
/// [`update_hit_result_component`] in `GameTick`.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
#[allow(clippy::type_complexity)]
pub fn insert_initial_hit_result_component(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            &Position,
            &EntityDimensions,
            &LookDirection,
            &WorldName,
            &Physics,
            &Attributes,
        ),
        (With<LocalEntity>, Without<HitResultComponent>),
    >,
    worlds: Res<Worlds>,
    aabb_query: AabbQuery,
    pickable_query: MaybePickableEntityQuery,
) {
    for (entity, position, dimensions, look_direction, world_name, physics, attributes) in &query {
        if let Some(hit_result) = cast_hit_result(
            entity,
            position,
            dimensions,
            look_direction,
            world_name,
            physics,
            attributes,
            None,
            &worlds,
            &aabb_query,
            &pickable_query,
        ) {
            commands
                .entity(entity)
                .insert(HitResultComponent(hit_result));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cast_hit_result(
    entity: Entity,
    position: &Position,
    dimensions: &EntityDimensions,
    look_direction: &LookDirection,
    world_name: &WorldName,
    physics: &Physics,
    attributes: &Attributes,
    peers: Option<&PeerView>,
    worlds: &Worlds,
    aabb_query: &AabbQuery,
    pickable_query: &MaybePickableEntityQuery,
) -> Option<HitResult> {
    let block_pick_range = attributes.block_interaction_range.calculate();
    let entity_pick_range = attributes.entity_interaction_range.calculate();
    let eye_position = position.up(dimensions.eye_height.into());
    let world_lock = worlds.get(world_name)?;
    let world = world_lock.read();
    Some(pick(PickOpts {
        source_entity: entity,
        look_direction: *look_direction,
        eye_position,
        aabb: &physics.bounding_box,
        world: &world,
        entity_pick_range,
        block_pick_range,
        aabb_query,
        pickable_query,
        peers,
    }))
}

pub type MaybePickableEntityQuery<'world, 'state, 'a> = Query<
    'world,
    'state,
    (
        Option<&'a ArmorStandMarker>,
        Option<&'a InGround>,
        Option<&'a MinecraftEntityId>,
    ),
    // search "isPickable" in the decompiled minecraft code
    (
        Or<(
            // TODO: ender dragon parts are pickable but the ender dragon itself isn't. this needs
            // more investigation.
            (With<Tnt>, Without<Dead>),
            (With<FallingBlock>, Without<Dead>),
            (With<AbstractMinecart>, Without<Dead>),
            (With<AbstractBoat>, Without<Dead>),
            With<ArmorStand>,
            With<EndCrystal>,
            With<Interaction>,
            With<ShulkerBullet>,
            (With<AbstractLiving>, Without<Dead>),
            With<AbstractArrow>,
        )>,
    ),
>;

pub struct PickOpts<'world, 'state, 'a, 'b, 'c> {
    pub source_entity: Entity,
    pub look_direction: LookDirection,
    pub eye_position: Vec3,
    pub aabb: &'a Aabb,
    pub world: &'a World,
    pub entity_pick_range: f64,
    pub block_pick_range: f64,
    pub aabb_query: &'a AabbQuery<'world, 'state, 'b>,
    pub pickable_query: &'a MaybePickableEntityQuery<'world, 'state, 'c>,
    /// The picking bot's view of the other bots (patch 31): a bot in it is picked at the
    /// smoothed box this bot sees, not at the other bot's own.
    pub peers: Option<&'a PeerView>,
}

/// Get the block or entity that a player would be looking at if their eyes were
/// at the given direction and position.
///
/// If you need to get the block/entity the player is looking at right now, use
/// [`HitResultComponent`].
///
/// Also see [`pick_block`].
pub fn pick(opts: PickOpts<'_, '_, '_, '_, '_>) -> HitResult {
    // vanilla does extra math here to calculate the pick result in between ticks by
    // interpolating, but since clients can still only interact on exact ticks, that
    // isn't relevant for us.

    let mut max_range = opts.entity_pick_range.max(opts.block_pick_range);
    let mut max_range_squared = max_range.powi(2);

    let block_hit_result = pick_block(
        opts.look_direction,
        opts.eye_position,
        &opts.world.chunks,
        max_range,
    );
    let block_hit_result_dist_squared = block_hit_result
        .location
        .distance_squared_to(opts.eye_position);
    if !block_hit_result.miss {
        max_range_squared = block_hit_result_dist_squared;
        max_range = block_hit_result_dist_squared.sqrt();
    }

    let view_vector = view_vector(opts.look_direction);
    let end_position = opts.eye_position + (view_vector * max_range);
    let inflate_by = 1.;
    let pick_aabb = opts
        .aabb
        .expand_towards(view_vector * max_range)
        .inflate_all(inflate_by);

    // Spec: specs/F5-version-layer.md §4.6 (What Null computes). Patch 40: the picking client's own Minecraft id. In a shared world another bot holds a
    // second entity with this id (its view of us); it must never be picked.
    let own_id = opts
        .pickable_query
        .get(opts.source_entity)
        .ok()
        .and_then(|(_, _, id)| id.copied());

    let is_pickable = |entity: Entity| {
        if entity == opts.source_entity {
            return false;
        }

        // TODO: ender dragon has extra logic here. also, we shouldn't be able to pick
        // spectators.
        if let Ok((armor_stand_marker, arrow_in_ground, id)) = opts.pickable_query.get(entity) {
            // Vanilla `ProjectileUtil#getEntityHitResult` gets its candidates from
            // `Level#getEntities(camera, box, predicate)`, which leaves out the camera entity (the
            // local player); the same id is never a candidate here.
            if own_id.is_some() && id.copied() == own_id {
                return false;
            }
            !(armor_stand_marker == Some(&ArmorStandMarker(true))
                || arrow_in_ground == Some(&InGround(true)))
        } else {
            false
        }
    };
    let entity_hit_result = pick_entity(PickEntityOpts {
        source_entity: opts.source_entity,
        eye_position: opts.eye_position,
        end_position,
        world: opts.world,
        pick_range_squared: max_range_squared,
        predicate: &is_pickable,
        aabb: &pick_aabb,
        aabb_query: opts.aabb_query,
        peers: opts.peers,
    });

    if let Some(entity_hit_result) = entity_hit_result
        && entity_hit_result
            .location
            .distance_squared_to(opts.eye_position)
            < block_hit_result_dist_squared
    {
        filter_hit_result(
            HitResult::Entity(entity_hit_result),
            opts.eye_position,
            opts.entity_pick_range,
        )
    } else {
        filter_hit_result(
            HitResult::Block(block_hit_result),
            opts.eye_position,
            opts.block_pick_range,
        )
    }
}

fn filter_hit_result(hit_result: HitResult, eye_position: Vec3, range: f64) -> HitResult {
    let location = hit_result.location();
    if !location.closer_than(eye_position, range) {
        let direction = Direction::nearest(location - eye_position);
        HitResult::new_miss(location, direction, location.into())
    } else {
        hit_result
    }
}

/// Get the block that a player would be looking at if their eyes were at the
/// given direction and position.
///
/// This does not consider entities.
///
/// Also see [`pick`].
pub fn pick_block(
    look_direction: LookDirection,
    eye_position: Vec3,
    chunks: &azalea_world::ChunkStorage,
    pick_range: f64,
) -> BlockHitResult {
    let view_vector = view_vector(look_direction);
    let end_position = eye_position + (view_vector * pick_range);

    let hit = azalea_physics::clip::clip(
        chunks,
        ClipContext {
            from: eye_position,
            to: end_position,
            block_shape_type: BlockShapeType::Outline,
            fluid_pick_type: FluidPickType::None,
        },
    );
    with_interaction_override(chunks, eye_position, end_position, hit)
}

/// Vanilla `getInteractionShape`: for a cauldron, hopper, composter or scaffolding, an interaction
/// shape that the ray enters before the outline's hit point changes the face of the hit (not its
/// location). azalea-physics' `clip` leaves this as a TODO, so it is applied here to the block
/// the ray hit, which is the only one vanilla applies it to.
///
/// Shapes from vanilla 1.21.11, Mojang names: `AbstractCauldronBlock#getInteractionShape`,
/// `HopperBlock#getInteractionShape` (per `FACING`), `ComposterBlock#getInteractionShape`,
/// `ScaffoldingBlock#getInteractionShape`; logic from `BlockGetter#clipWithInteractionOverride`.
fn with_interaction_override(
    chunks: &azalea_world::ChunkStorage,
    from: Vec3,
    to: Vec3,
    hit: BlockHitResult,
) -> BlockHitResult {
    use azalea_block::properties::Facing;
    use azalea_physics::collision::{Shapes, box_shape};
    use azalea_registry::builtin::BlockKind;
    use std::sync::LazyLock;
    static CAULDRON: LazyLock<VoxelShape> =
        LazyLock::new(|| box_shape(0.125, 0.25, 0.125, 0.875, 1.0, 0.875));
    // The hopper's bowl; a sideways hopper adds its spout on the facing side.
    static HOPPER_DOWN: LazyLock<VoxelShape> =
        LazyLock::new(|| box_shape(0.125, 0.6875, 0.125, 0.875, 1.0, 0.875));
    static HOPPER_SIDE: LazyLock<[VoxelShape; 4]> = LazyLock::new(|| {
        let spout = |x0, z0, x1, z1| {
            Shapes::or(HOPPER_DOWN.clone(), box_shape(x0, 0.5, z0, x1, 0.625, z1))
        };
        [
            spout(0.375, 0.0, 0.625, 0.25),
            spout(0.75, 0.375, 1.0, 0.625),
            spout(0.375, 0.75, 0.625, 1.0),
            spout(0.0, 0.375, 0.25, 0.625),
        ]
    });
    static FULL: LazyLock<VoxelShape> = LazyLock::new(|| box_shape(0., 0., 0., 1., 1., 1.));
    if hit.miss {
        return hit;
    }
    let Some(state) = chunks.get_block_state(hit.block_pos) else { return hit };
    let shape: &VoxelShape = match BlockKind::from(state) {
        BlockKind::Cauldron
        | BlockKind::WaterCauldron
        | BlockKind::LavaCauldron
        | BlockKind::PowderSnowCauldron => &CAULDRON,
        BlockKind::Hopper => match state.property::<Facing>() {
            Some(Facing::North) => &HOPPER_SIDE[0],
            Some(Facing::East) => &HOPPER_SIDE[1],
            Some(Facing::South) => &HOPPER_SIDE[2],
            Some(Facing::West) => &HOPPER_SIDE[3],
            _ => &HOPPER_DOWN,
        },
        BlockKind::Composter | BlockKind::Scaffolding => &FULL,
        _ => return hit,
    };
    match shape.clip(from, to, hit.block_pos) {
        Some(inner)
            if inner.location.distance_squared_to(from) < hit.location.distance_squared_to(from) =>
        {
            hit.with_direction(inner.direction)
        }
        _ => hit,
    }
}

struct PickEntityOpts<'world, 'state, 'a, 'b> {
    source_entity: Entity,
    eye_position: Vec3,
    end_position: Vec3,
    world: &'a azalea_world::World,
    pick_range_squared: f64,
    predicate: &'a dyn Fn(Entity) -> bool,
    aabb: &'a Aabb,
    aabb_query: &'a AabbQuery<'world, 'state, 'b>,
    peers: Option<&'a PeerView>,
}

// port of getEntityHitResult
fn pick_entity(opts: PickEntityOpts) -> Option<EntityHitResult> {
    let mut picked_distance_squared = opts.pick_range_squared;
    let mut result = None;

    // Patch 31: a bot in the picking bot's view is a candidate at the box in that view; the world
    // lookup skips it (it would give the other bot's own box).
    let in_view = |e: Entity| opts.peers.is_some_and(|v| v.contains(e));
    let world_predicate = |e: Entity| !in_view(e) && (opts.predicate)(e);
    let mut candidates = get_entities(
        opts.world,
        Some(opts.source_entity),
        opts.aabb,
        &world_predicate,
        opts.aabb_query,
    );
    if let Some(view) = opts.peers {
        for (peer, entry) in view.iter() {
            if peer != opts.source_entity
                && (opts.predicate)(peer)
                && opts.aabb.intersects_aabb(&entry.bounding_box)
            {
                candidates.push((peer, entry.bounding_box));
            }
        }
    }
    for (candidate, candidate_aabb) in candidates {
        // TODO: if the entity is "REDIRECTABLE_PROJECTILE" then this should be 1.0.
        // azalea needs support for entity tags first for this to be possible. see
        // getPickRadius in decompiled minecraft source
        let candidate_pick_radius = 0.;
        let candidate_aabb = candidate_aabb.inflate_all(candidate_pick_radius);
        let clip_location = candidate_aabb.clip(opts.eye_position, opts.end_position);

        if candidate_aabb.contains(opts.eye_position) {
            if picked_distance_squared >= 0. {
                result = Some(EntityHitResult {
                    location: clip_location.unwrap_or(opts.eye_position),
                    entity: candidate,
                });
                picked_distance_squared = 0.;
            }
        } else if let Some(clip_location) = clip_location {
            let distance_squared = opts.eye_position.distance_squared_to(clip_location);
            if distance_squared < picked_distance_squared || picked_distance_squared == 0. {
                // TODO: don't pick the entity we're riding on
                // if candidate_root_vehicle == entity_root_vehicle {
                //     if picked_distance_squared == 0. {
                //         picked_entity = Some(candidate);
                //         picked_location = Some(clip_location);
                //     }
                // } else {
                result = Some(EntityHitResult {
                    location: clip_location,
                    entity: candidate,
                });
                picked_distance_squared = distance_squared;
            }
        }
    }

    result
}
