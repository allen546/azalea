//! Client-side place prediction, stage 1 (Null fork, patch 33).
//!
//! Vanilla 1.21.11 writes a placed block into its own world at once (`MultiPlayerGameMode#
//! useItemOn` -> `startPrediction` -> `performUseItemOn` -> `BlockItem#place` -> `Level#setBlock`,
//! which `ClientLevel#setBlock` retains for a rollback) and the server's update or ack settles
//! it (`BlockStatePredictionHandler#endPredictionsUpTo` -> `ClientLevel#syncBlockState`). The
//! fork used to write nothing until the server's update arrived, so for one round trip the bot's
//! world lacked a block that the server and Grim already had: a bot that stood on it or walked
//! into it moved as if it was missing, and Grim flagged `Simulation`.
//!
//! The safety rule: the bot's world may be wrong only where its physics does not read it. Inside
//! the movement zone ([`movement_zone`]) a place is predicted exactly or refused on the client
//! with a reason and nothing is sent. Outside it, a place that stage 1 cannot compute is sent
//! unpredicted, as vanilla would send it whatever its local result.
//!
//! Stage 1 computes the placed state for the families in `azalea_block::PlaceFamily` (plain,
//! axis, waterloggable, slab, horizontal facing), the replace rule, and the entity obstruction
//! (`Level#isUnobstructed`: the placer and every other entity with `blocksBuilding`). Neighbour
//! shape updates (`Level#setBlock` flags 11 -> `updateNeighbourShapes`) are stage 3: a neighbour
//! that may react, inside the zone, refuses the place.
//!
//! Grim (`BlockPlace`, `CheckManagerListener`) works out the placed state itself, skips it when
//! its collision meets the player's box or an entity's box, and writes it into its own world at
//! once. An unpredicted or wrongly shaped block near the bot costs `Simulation`.
//!
//! Spec: specs/B6-placement-breaking.md §4.10 (place prediction)

use azalea_block::{
    BlockState, BlockTrait, PlaceFamily, indirect_shape_block, place_family, plain_replace_rule,
    shape_inert,
};
use azalea_core::{
    aabb::Aabb,
    direction::Direction,
    game_type::GameMode,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_physics::collision::BlockWithShape;
use azalea_registry::{builtin::BlockKind, tags};
use azalea_entity::{
    EntityKindComponent, EntityUuid, LocalEntity, Physics, Position,
    dimensions::EntityDimensions,
    indexing::EntityIdIndex,
    interpolation::PeerView,
    metadata::{AbstractBoat, AbstractLiving, AbstractMinecart},
    LoadedBy,
};
use azalea_registry::builtin::EntityKind;
use azalea_world::ChunkStorage;
use bevy_ecs::{prelude::*, query::QueryData, system::SystemParam};

use crate::{
    local_player::{LocalGameMode, TabList},
    movement::LastSentLookDirection,
};

/// Why stage 1 cannot compute a place. Every reason is stable text for a refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownWhy {
    /// Adventure mode: placing needs the item's `can_place_on` list.
    Adventure,
    /// The held item is not a plain block item of a stage 1 family (or not a block item at all).
    Family,
    /// The held stack carries a `block_state` component (`BlockItem#updateBlockStateFromTag`).
    BlockStateComponent,
    /// The clicked or target block is not in a loaded chunk.
    Unloaded,
    /// The clicked or target block has its own replace rule (`canBeReplaced` override).
    ReplaceRule,
    /// An entity that may or may not block building overlaps the cell (an armor stand).
    EntityUnsure,
    /// The facing family: the yaw the server uses (the last one sent) differs from the current one.
    FacingYaw,
}

impl UnknownWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Adventure => "adventure mode: placing depends on the item's can_place_on list",
            Self::Family => "the held item's placed state is not predicted (not a stage 1 block family)",
            Self::BlockStateComponent => "the held stack has a block_state component",
            Self::Unloaded => "the target is not in a loaded chunk",
            Self::ReplaceRule => "the target block has its own replace rule",
            Self::EntityUnsure => "an entity that may block building overlaps the cell",
            Self::FacingYaw => "the yaw the server knows differs from the current yaw",
        }
    }
}

/// What stage 1 says about one place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Vanilla writes this state into the world.
    Predict { pos: BlockPos, state: BlockState },
    /// Vanilla's local result writes nothing (FAIL or PASS): the packet goes out, nothing is
    /// predicted. The text says which rule.
    Nothing(&'static str),
    /// Stage 1 cannot compute it.
    Unknown(UnknownWhy),
}

/// An entity box that may stop a block from being placed.
#[derive(Clone, Copy, Debug)]
pub struct Obstacle {
    pub aabb: Aabb,
    /// False when whether the entity blocks building depends on state the fork does not read
    /// (an armor stand's marker flag).
    pub certain: bool,
}

/// Everything one place needs.
pub struct PlaceRequest<'a> {
    pub clicked: BlockPos,
    pub face: Direction,
    /// Where the ray hit, in world coordinates.
    pub hit: Vec3,
    /// The held item as `"minecraft:dirt"`; empty for an empty hand.
    pub held: &'a str,
    pub has_block_state_component: bool,
    pub game_mode: GameMode,
    /// The yaw now, and the yaw the server knows (the last one sent).
    pub yaw: f32,
    pub sent_yaw: f32,
    /// The placer's box, then every other entity box near the cell.
    pub obstacles: &'a [Obstacle],
}

/// Per axis, how far the bot can move in the two ticks around a click, at the most: twice the
/// speed it has now plus [`ACCEL_BOUND`] per tick, and never under one block.
///
/// Vanilla bounds one tick of the player's own motion by what it has (`Entity#move` moves by the
/// velocity) and by what one tick adds to it: a jump is 0.42 (plus 0.1 per level of Jump Boost,
/// capped here), a sprint jump adds about 0.2 on the ground axes, gravity 0.08, and ladder or
/// water currents less. Two ticks cover the tick the click lands in and the next one, which is
/// the one a late server correction would hit. A server push (velocity packet) can exceed it;
/// that arrives as a packet and is read, not predicted.
pub const ACCEL_BOUND: f64 = 0.5;

/// The movement zone: the bot's box grown by two ticks of its largest possible motion, and at
/// least one block on each side. Cells inside it are read by the bot's physics soon.
// Spec: specs/B6-placement-breaking.md §4.10 (the movement zone)
pub fn movement_zone(bot_box: &Aabb, velocity: Vec3) -> Aabb {
    let grow = |v: f64| (2.0 * (v.abs() + ACCEL_BOUND)).max(1.0);
    Aabb {
        min: Vec3::new(
            bot_box.min.x - grow(velocity.x),
            bot_box.min.y - grow(velocity.y),
            bot_box.min.z - grow(velocity.z),
        ),
        max: Vec3::new(
            bot_box.max.x + grow(velocity.x),
            bot_box.max.y + grow(velocity.y),
            bot_box.max.z + grow(velocity.z),
        ),
    }
}

/// The unit cube of a cell.
pub fn cell_box(pos: BlockPos) -> Aabb {
    Aabb {
        min: Vec3::new(pos.x as f64, pos.y as f64, pos.z as f64),
        max: Vec3::new(pos.x as f64 + 1.0, pos.y as f64 + 1.0, pos.z as f64 + 1.0),
    }
}

/// Vanilla `Direction#fromYRot`: the horizontal direction a yaw faces.
fn horizontal_direction(yaw: f32) -> Direction {
    match ((yaw as f64 / 90.0 + 0.5).floor() as i32) & 3 {
        0 => Direction::South,
        1 => Direction::West,
        2 => Direction::North,
        _ => Direction::East,
    }
}

fn axis_name(face: Direction) -> &'static str {
    match face {
        Direction::Down | Direction::Up => "y",
        Direction::North | Direction::South => "z",
        Direction::East | Direction::West => "x",
    }
}

fn direction_name(d: Direction) -> &'static str {
    match d {
        Direction::Down => "down",
        Direction::Up => "up",
        Direction::North => "north",
        Direction::South => "south",
        Direction::West => "west",
        Direction::East => "east",
    }
}

/// Whether the cell holds a water source (`FluidState#getType() == Fluids.WATER`, not flowing
/// water): a waterlogged block, a water block at level 0, or a plant that holds source water.
fn is_water_source(state: BlockState) -> bool {
    if state.get_property("waterlogged") == Some("true") {
        return true;
    }
    match BlockKind::from(state) {
        BlockKind::Water => state.get_property("level") == Some("0"),
        BlockKind::BubbleColumn | BlockKind::Seagrass | BlockKind::TallSeagrass | BlockKind::Kelp | BlockKind::KelpPlant => {
            true
        }
        _ => false,
    }
}

fn with_properties(kind: BlockKind, props: &[(&str, &str)]) -> Option<BlockState> {
    let mut block: Box<dyn BlockTrait> = kind.into();
    for (name, value) in props {
        block.set_property(name, value).ok()?;
    }
    Some(block.as_block_state())
}

/// Whether placing `held` on the cell `state` replaces it (vanilla `BlockState#canBeReplaced
/// (BlockPlaceContext)`), when the context is `replacing_clicked` (the cell is the clicked
/// block) or the cell next to it. `None` for a block with a replace rule stage 1 does not model.
/// `hit_y` is the hit height inside the cell.
// Spec: specs/B6-placement-breaking.md §4.10 (the replace rule)
fn replaceable(state: BlockState, held: &str, replacing_clicked: bool, face: Direction, hit_y: f64) -> Option<bool> {
    let kind = BlockKind::from(state);
    let same_item = !held.is_empty() && held == kind.to_str();
    if kind == BlockKind::Snow {
        let layers: u8 = state.get_property("layers").and_then(|l| l.parse().ok()).unwrap_or(8);
        return Some(if same_item && layers < 8 {
            !replacing_clicked || face == Direction::Up
        } else {
            layers == 1
        });
    }
    if kind.to_str().ends_with("_slab") {
        let horizontal = !matches!(face, Direction::Up | Direction::Down);
        let ty = state.get_property("type");
        if ty == Some("double") || !same_item {
            return Some(false);
        }
        if !replacing_clicked {
            return Some(true);
        }
        return Some(match ty {
            Some("bottom") => face == Direction::Up || (hit_y > 0.5 && horizontal),
            Some("top") => face == Direction::Down || (hit_y <= 0.5 && horizontal),
            _ => false,
        });
    }
    if !plain_replace_rule(kind) {
        return None;
    }
    Some(tags::blocks::REPLACEABLE.contains(&kind) && !same_item)
}

/// The state a place writes, or why not. Follows `BlockItem#place` for a plain block item.
// Spec: specs/B6-placement-breaking.md §4.10 (the placed state)
pub fn predict_place(chunks: &ChunkStorage, req: &PlaceRequest) -> Verdict {
    // MultiPlayerGameMode#performUseItemOn: spectators do nothing locally.
    match req.game_mode {
        GameMode::Spectator => return Verdict::Nothing("spectator: the client does nothing"),
        GameMode::Adventure => return Verdict::Unknown(UnknownWhy::Adventure),
        _ => {}
    }
    // ItemStack#isEmpty -> PASS
    if req.held.is_empty() {
        return Verdict::Nothing("empty hand");
    }
    let Ok(kind) = req.held.parse::<BlockKind>() else { return Verdict::Unknown(UnknownWhy::Family) };
    let family = place_family(kind);
    if family == PlaceFamily::Unknown {
        return Verdict::Unknown(UnknownWhy::Family);
    }
    if req.has_block_state_component {
        return Verdict::Unknown(UnknownWhy::BlockStateComponent);
    }
    let Some(clicked) = chunks.get_block_state(req.clicked) else {
        return Verdict::Unknown(UnknownWhy::Unloaded);
    };
    // BlockPlaceContext: the clicked cell if it is replaceable, else the cell beside it.
    let hit_y_clicked = req.hit.y - req.clicked.y as f64;
    let Some(replace_clicked) = replaceable(clicked, req.held, true, req.face, hit_y_clicked) else {
        return Verdict::Unknown(UnknownWhy::ReplaceRule);
    };
    let target = if replace_clicked { req.clicked } else { req.clicked.offset_with_direction(req.face) };
    let Some(before) = chunks.get_block_state(target) else {
        return Verdict::Unknown(UnknownWhy::Unloaded);
    };
    // BlockPlaceContext#canPlace: a cell beside the click must be replaceable too.
    if !replace_clicked {
        match replaceable(before, req.held, false, req.face, req.hit.y - target.y as f64) {
            None => return Verdict::Unknown(UnknownWhy::ReplaceRule),
            Some(false) => return Verdict::Nothing("the target cell is not replaceable"),
            Some(true) => {}
        }
    }

    // Block#getStateForPlacement
    let default = BlockState::from(kind);
    let state = match family {
        PlaceFamily::Plain => Some(default),
        PlaceFamily::Axis => with_properties(kind, &[("axis", axis_name(req.face))]),
        PlaceFamily::Waterloggable => {
            with_properties(kind, &[("waterlogged", if is_water_source(before) { "true" } else { "false" })])
        }
        PlaceFamily::Slab => {
            if BlockKind::from(before) == kind {
                // SlabBlock: the same slab kind in the cell becomes a double slab, never waterlogged.
                with_properties(kind, &[("type", "double"), ("waterlogged", "false")])
            } else {
                let water = if is_water_source(before) { "true" } else { "false" };
                let bottom = req.face != Direction::Down
                    && (req.face == Direction::Up || req.hit.y - target.y as f64 <= 0.5);
                with_properties(kind, &[("type", if bottom { "bottom" } else { "top" }), ("waterlogged", water)])
            }
        }
        PlaceFamily::HorizontalFacing | PlaceFamily::HorizontalFacingSame => {
            let now = horizontal_direction(req.yaw);
            // The server places with the yaw it knows, which is the last one sent.
            if now != horizontal_direction(req.sent_yaw) {
                return Verdict::Unknown(UnknownWhy::FacingYaw);
            }
            let facing = if family == PlaceFamily::HorizontalFacing { now.opposite() } else { now };
            with_properties(kind, &[("facing", direction_name(facing))])
        }
        PlaceFamily::Unknown => None,
    };
    let Some(state) = state else { return Verdict::Unknown(UnknownWhy::Family) };

    // BlockItem#canPlace: canSurvive is true for every stage 1 family (the generator checks that
    // no class in the chain overrides it); then Level#isUnobstructed.
    let shape = state.collision_shape();
    if !shape.is_empty() {
        let cell = Vec3::new(target.x as f64, target.y as f64, target.z as f64);
        let mut unsure = false;
        for part in shape.to_aabbs() {
            let part = part.move_relative(cell);
            for o in req.obstacles {
                if part.intersects_aabb(&o.aabb) {
                    if o.certain {
                        return Verdict::Nothing("an entity is in the way (Level#isUnobstructed)");
                    }
                    unsure = true;
                }
            }
        }
        if unsure {
            return Verdict::Unknown(UnknownWhy::EntityUnsure);
        }
    }
    // Level#setBlock returns false for an unchanged state, so BlockItem#place fails.
    if state == before {
        return Verdict::Nothing("the cell already holds that state");
    }
    Verdict::Predict { pos: target, state }
}

/// Whether placing at `pos` can change a neighbour's shape that the bot's physics reads soon: a
/// neighbour that reacts (`updateShape`, stage 3) or a redstone-like block two cells away, with
/// the cell inside the zone. Returns the cell.
// Spec: specs/B6-placement-breaking.md §4.10 (neighbour shape updates are stage 3)
pub fn reactive_neighbour_in_zone(chunks: &ChunkStorage, pos: BlockPos, zone: &Aabb) -> Option<BlockPos> {
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                if (dx, dy, dz) == (0, 0, 0) {
                    continue;
                }
                let direct = (dx != 0) as i32 + (dy != 0) as i32 + (dz != 0) as i32 == 1;
                let cell = BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
                if !cell_box(cell).intersects_aabb(zone) {
                    continue;
                }
                let Some(state) = chunks.get_block_state(cell) else { continue };
                let kind = BlockKind::from(state);
                if (direct && !shape_inert(kind)) || indirect_shape_block(kind) {
                    return Some(cell);
                }
            }
        }
    }
    None
}

/// Why a place was refused on the client by the safety rule (nothing sent).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaceRefusal {
    /// Stage 1 cannot compute the placed state and the cell is inside the movement zone.
    NotPredicted(UnknownWhy),
    /// A neighbour of the target cell may change its shape and sits inside the movement zone.
    NeighbourReacts(BlockPos),
}

/// What to do with a place once stage 1 has its verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Send the packet; write the state first when there is one.
    Send { write: Option<(BlockPos, BlockState)> },
    Refuse(PlaceRefusal),
}

/// The safety rule: inside `zone` a place is predicted exactly or refused; outside it, a place
/// that cannot be computed is sent unpredicted. `cells` are the cells the place may touch (the
/// clicked cell and the one beside it).
// Spec: specs/B6-placement-breaking.md §4.10 (the safety rule)
pub fn decide(chunks: &ChunkStorage, verdict: Verdict, cells: [BlockPos; 2], zone: &Aabb) -> Decision {
    match verdict {
        Verdict::Predict { pos, state } => match reactive_neighbour_in_zone(chunks, pos, zone) {
            Some(cell) => Decision::Refuse(PlaceRefusal::NeighbourReacts(cell)),
            None => Decision::Send { write: Some((pos, state)) },
        },
        Verdict::Nothing(_) => Decision::Send { write: None },
        Verdict::Unknown(why) => {
            if cells.iter().any(|c| cell_box(*c).intersects_aabb(zone)) {
                Decision::Refuse(PlaceRefusal::NotPredicted(why))
            } else {
                Decision::Send { write: None }
            }
        }
    }
}

/// The entity data the obstruction check reads.
#[derive(QueryData)]
pub struct ObstacleData {
    position: &'static Position,
    dimensions: &'static EntityDimensions,
    kind: &'static EntityKindComponent,
    uuid: &'static EntityUuid,
    loaded_by: Option<&'static LoadedBy>,
    living: Has<AbstractLiving>,
    boat: Has<AbstractBoat>,
    minecart: Has<AbstractMinecart>,
    local: Has<LocalEntity>,
}

/// What the bot knows that a place reads besides its world.
#[derive(QueryData)]
pub struct PlaceBotData {
    physics: &'static Physics,
    last_look: &'static LastSentLookDirection,
    game_mode: &'static LocalGameMode,
    ids: &'static EntityIdIndex,
    peers: Option<&'static PeerView>,
    tab_list: Option<&'static TabList>,
}

/// The queries stage 1 needs, bundled so the place system stays within bevy's parameter limit.
#[derive(SystemParam)]
pub struct PlaceView<'w, 's> {
    bots: Query<'w, 's, PlaceBotData>,
    entities: Query<'w, 's, ObstacleData>,
}

/// Everything the place system learns from one look at the bot: the movement zone, the entity
/// boxes near the cell, the mode and the yaw the server knows.
pub struct PlaceContext {
    pub zone: Aabb,
    pub game_mode: GameMode,
    pub sent_yaw: f32,
    pub obstacles: Vec<Obstacle>,
}

// Spec: specs/B6-placement-breaking.md §4.10 (entity obstruction)
/// `Some(certain)` when the entity may block building (`Entity#blocksBuilding`); `certain` is
/// false when it depends on state the fork does not read.
fn blocks_building(d: &ObstacleDataItem) -> Option<bool> {
    let kind = **d.kind;
    if kind == EntityKind::ArmorStand {
        // blocksBuilding is `!isMarker()`; the marker flag is not read
        return Some(false);
    }
    let blocks = d.living
        || d.boat
        || d.minecart
        || matches!(kind, EntityKind::Tnt | EntityKind::FallingBlock | EntityKind::EndCrystal);
    blocks.then_some(true)
}

impl PlaceView<'_, '_> {
    /// Gather the context for a place by `bot`, whose box is at `position`/`dims`, around the cells
    /// `cells`.
    // Spec: specs/B6-placement-breaking.md §4.10 (entity obstruction; the movement zone)
    pub fn context(
        &self,
        bot: Entity,
        world: &azalea_world::World,
        position: Vec3,
        dims: &EntityDimensions,
        cells: [BlockPos; 2],
    ) -> Option<PlaceContext> {
        let data = self.bots.get(bot).ok()?;
        let bot_box = dims.make_bounding_box(position);
        let mut obstacles = vec![Obstacle { aabb: bot_box, certain: true }];
        let spectator_player = |d: &ObstacleDataItem| {
            **d.kind == EntityKind::Player
                && data.tab_list.and_then(|t| t.get(&**d.uuid)).is_some_and(|i| i.gamemode == GameMode::Spectator)
        };
        // the chunks within one of the cells (an entity's box reaches at most a chunk away)
        let mut chunk_set = Vec::new();
        for c in cells {
            let (cx, cz) = (c.x.div_euclid(16), c.z.div_euclid(16));
            for dx in -1..=1 {
                for dz in -1..=1 {
                    let p = ChunkPos::new(cx + dx, cz + dz);
                    if !chunk_set.contains(&p) {
                        chunk_set.push(p);
                    }
                }
            }
        }
        for chunk in chunk_set {
            let Some(in_chunk) = world.entities_by_chunk.get(&chunk) else { continue };
            for &entity in in_chunk {
                if entity == bot {
                    continue;
                }
                let Ok(d) = self.entities.get(entity) else { continue };
                // another bot is read from this bot's own view below, never from its exact position
                if d.local
                    || !d.loaded_by.is_some_and(|l| l.contains(&bot))
                    || !data.ids.contains_ecs_entity(entity)
                {
                    continue;
                }
                let Some(certain) = blocks_building(&d) else { continue };
                if spectator_player(&d) {
                    continue;
                }
                let aabb = d.dimensions.make_bounding_box(**d.position);
                obstacles.push(Obstacle { aabb, certain });
            }
        }
        // the other bots this bot sees (patch 31): their smoothed boxes
        if let Some(view) = data.peers {
            for (peer, entry) in view.iter() {
                let Ok(d) = self.entities.get(peer) else { continue };
                let Some(certain) = blocks_building(&d) else { continue };
                obstacles.push(Obstacle { aabb: entry.bounding_box, certain });
            }
        }
        Some(PlaceContext {
            zone: movement_zone(&bot_box, data.physics.velocity),
            game_mode: data.game_mode.current,
            sent_yaw: data.last_look.y_rot,
            obstacles,
        })
    }
}

#[cfg(test)]
mod tests {
    use azalea_core::position::ChunkPos;
    use azalea_world::{Chunk, ChunkStorage, PartialChunkStorage};

    use super::*;

    const FLOOR: i32 = 64;

    /// A loaded chunk (0, 0) with a stone floor at `FLOOR`. Keep the second value alive: the
    /// storage holds chunks weakly.
    fn world() -> (ChunkStorage, PartialChunkStorage) {
        let mut chunks = ChunkStorage::new(384, -64);
        let mut partial = PartialChunkStorage::new(2);
        partial.set(&ChunkPos::new(0, 0), Some(Chunk::default()), &mut chunks);
        for x in 0..16 {
            for z in 0..16 {
                chunks.set_block_state(BlockPos::new(x, FLOOR, z), BlockState::from(BlockKind::Stone));
            }
        }
        (chunks, partial)
    }

    fn req<'a>(held: &'a str, clicked: BlockPos, face: Direction, hit: Vec3, obstacles: &'a [Obstacle]) -> PlaceRequest<'a> {
        PlaceRequest {
            clicked,
            face,
            hit,
            held,
            has_block_state_component: false,
            game_mode: GameMode::Survival,
            yaw: 0.0,
            sent_yaw: 0.0,
            obstacles,
        }
    }

    fn top_of_floor() -> (BlockPos, Vec3) {
        (BlockPos::new(2, FLOOR, 2), Vec3::new(2.5, FLOOR as f64 + 1.0, 2.5))
    }

    fn predicted(v: Verdict) -> (BlockPos, BlockState) {
        match v {
            Verdict::Predict { pos, state } => (pos, state),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_block_goes_on_the_clicked_face() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        let (pos, state) = predicted(predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[])));
        assert_eq!(pos, BlockPos::new(2, FLOOR + 1, 2));
        assert_eq!(BlockKind::from(state), BlockKind::Dirt);
    }

    #[test]
    fn axis_follows_the_clicked_face() {
        let (chunks, _keep) = world();
        for (face, axis) in [(Direction::Up, "y"), (Direction::North, "z"), (Direction::East, "x")] {
            chunks.set_block_state(BlockPos::new(2, FLOOR + 1, 2), BlockState::from(BlockKind::Stone));
            let (clicked, hit) = (BlockPos::new(2, FLOOR + 1, 2), Vec3::new(2.5, FLOOR as f64 + 1.5, 2.5));
            let (_, state) = predicted(predict_place(&chunks, &req("minecraft:oak_log", clicked, face, hit, &[])));
            assert_eq!(state.get_property("axis"), Some(axis), "{face:?}");
        }
    }

    #[test]
    fn slab_is_bottom_top_or_double() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        // up face: bottom
        let (pos, s) = predicted(predict_place(&chunks, &req("minecraft:oak_slab", clicked, Direction::Up, hit, &[])));
        assert_eq!((s.get_property("type"), s.get_property("waterlogged")), (Some("bottom"), Some("false")));
        // down face of a block above: top
        let ceil = BlockPos::new(2, FLOOR + 5, 2);
        chunks.set_block_state(ceil, BlockState::from(BlockKind::Stone));
        let (p, s) = predicted(predict_place(
            &chunks,
            &req("minecraft:oak_slab", ceil, Direction::Down, Vec3::new(2.5, FLOOR as f64 + 5.0, 2.5), &[]),
        ));
        assert_eq!((p, s.get_property("type")), (BlockPos::new(2, FLOOR + 4, 2), Some("top")));
        // side face, hit in the upper half: top; lower half: bottom
        let wall = BlockPos::new(2, FLOOR + 1, 4);
        chunks.set_block_state(wall, BlockState::from(BlockKind::Stone));
        for (y, ty) in [(0.75, "top"), (0.25, "bottom")] {
            let hit = Vec3::new(2.5, (FLOOR + 1) as f64 + y, 4.0);
            let (p, s) = predicted(predict_place(&chunks, &req("minecraft:oak_slab", wall, Direction::North, hit, &[])));
            assert_eq!((p, s.get_property("type")), (BlockPos::new(2, FLOOR + 1, 3), Some(ty)), "{y}");
        }
        // a second slab of the same kind on the first completes it
        let lower = with_properties(BlockKind::OakSlab, &[("type", "bottom"), ("waterlogged", "false")]).unwrap();
        chunks.set_block_state(pos, lower);
        let (p, s) = predicted(predict_place(&chunks, &req("minecraft:oak_slab", clicked, Direction::Up, hit, &[])));
        assert_eq!((p, s.get_property("type")), (pos, Some("double")));
        // a different slab kind goes on top instead
        let (p, s) = predicted(predict_place(
            &chunks,
            &req("minecraft:birch_slab", pos, Direction::Up, Vec3::new(2.5, FLOOR as f64 + 1.5, 2.5), &[]),
        ));
        assert_eq!(p, pos.up(1));
        assert_eq!(s.get_property("type"), Some("bottom"));
        // a double slab takes nothing more
        let double = with_properties(BlockKind::OakSlab, &[("type", "double"), ("waterlogged", "false")]).unwrap();
        chunks.set_block_state(pos, double);
        let (p, _) = predicted(predict_place(&chunks, &req("minecraft:oak_slab", pos, Direction::Up, Vec3::new(2.5, (FLOOR + 2) as f64, 2.5), &[])));
        assert_eq!(p, pos.up(1));
    }

    #[test]
    fn waterlogged_only_from_a_water_source() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        let target = BlockPos::new(2, FLOOR + 1, 2);
        let (_, s) = predicted(predict_place(&chunks, &req("minecraft:barrier", clicked, Direction::Up, hit, &[])));
        assert_eq!(s.get_property("waterlogged"), Some("false"));
        let water = |level: &str| with_properties(BlockKind::Water, &[("level", level)]).unwrap();
        chunks.set_block_state(target, water("0"));
        let (_, s) = predicted(predict_place(&chunks, &req("minecraft:barrier", clicked, Direction::Up, hit, &[])));
        assert_eq!(s.get_property("waterlogged"), Some("true"));
        // flowing water is not the water source type
        chunks.set_block_state(target, water("3"));
        let (_, s) = predicted(predict_place(&chunks, &req("minecraft:barrier", clicked, Direction::Up, hit, &[])));
        assert_eq!(s.get_property("waterlogged"), Some("false"));
        // a slab in a source takes the water
        chunks.set_block_state(target, water("0"));
        let (_, s) = predicted(predict_place(&chunks, &req("minecraft:oak_slab", clicked, Direction::Up, hit, &[])));
        assert_eq!(s.get_property("waterlogged"), Some("true"));
    }

    #[test]
    fn facing_follows_the_yaw_and_the_yaw_the_server_knows() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        // yaw 0 faces south; the block faces the opposite way, toward the player
        let mut r = req("minecraft:stonecutter", clicked, Direction::Up, hit, &[]);
        assert_eq!(predicted(predict_place(&chunks, &r)).1.get_property("facing"), Some("north"));
        r.yaw = 90.0;
        r.sent_yaw = 90.0;
        assert_eq!(predicted(predict_place(&chunks, &r)).1.get_property("facing"), Some("east"));
        r.yaw = -90.0;
        r.sent_yaw = -90.0;
        assert_eq!(predicted(predict_place(&chunks, &r)).1.get_property("facing"), Some("west"));
        // the yaw changed this tick and the server has not seen it
        r.sent_yaw = 0.0;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::FacingYaw));
    }

    #[test]
    fn replace_rule_puts_the_block_into_replaceable_cells() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        // short grass is replaced, whatever face is clicked
        chunks.set_block_state(clicked, BlockState::from(BlockKind::ShortGrass));
        let (pos, _) = predicted(predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[])));
        assert_eq!(pos, clicked);
        // the target beside the click holding a non-replaceable block: nothing, exactly
        chunks.set_block_state(clicked, BlockState::from(BlockKind::Stone));
        chunks.set_block_state(clicked.up(1), BlockState::from(BlockKind::Stone));
        assert!(matches!(
            predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[])),
            Verdict::Nothing(_)
        ));
        // a block with its own replace rule is not modelled
        chunks.set_block_state(clicked.up(1), BlockState::from(BlockKind::Vine));
        assert_eq!(
            predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[])),
            Verdict::Unknown(UnknownWhy::ReplaceRule)
        );
    }

    #[test]
    fn an_entity_in_the_cell_blocks_the_place() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        let inside = |certain| Obstacle {
            aabb: Aabb { min: Vec3::new(2.2, FLOOR as f64 + 1.0, 2.2), max: Vec3::new(2.8, FLOOR as f64 + 2.8, 2.8) },
            certain,
        };
        assert!(matches!(
            predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[inside(true)])),
            Verdict::Nothing(_)
        ));
        assert_eq!(
            predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[inside(false)])),
            Verdict::Unknown(UnknownWhy::EntityUnsure)
        );
        // an entity that only touches the cell does not block (joinIsNotEmpty is strict)
        let touching = Obstacle {
            aabb: Aabb { min: Vec3::new(2.2, FLOOR as f64, 2.2), max: Vec3::new(2.8, FLOOR as f64 + 1.0, 2.8) },
            certain: true,
        };
        assert!(matches!(
            predict_place(&chunks, &req("minecraft:dirt", clicked, Direction::Up, hit, &[touching])),
            Verdict::Predict { .. }
        ));
        // a cell with no collision (barrier has one; use a block with an empty shape: cobweb)
        assert!(matches!(
            predict_place(&chunks, &req("minecraft:cobweb", clicked, Direction::Up, hit, &[inside(true)])),
            Verdict::Predict { .. }
        ));
    }

    #[test]
    fn what_stage_one_cannot_compute_is_unknown() {
        let (chunks, _keep) = world();
        let (clicked, hit) = top_of_floor();
        for held in ["minecraft:oak_fence", "minecraft:oak_stairs", "minecraft:torch", "minecraft:diamond_sword", "minecraft:chest"] {
            assert_eq!(
                predict_place(&chunks, &req(held, clicked, Direction::Up, hit, &[])),
                Verdict::Unknown(UnknownWhy::Family),
                "{held}"
            );
        }
        let mut r = req("minecraft:dirt", clicked, Direction::Up, hit, &[]);
        r.game_mode = GameMode::Adventure;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::Adventure));
        r.game_mode = GameMode::Spectator;
        assert!(matches!(predict_place(&chunks, &r), Verdict::Nothing(_)));
        r.game_mode = GameMode::Survival;
        r.has_block_state_component = true;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::BlockStateComponent));
        // an empty hand is vanilla's PASS
        assert!(matches!(
            predict_place(&chunks, &req("", clicked, Direction::Up, hit, &[])),
            Verdict::Nothing(_)
        ));
    }

    #[test]
    fn the_zone_grows_with_speed_and_is_a_block_at_least() {
        let b = Aabb { min: Vec3::new(0.0, 64.0, 0.0), max: Vec3::new(0.6, 65.8, 0.6) };
        let z = movement_zone(&b, Vec3::ZERO);
        assert_eq!((z.min.x, z.max.x, z.min.y, z.max.y), (-1.0, 1.6, 63.0, 66.8));
        let z = movement_zone(&b, Vec3::new(0.0, -3.0, 0.0));
        assert_eq!((z.min.y, z.max.y), (64.0 - 7.0, 65.8 + 7.0));
    }

    #[test]
    fn only_reactive_neighbours_inside_the_zone_matter() {
        let (chunks, _keep) = world();
        let pos = BlockPos::new(2, FLOOR + 1, 2);
        let zone = Aabb { min: Vec3::new(1.0, FLOOR as f64, 1.0), max: Vec3::new(4.0, FLOOR as f64 + 3.0, 4.0) };
        assert_eq!(reactive_neighbour_in_zone(&chunks, pos, &zone), None);
        chunks.set_block_state(BlockPos::new(3, FLOOR + 1, 2), BlockState::from(BlockKind::OakFence));
        assert_eq!(reactive_neighbour_in_zone(&chunks, pos, &zone), Some(BlockPos::new(3, FLOOR + 1, 2)));
        // the same fence outside the zone is left alone
        let far = Aabb { min: Vec3::new(10.0, FLOOR as f64, 10.0), max: Vec3::new(12.0, FLOOR as f64 + 3.0, 12.0) };
        assert_eq!(reactive_neighbour_in_zone(&chunks, pos, &far), None);
        // a stone neighbour never matters
        chunks.set_block_state(BlockPos::new(3, FLOOR + 1, 2), BlockState::from(BlockKind::Stone));
        assert_eq!(reactive_neighbour_in_zone(&chunks, pos, &zone), None);
    }
}
