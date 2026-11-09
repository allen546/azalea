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
use azalea_physics::collision::BlockShapeAt;
use azalea_registry::{builtin::BlockKind, tags};
use azalea_entity::{
    EntityKindComponent, EntityUuid, Physics, Position,
    dimensions::EntityDimensions,
    indexing::EntityIdIndex,
    metadata::{AbstractBoat, AbstractLiving, AbstractMinecart, ArmorStandMarker},
    LoadedBy,
};
use azalea_registry::builtin::EntityKind;
use azalea_world::BlockView;
use bevy_ecs::{prelude::*, query::QueryData, system::SystemParam};

use super::place_rules;
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
    /// A support shape the fork does not model decides the state (snow layers, fence gates, shulker
    /// boxes, chorus flowers, hanging signs, multiface blocks).
    Support,
    /// Redstone power at the cell cannot be shown to be absent from the blocks around it.
    Signal,
    /// Sneaking changed this tick, so the server's view of it is not the client's.
    SecondaryUse,
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
            Self::Support => "a neighbour's support shape is not modelled",
            Self::Signal => "redstone power at the cell is not provably absent",
            Self::SecondaryUse => "the sneak state is changing this tick",
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
#[derive(Clone, Copy)]
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
    /// The pitch now, and the pitch the server knows (the families that look up or down).
    pub pitch: f32,
    pub sent_pitch: f32,
    /// `Player#isSecondaryUseActive` as the server knows it; `None` while that is changing.
    pub secondary_use: Option<bool>,
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
pub(crate) fn horizontal_direction(yaw: f32) -> Direction {
    match ((yaw as f64 / 90.0 + 0.5).floor() as i32) & 3 {
        0 => Direction::South,
        1 => Direction::West,
        2 => Direction::North,
        _ => Direction::East,
    }
}

pub(crate) fn clockwise(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::East,
        Direction::East => Direction::South,
        Direction::South => Direction::West,
        Direction::West => Direction::North,
        other => other,
    }
}

pub(crate) fn counter_clockwise(d: Direction) -> Direction {
    clockwise(clockwise(clockwise(d)))
}

pub(crate) fn direction_from_name(name: &str) -> Option<Direction> {
    Some(match name {
        "down" => Direction::Down,
        "up" => Direction::Up,
        "north" => Direction::North,
        "south" => Direction::South,
        "west" => Direction::West,
        "east" => Direction::East,
        _ => return None,
    })
}

/// Vanilla `Direction#orderedByNearest(player)[0]`: the axis the look vector is longest on, with
/// the sign of that component. `None` when two compared lengths are within a hair of each other:
/// vanilla reads its sine table there and the pick could differ from this exact maths.
// Spec: specs/B6-placement-breaking.md §4.10 (families that look: patch 67)
fn nearest_looking(yaw: f32, pitch: f32) -> Option<Direction> {
    const TIE: f64 = 1e-3;
    let (f, g) = ((pitch as f64).to_radians(), -(yaw as f64).to_radians());
    let (h, i, j, k) = (f.sin(), f.cos(), g.sin(), g.cos());
    let (l, m, n) = (j.abs(), h.abs(), k.abs());
    let (o, p) = (l * i, n * i);
    let x = if j > 0.0 { Direction::East } else { Direction::West };
    let y = if h < 0.0 { Direction::Up } else { Direction::Down };
    let z = if k > 0.0 { Direction::South } else { Direction::North };
    let near = |a: f64, b: f64| (a - b).abs() < TIE;
    // the pick when the x length wins, and when the z length wins
    let pick_x = if near(m, o) { None } else { Some(if m > o { y } else { x }) };
    let pick_z = if near(m, p) { None } else { Some(if m > p { y } else { z }) };
    if near(l, n) {
        // either branch may apply: only a shared answer (looking steeply up or down) is safe
        return if pick_x == pick_z { pick_x } else { None };
    }
    if l > n { pick_x } else { pick_z }
}

pub(crate) fn axis_name(face: Direction) -> &'static str {
    match face {
        Direction::Down | Direction::Up => "y",
        Direction::North | Direction::South => "z",
        Direction::East | Direction::West => "x",
    }
}

pub(crate) fn direction_name(d: Direction) -> &'static str {
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
pub(crate) fn is_water_source(state: BlockState) -> bool {
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

pub(crate) fn with_properties(kind: BlockKind, props: &[(&str, &str)]) -> Option<BlockState> {
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
pub fn predict_place(chunks: &impl BlockView, req: &PlaceRequest) -> Verdict {
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
        // AnvilBlock: the horizontal direction turned a quarter clockwise.
        PlaceFamily::FacingClockwise => {
            let now = horizontal_direction(req.yaw);
            if now != horizontal_direction(req.sent_yaw) {
                return Verdict::Unknown(UnknownWhy::FacingYaw);
            }
            with_properties(kind, &[("facing", direction_name(clockwise(now)))])
        }
        // DispenserBlock, BarrelBlock, PistonBaseBlock (opposite) and ObserverBlock (same): the
        // nearest looking direction, which the server reads from the look it was last sent.
        PlaceFamily::LookingOpposite | PlaceFamily::LookingSame => {
            let (Some(now), Some(sent)) =
                (nearest_looking(req.yaw, req.pitch), nearest_looking(req.sent_yaw, req.sent_pitch))
            else {
                return Verdict::Unknown(UnknownWhy::FacingYaw);
            };
            if now != sent {
                return Verdict::Unknown(UnknownWhy::FacingYaw);
            }
            let facing = if family == PlaceFamily::LookingOpposite { now.opposite() } else { now };
            with_properties(kind, &[("facing", direction_name(facing))])
        }
        // ChainBlock: RotatedPillarBlock's axis plus the water of the cell.
        PlaceFamily::AxisWaterlogged => with_properties(
            kind,
            &[("axis", axis_name(req.face)), ("waterlogged", if is_water_source(before) { "true" } else { "false" })],
        ),
        // LightningRodBlock: faces the clicked face.
        PlaceFamily::ClickedFaceWaterlogged => with_properties(
            kind,
            &[("facing", direction_name(req.face)), ("waterlogged", if is_water_source(before) { "true" } else { "false" })],
        ),
        // VegetationBlock keeps the default state; its canSurvive is checked below.
        PlaceFamily::Vegetation => Some(default),
        // SnowyDirtBlock: snowy when the block above is in the snow tag.
        PlaceFamily::SnowyTop => {
            let Some(above) = chunks.get_block_state(target.up(1)) else {
                return Verdict::Unknown(UnknownWhy::Unloaded);
            };
            let snowy = tags::blocks::SNOW.contains(&BlockKind::from(above));
            with_properties(kind, &[("snowy", if snowy { "true" } else { "false" })])
        }
        // HugeMushroomBlock: a side shows unless the block beside it is the same block.
        PlaceFamily::HugeMushroom => {
            let mut props = Vec::with_capacity(6);
            for (name, dir) in [
                ("down", Direction::Down),
                ("up", Direction::Up),
                ("north", Direction::North),
                ("east", Direction::East),
                ("south", Direction::South),
                ("west", Direction::West),
            ] {
                let Some(next) = chunks.get_block_state(target.offset_with_direction(dir)) else {
                    return Verdict::Unknown(UnknownWhy::Unloaded);
                };
                props.push((name, if BlockKind::from(next) == kind { "false" } else { "true" }));
            }
            with_properties(kind, &props)
        }
        PlaceFamily::Stairs => {
            let facing = horizontal_direction(req.yaw);
            if facing != horizontal_direction(req.sent_yaw) {
                return Verdict::Unknown(UnknownWhy::FacingYaw);
            }
            // StairBlock#getStateForPlacement: bottom unless the face is down or the click is in
            // the upper half of a side face.
            let bottom = req.face != Direction::Down
                && (req.face == Direction::Up || req.hit.y - target.y as f64 <= 0.5);
            let Some(shape) = stairs_shape(chunks, target, facing, bottom) else {
                return Verdict::Unknown(UnknownWhy::Unloaded);
            };
            with_properties(
                kind,
                &[
                    ("facing", direction_name(facing)),
                    ("half", if bottom { "bottom" } else { "top" }),
                    ("waterlogged", if is_water_source(before) { "true" } else { "false" }),
                    ("shape", shape),
                ],
            )
        }
        PlaceFamily::Unknown => None,
        // Families that read neighbours, the look or redstone: see place_rules.rs.
        _ => {
            let cx = place_rules::Cx { chunks, req, kind, target, before, replace_clicked };
            match place_rules::state_for(&cx, family) {
                place_rules::Rule::State(st) => Some(st),
                place_rules::Rule::Fail(why) => return Verdict::Nothing(why),
                place_rules::Rule::Unknown(why) => return Verdict::Unknown(why),
            }
        }
    };
    let Some(state) = state else { return Verdict::Unknown(UnknownWhy::Family) };

    // VegetationBlock#canSurvive (BlockItem#canPlace): dirt tag or farmland below the cell. A
    // failed check is a client FAIL: the packet goes out, nothing is predicted.
    if family == PlaceFamily::Vegetation {
        let Some(below) = chunks.get_block_state(target.down(1)) else {
            return Verdict::Unknown(UnknownWhy::Unloaded);
        };
        let below = BlockKind::from(below);
        if !(tags::blocks::DIRT.contains(&below) || below == BlockKind::Farmland) {
            return Verdict::Nothing("the plant cannot survive on the block below (canSurvive)");
        }
    }

    // BlockItem#canPlace: canSurvive is true for every stage 1 family (the generator checks that
    // no class in the chain overrides it); then Level#isUnobstructed.
    let shape = state.collision_shape_at(target);
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

/// `StairBlock#getStairsShape` for a stair about to stand in `pos`: outer corner from the stair in
/// front, inner corner from the one behind, else straight. `None` when a neighbour is not loaded.
// Spec: specs/B6-placement-breaking.md §4.10 (stairs: patch 71)
fn stairs_shape(chunks: &impl BlockView, pos: BlockPos, facing: Direction, bottom: bool) -> Option<&'static str> {
    let half = if bottom { "bottom" } else { "top" };
    // (facing, half) of a stair in the cell, None for any other block
    let stair_at = |cell: BlockPos| -> Option<Option<(Direction, bool)>> {
        let st = chunks.get_block_state(cell)?;
        if !BlockKind::from(st).to_str().ends_with("_stairs") {
            return Some(None);
        }
        let f = direction_from_name(st.get_property("facing")?)?;
        Some(Some((f, st.get_property("half")? == half)))
    };
    // canTakeShape: no stair of the same facing and half in the cell on the given side
    let can_take = |dir: Direction| -> Option<bool> {
        Some(match stair_at(pos.offset_with_direction(dir))? {
            Some((f, same_half)) => !(same_half && f == facing),
            None => true,
        })
    };
    let axis_z = |d: Direction| matches!(d, Direction::North | Direction::South);
    if let Some((f, true)) = stair_at(pos.offset_with_direction(facing))? {
        if axis_z(f) != axis_z(facing) && can_take(f.opposite())? {
            return Some(if f == counter_clockwise(facing) { "outer_left" } else { "outer_right" });
        }
    }
    if let Some((f, true)) = stair_at(pos.offset_with_direction(facing.opposite()))? {
        if axis_z(f) != axis_z(facing) && can_take(f)? {
            return Some(if f == counter_clockwise(facing) { "inner_left" } else { "inner_right" });
        }
    }
    Some("straight")
}

/// What `Minecraft#startUseItem` does with the swing after the client-side try of a place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientSwing {
    /// `InteractionResult.Success` with the client swing source: the client swings.
    Yes,
    /// `FAIL`, `PASS` or `CONSUME`: no swing (and after `FAIL` no other hand is tried).
    No,
    /// The fork cannot compute the client result (an `Unknown` verdict it cannot resolve). Known gap: the swing is
    /// sent as before, as it is right for a success, which is the common case.
    Unknown,
}

/// Whether the vanilla client swings after this place. `BlockItem#place` returns `SUCCESS` when it
/// writes a block and `FAIL` for every refusal (an entity in the way, a cell that cannot be
/// replaced, an unchanged state, a block that cannot survive there); a spectator gets `CONSUME`
/// and an empty hand `PASS`, neither swings.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 48: the swing follows the client result)
pub fn client_swing(chunks: &impl BlockView, req: &PlaceRequest, verdict: &Verdict) -> ClientSwing {
    match verdict {
        Verdict::Predict { .. } => ClientSwing::Yes,
        Verdict::Nothing(_) => ClientSwing::No,
        Verdict::Unknown(UnknownWhy::Family) => match sapling_place_succeeds(chunks, req) {
            Some(true) => ClientSwing::Yes,
            Some(false) => ClientSwing::No,
            None => match redstone_wire_place_succeeds(chunks, req) {
                Some(true) => ClientSwing::Yes,
                Some(false) => ClientSwing::No,
                None => ClientSwing::Unknown,
            },
        },
        // The facing differs only in the state written, not in whether the place succeeds: work
        // the result out as if the server's yaw were the current one.
        Verdict::Unknown(UnknownWhy::FacingYaw) => {
            match predict_place(chunks, &PlaceRequest { sent_yaw: req.yaw, ..*req }) {
                Verdict::Predict { .. } => ClientSwing::Yes,
                Verdict::Nothing(_) => ClientSwing::No,
                Verdict::Unknown(_) => ClientSwing::Unknown,
            }
        }
        Verdict::Unknown(_) => ClientSwing::Unknown,
    }
}

/// A sapling is the one item outside the stage 1 families that the client result can be computed
/// for: no collision (nothing obstructs it) and `VegetationBlock#canSurvive` wants a block of the
/// `dirt` tag or farmland below the cell. `None` for any other item.
fn sapling_place_succeeds(chunks: &impl BlockView, req: &PlaceRequest) -> Option<bool> {
    if !req.held.ends_with("_sapling") || req.has_block_state_component {
        return None;
    }
    let clicked = chunks.get_block_state(req.clicked)?;
    let replace_clicked = replaceable(clicked, req.held, true, req.face, req.hit.y - req.clicked.y as f64)?;
    let target = if replace_clicked { req.clicked } else { req.clicked.offset_with_direction(req.face) };
    let before = chunks.get_block_state(target)?;
    if !replace_clicked && !replaceable(before, req.held, false, req.face, req.hit.y - target.y as f64)? {
        return Some(false);
    }
    let below = BlockKind::from(chunks.get_block_state(target.down(1))?);
    Some(tags::blocks::DIRT.contains(&below) || below == BlockKind::Farmland)
}

/// Redstone dust (`minecraft:redstone` is an `ItemNameBlockItem` for `redstone_wire`): no
/// collision, so nothing obstructs it, and `RedStoneWireBlock#canSurvive` wants a sturdy top face
/// below the cell (`BlockStateBase#isFaceSturdy`) or a hopper. A bottom slab has no sturdy top,
/// so that place fails on the client (patch 61). `None` for any other item, or a block below
/// whose support shape the fork does not model.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 61: redstone wire swing)
fn redstone_wire_place_succeeds(chunks: &impl BlockView, req: &PlaceRequest) -> Option<bool> {
    if req.held != "minecraft:redstone" || req.has_block_state_component {
        return None;
    }
    let clicked = chunks.get_block_state(req.clicked)?;
    let replace_clicked = replaceable(clicked, req.held, true, req.face, req.hit.y - req.clicked.y as f64)?;
    let target = if replace_clicked { req.clicked } else { req.clicked.offset_with_direction(req.face) };
    let before = chunks.get_block_state(target)?;
    if !replace_clicked && !replaceable(before, req.held, false, req.face, req.hit.y - target.y as f64)? {
        return Some(false);
    }
    let below = chunks.get_block_state(target.down(1))?;
    if BlockKind::from(below) == BlockKind::Hopper {
        return Some(true);
    }
    azalea_physics::collision::is_face_sturdy(below, Direction::Up)
}

/// Whether placing at `pos` can change a neighbour's shape that the bot's physics reads soon: a
/// neighbour that reacts (`updateShape`, stage 3) or a redstone-like block two cells away, with
/// the cell inside the zone. Returns the cell.
// Spec: specs/B6-placement-breaking.md §4.10 (neighbour shape updates are stage 3)
pub fn reactive_neighbour_in_zone(chunks: &impl BlockView, pos: BlockPos, zone: &Aabb) -> Option<BlockPos> {
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
pub fn decide(chunks: &impl BlockView, verdict: Verdict, cells: [BlockPos; 2], zone: &Aabb) -> Decision {
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
    marker: Option<&'static ArmorStandMarker>,
    living: Has<AbstractLiving>,
    boat: Has<AbstractBoat>,
    minecart: Has<AbstractMinecart>,
}

/// What the bot knows that a place reads besides its world.
#[derive(QueryData)]
pub struct PlaceBotData {
    physics: &'static Physics,
    last_look: &'static LastSentLookDirection,
    game_mode: &'static LocalGameMode,
    ids: &'static EntityIdIndex,
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
    pub sent_pitch: f32,
    pub obstacles: Vec<Obstacle>,
}

// Spec: specs/B6-placement-breaking.md §4.10 (entity obstruction)
/// `Some(certain)` when the entity may block building (`Entity#blocksBuilding`); `certain` is
/// false when it depends on state the fork does not read.
fn blocks_building(d: &ObstacleDataItem) -> Option<bool> {
    let kind = **d.kind;
    if kind == EntityKind::ArmorStand {
        // `LivingEntity` starts with blocksBuilding true; `ArmorStand#onSyncedDataUpdated` sets it to
        // `!isMarker()` when the client flags arrive. No flags received means the default (not a marker).
        return d.marker.is_none_or(|m| !m.0).then_some(true);
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
                if !d.loaded_by.is_some_and(|l| l.contains(&bot)) || !data.ids.contains_ecs_entity(entity) {
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
        Some(PlaceContext {
            zone: movement_zone(&bot_box, data.physics.velocity),
            game_mode: data.game_mode.current,
            sent_yaw: data.last_look.y_rot,
            sent_pitch: data.last_look.x_rot,
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
            pitch: 0.0,
            sent_pitch: 0.0,
            secondary_use: Some(false),
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
        for held in ["minecraft:oak_door", "minecraft:diamond_sword", "minecraft:redstone_lamp", "minecraft:powered_rail", "minecraft:white_bed"] {
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

    fn swing_of(chunks: &ChunkStorage, r: &PlaceRequest) -> ClientSwing {
        let v = predict_place(chunks, r);
        client_swing(chunks, r, &v)
    }

    // Spec: specs/B6-placement-breaking.md §4.10 (patch 61: redstone wire swing)
    #[test]
    fn redstone_dust_swings_on_a_full_block_and_not_on_a_bottom_slab() {
        let (chunks, _p) = world();
        let (clicked, hit) = top_of_floor();
        let r = req("minecraft:redstone", clicked, Direction::Up, hit, &[]);
        assert_eq!(swing_of(&chunks, &r), ClientSwing::Yes, "stone below: canSurvive");
        let mut slab: Box<dyn BlockTrait> = BlockKind::StoneSlab.into();
        slab.set_property("type", "bottom").unwrap();
        chunks.set_block_state(clicked, slab.as_block_state());
        assert_eq!(swing_of(&chunks, &r), ClientSwing::No, "bottom slab below: FAIL");
        slab.set_property("type", "top").unwrap();
        chunks.set_block_state(clicked, slab.as_block_state());
        assert_eq!(swing_of(&chunks, &r), ClientSwing::Yes, "top slab has a sturdy top");
        let mut snow: Box<dyn BlockTrait> = BlockKind::Snow.into();
        snow.set_property("layers", "8").unwrap();
        chunks.set_block_state(clicked, snow.as_block_state());
        assert_eq!(swing_of(&chunks, &r), ClientSwing::Unknown, "snow layer support is not modelled");
    }

    // Spec: specs/B6-placement-breaking.md §4.10 (patch 61: a facing mismatch does not change the result)
    #[test]
    fn a_facing_yaw_mismatch_still_gives_the_swing_result() {
        let (chunks, _p) = world();
        let (clicked, hit) = top_of_floor();
        let mut r = req("minecraft:stonecutter", clicked, Direction::Up, hit, &[]);
        r.yaw = 90.0;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::FacingYaw));
        assert_eq!(swing_of(&chunks, &r), ClientSwing::Yes);
        // an entity that certainly blocks the cell: FAIL whatever the yaw
        let blocker = [Obstacle { aabb: cell_box(BlockPos::new(2, FLOOR + 1, 2)), certain: true }];
        let r2 = PlaceRequest { obstacles: &blocker, ..r };
        assert_eq!(swing_of(&chunks, &r2), ClientSwing::No);
    }

    // Expected states below come from reading the 1.21.11 sources named on each test (the
    // `getStateForPlacement` of the class), not from this module.

    fn place_at(chunks: &ChunkStorage, r: &PlaceRequest) -> BlockState {
        predicted(predict_place(chunks, r)).1
    }

    /// A request that clicks the top of the floor block (2, FLOOR, 2), so the cell is (2, FLOOR+1, 2).
    fn on_floor<'a>(held: &'a str, hit_y: f64) -> PlaceRequest<'a> {
        req(held, BlockPos::new(2, FLOOR, 2), Direction::Up, Vec3::new(2.5, hit_y, 2.5), &[])
    }

    fn set(chunks: &ChunkStorage, x: i32, y: i32, z: i32, kind: BlockKind, props: &[(&str, &str)]) {
        chunks.set_block_state(BlockPos::new(x, y, z), with_properties(kind, props).unwrap());
    }

    // StairBlock#getStateForPlacement, #getStairsShape
    #[test]
    fn stairs_face_the_player_and_take_half_and_water() {
        let (chunks, _keep) = world();
        let mut r = on_floor("minecraft:oak_stairs", FLOOR as f64 + 1.0);
        r.yaw = 90.0;
        r.sent_yaw = 90.0;
        let s = place_at(&chunks, &r);
        assert_eq!(
            (s.get_property("facing"), s.get_property("half"), s.get_property("shape"), s.get_property("waterlogged")),
            (Some("west"), Some("bottom"), Some("straight"), Some("false"))
        );
        // the side of a block, hit in the upper half: top
        set(&chunks, 2, FLOOR + 1, 4, BlockKind::Stone, &[]);
        let mut side = req(
            "minecraft:oak_stairs",
            BlockPos::new(2, FLOOR + 1, 4),
            Direction::North,
            Vec3::new(2.5, FLOOR as f64 + 1.75, 4.0),
            &[],
        );
        assert_eq!(place_at(&chunks, &side).get_property("half"), Some("top"));
        side.hit = Vec3::new(2.5, FLOOR as f64 + 1.25, 4.0);
        assert_eq!(place_at(&chunks, &side).get_property("half"), Some("bottom"));
        // the down face is always top
        set(&chunks, 2, FLOOR + 6, 4, BlockKind::Stone, &[]);
        let down = req("minecraft:oak_stairs", BlockPos::new(2, FLOOR + 6, 4), Direction::Down, Vec3::new(2.5, FLOOR as f64 + 6.0, 4.5), &[]);
        assert_eq!(place_at(&chunks, &down).get_property("half"), Some("top"));
        // a water source in the cell
        set(&chunks, 2, FLOOR + 1, 2, BlockKind::Water, &[("level", "0")]);
        assert_eq!(place_at(&chunks, &on_floor("minecraft:oak_stairs", FLOOR as f64 + 1.0)).get_property("waterlogged"), Some("true"));
    }

    #[test]
    fn stairs_corner_shapes_follow_the_neighbours() {
        let (chunks, _keep) = world();
        let (y, st) = (FLOOR + 1, BlockKind::OakStairs);
        // yaw 0 faces south. A stair in front (south) facing east is the counter-clockwise side: outer left
        set(&chunks, 2, y, 3, st, &[("facing", "east"), ("half", "bottom")]);
        let r = on_floor("minecraft:oak_stairs", FLOOR as f64 + 1.0);
        assert_eq!(place_at(&chunks, &r).get_property("shape"), Some("outer_left"));
        // facing west in front: outer right
        set(&chunks, 2, y, 3, st, &[("facing", "west"), ("half", "bottom")]);
        assert_eq!(place_at(&chunks, &r).get_property("shape"), Some("outer_right"));
        // another half in front does not count
        set(&chunks, 2, y, 3, st, &[("facing", "west"), ("half", "top")]);
        assert_eq!(place_at(&chunks, &r).get_property("shape"), Some("straight"));
        // behind (north) facing west: inner right; facing east: inner left
        set(&chunks, 2, y, 3, BlockKind::Air, &[]);
        set(&chunks, 2, y, 1, st, &[("facing", "west"), ("half", "bottom")]);
        assert_eq!(place_at(&chunks, &r).get_property("shape"), Some("inner_right"));
        set(&chunks, 2, y, 1, st, &[("facing", "east"), ("half", "bottom")]);
        assert_eq!(place_at(&chunks, &r).get_property("shape"), Some("inner_left"));
        // canTakeShape: a stair with our own facing and half on the far side blocks the corner
        set(&chunks, 2, y, 1, BlockKind::Air, &[]);
        set(&chunks, 2, y, 3, st, &[("facing", "east"), ("half", "bottom")]);
        set(&chunks, 1, y, 2, st, &[("facing", "south"), ("half", "bottom")]);
        assert_eq!(place_at(&chunks, &r).get_property("shape"), Some("straight"));
    }

    // AnvilBlock, DispenserBlock, BarrelBlock, PistonBaseBlock, ObserverBlock, AbstractFurnaceBlock
    #[test]
    fn looking_and_clockwise_families() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        let mut r = on_floor("minecraft:anvil", h);
        assert_eq!(place_at(&chunks, &r).get_property("facing"), Some("west")); // south, clockwise
        r.held = "minecraft:furnace";
        assert_eq!(place_at(&chunks, &r).get_property("facing"), Some("north")); // opposite of south
        r.held = "minecraft:dispenser";
        assert_eq!(place_at(&chunks, &r).get_property("facing"), Some("north"));
        r.held = "minecraft:observer";
        assert_eq!(place_at(&chunks, &r).get_property("facing"), Some("south"));
        for (pitch, dispenser) in [(90.0, "up"), (-90.0, "down")] {
            r.pitch = pitch;
            r.sent_pitch = pitch;
            r.held = "minecraft:barrel";
            assert_eq!(place_at(&chunks, &r).get_property("facing"), Some(dispenser), "{pitch}");
        }
        r.pitch = 0.0;
        r.sent_pitch = 0.0;
        r.yaw = 90.0;
        r.sent_yaw = 90.0;
        r.held = "minecraft:piston";
        assert_eq!(place_at(&chunks, &r).get_property("facing"), Some("east"));
        // on a diagonal vanilla's sine table decides: not predicted
        r.yaw = 45.0;
        r.sent_yaw = 45.0;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::FacingYaw));
        // the server knows another pitch than the client shows
        r.yaw = 0.0;
        r.sent_yaw = 0.0;
        r.pitch = 80.0;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::FacingYaw));
    }

    // LightningRodBlock, ChainBlock
    #[test]
    fn clicked_face_and_axis_families_take_water() {
        let (chunks, _keep) = world();
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Stone, &[]);
        let r = req("minecraft:lightning_rod", BlockPos::new(3, FLOOR + 1, 2), Direction::West, Vec3::new(3.0, FLOOR as f64 + 1.5, 2.5), &[]);
        let s = place_at(&chunks, &r);
        assert_eq!((s.get_property("facing"), s.get_property("waterlogged")), (Some("west"), Some("false")));
        set(&chunks, 2, FLOOR + 1, 2, BlockKind::Water, &[("level", "0")]);
        assert_eq!(place_at(&chunks, &r).get_property("waterlogged"), Some("true"));
        let r = req("minecraft:iron_chain", BlockPos::new(3, FLOOR + 1, 2), Direction::West, Vec3::new(3.0, FLOOR as f64 + 1.5, 2.5), &[]);
        let s = place_at(&chunks, &r);
        assert_eq!((s.get_property("axis"), s.get_property("waterlogged")), (Some("x"), Some("true")));
    }

    // VegetationBlock#canSurvive (mayPlaceOn: dirt tag or farmland)
    #[test]
    fn plants_need_dirt_or_farmland_below() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        // stone below: a client FAIL, nothing predicted
        assert!(matches!(predict_place(&chunks, &on_floor("minecraft:dandelion", h)), Verdict::Nothing(_)));
        set(&chunks, 2, FLOOR, 2, BlockKind::GrassBlock, &[]);
        assert_eq!(BlockKind::from(place_at(&chunks, &on_floor("minecraft:dandelion", h))), BlockKind::Dandelion);
        assert_eq!(place_at(&chunks, &on_floor("minecraft:oak_sapling", h)).get_property("stage"), Some("0"));
        set(&chunks, 2, FLOOR, 2, BlockKind::Farmland, &[]);
        assert_eq!(BlockKind::from(place_at(&chunks, &on_floor("minecraft:poppy", h))), BlockKind::Poppy);
        // sand is not in the dirt tag
        set(&chunks, 2, FLOOR, 2, BlockKind::Sand, &[]);
        assert!(matches!(predict_place(&chunks, &on_floor("minecraft:fern", h)), Verdict::Nothing(_)));
        // dead bush, wheat and mushrooms have their own mayPlaceOn: not predicted
        for k in ["minecraft:dead_bush", "minecraft:wheat_seeds", "minecraft:brown_mushroom", "minecraft:nether_sprouts"] {
            assert!(matches!(predict_place(&chunks, &on_floor(k, h)), Verdict::Unknown(_)), "{k}");
        }
    }

    // SnowyDirtBlock, HugeMushroomBlock
    #[test]
    fn snowy_top_and_mushroom_sides() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        assert_eq!(place_at(&chunks, &on_floor("minecraft:grass_block", h)).get_property("snowy"), Some("false"));
        set(&chunks, 2, FLOOR + 2, 2, BlockKind::SnowBlock, &[]);
        assert_eq!(place_at(&chunks, &on_floor("minecraft:grass_block", h)).get_property("snowy"), Some("true"));
        set(&chunks, 2, FLOOR + 2, 2, BlockKind::Snow, &[("layers", "1")]);
        assert_eq!(place_at(&chunks, &on_floor("minecraft:podzol", h)).get_property("snowy"), Some("true"));
        set(&chunks, 2, FLOOR + 2, 2, BlockKind::Stone, &[]);
        assert_eq!(place_at(&chunks, &on_floor("minecraft:mycelium", h)).get_property("snowy"), Some("false"));
        // a red mushroom block to the east hides our east side only; the stem does not count
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::RedMushroomBlock, &[]);
        set(&chunks, 1, FLOOR + 1, 2, BlockKind::MushroomStem, &[]);
        let s = place_at(&chunks, &on_floor("minecraft:red_mushroom_block", h));
        let sides: Vec<_> = ["down", "up", "north", "east", "south", "west"].iter().map(|n| s.get_property(n).unwrap()).collect();
        assert_eq!(sides, ["true", "true", "true", "false", "true", "true"]);
    }
    // Group tests for patch 71. Expected states come from reading the 1.21.11 sources named
    // on each test (the `getStateForPlacement` of the class), not from this module. The default
    // look is yaw 0 (south), pitch 0: Direction#orderedByNearest gives south, down, west, north,
    // up, east.

    fn side_click<'a>(held: &'a str, wall: (i32, i32, i32), face: Direction, hit_y: f64) -> PlaceRequest<'a> {
        req(held, BlockPos::new(wall.0, wall.1, wall.2), face, Vec3::new(2.5, hit_y, 2.5), &[])
    }

    fn props_of(s: BlockState, names: &[&str]) -> Vec<String> {
        names.iter().map(|n| s.get_property(n).unwrap_or("-").to_string()).collect()
    }

    // StandingAndWallBlockItem#getPlacementState, WallTorchBlock#getStateForPlacement, BaseTorchBlock#canSurvive
    #[test]
    fn torches_stand_on_a_centre_support_or_hang_on_a_sturdy_side() {
        let (chunks, _keep) = world();
        let f = FLOOR as f64;
        // on the floor: the clicked face's opposite (down) comes first, a stone floor supports the centre
        let s = place_at(&chunks, &on_floor("minecraft:torch", f + 1.0));
        assert_eq!(BlockKind::from(s), BlockKind::Torch);
        // on the south face of a stone block: the north look direction goes first, so the torch
        // hangs on that block and faces south
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::Stone, &[]);
        let r = side_click("minecraft:soul_torch", (2, FLOOR + 1, 1), Direction::South, f + 1.5);
        let s = place_at(&chunks, &r);
        assert_eq!((BlockKind::from(s), s.get_property("facing")), (BlockKind::SoulWallTorch, Some("south")));
        let r = side_click("minecraft:redstone_torch", (2, FLOOR + 1, 1), Direction::South, f + 1.5);
        assert_eq!(BlockKind::from(place_at(&chunks, &r)), BlockKind::RedstoneWallTorch);
        // on top of a fence post: a post covers the centre square
        set(&chunks, 6, FLOOR + 1, 6, BlockKind::OakFence, &[]);
        let r = req("minecraft:torch", BlockPos::new(6, FLOOR + 1, 6), Direction::Up, Vec3::new(6.5, f + 2.5, 6.5), &[]);
        assert_eq!(BlockKind::from(place_at(&chunks, &r)), BlockKind::Torch);
        // on leaves with nothing beside: no support, the client's try fails
        set(&chunks, 8, FLOOR + 1, 8, BlockKind::OakLeaves, &[("persistent", "true")]);
        let r = req("minecraft:torch", BlockPos::new(8, FLOOR + 1, 8), Direction::Up, Vec3::new(8.5, f + 2.0, 8.5), &[]);
        assert!(matches!(predict_place(&chunks, &r), Verdict::Nothing(_)));
        // a look that is not settled (yaw sent differs across a direction) is not guessed
        let mut r = on_floor("minecraft:torch", f + 1.0);
        r.yaw = 100.0;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::FacingYaw));
    }

    // LadderBlock#getStateForPlacement, #canSurvive
    #[test]
    fn ladders_face_away_from_the_first_sturdy_look_direction() {
        let (chunks, _keep) = world();
        let f = FLOOR as f64;
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::Stone, &[]);
        let r = side_click("minecraft:ladder", (2, FLOOR + 1, 1), Direction::South, f + 1.5);
        let s = place_at(&chunks, &r);
        assert_eq!(props_of(s, &["facing", "waterlogged"]), ["south", "false"]);
        // nothing to hang on beside a floor click far from that wall: fails on the client
        let r = req("minecraft:ladder", BlockPos::new(9, FLOOR, 9), Direction::Up, Vec3::new(9.5, f + 1.0, 9.5), &[]);
        assert!(matches!(predict_place(&chunks, &r), Verdict::Nothing(_)));
        // a ladder on a ladder facing the clicked face: null
        set(&chunks, 4, FLOOR + 1, 4, BlockKind::Ladder, &[("facing", "south")]);
        let r = side_click("minecraft:ladder", (4, FLOOR + 1, 4), Direction::South, f + 1.5);
        assert!(matches!(predict_place(&chunks, &r), Verdict::Nothing(_)));
        // water in the cell
        set(&chunks, 2, FLOOR + 1, 2, BlockKind::Water, &[("level", "0")]);
        let r = side_click("minecraft:ladder", (2, FLOOR + 1, 1), Direction::South, f + 1.5);
        assert_eq!(props_of(place_at(&chunks, &r), &["waterlogged"]), ["true"]);
    }

    // FenceBlock#getStateForPlacement, #connectsTo, FenceGateBlock.connectsToDirection
    #[test]
    fn fences_connect_to_sturdy_sides_same_fences_and_gates_across() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Stone, &[]);
        set(&chunks, 1, FLOOR + 1, 2, BlockKind::OakFence, &[]);
        set(&chunks, 2, FLOOR + 1, 3, BlockKind::NetherBrickFence, &[]);
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::OakFenceGate, &[("facing", "west")]);
        let s = place_at(&chunks, &on_floor("minecraft:oak_fence", h));
        // north (gate whose axis crosses), east (stone), south (a nether fence is not the same kind), west (oak fence)
        assert_eq!(props_of(s, &["north", "east", "south", "west"]), ["true", "true", "false", "true"]);
        // the gate facing north does not connect
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::OakFenceGate, &[("facing", "north")]);
        let s = place_at(&chunks, &on_floor("minecraft:oak_fence", h));
        // a gate across the line is not connected and a gate's support is a bar, never a full face
        assert_eq!(s.get_property("north"), Some("false"));
    }

    // IronBarsBlock#getStateForPlacement, #attachsTo, Block#isExceptionForConnection
    #[test]
    fn panes_attach_to_bars_and_walls_but_not_to_exceptions() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Stone, &[]);
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::IronBars, &[]);
        set(&chunks, 1, FLOOR + 1, 2, BlockKind::Pumpkin, &[]);
        set(&chunks, 2, FLOOR + 1, 3, BlockKind::CobblestoneWall, &[]);
        let s = place_at(&chunks, &on_floor("minecraft:glass_pane", h));
        assert_eq!(props_of(s, &["north", "east", "south", "west"]), ["true", "true", "true", "false"]);
    }

    // WallBlock#getStateForPlacement, #updateShape, #shouldRaisePost, #makeWallState
    #[test]
    fn walls_set_arms_by_the_block_above_and_the_post_by_the_arms() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        let names = ["up", "north", "east", "south", "west"];
        let s = place_at(&chunks, &on_floor("minecraft:cobblestone_wall", h));
        assert_eq!(props_of(s, &names), ["true", "none", "none", "none", "none"]);
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Stone, &[]);
        let s = place_at(&chunks, &on_floor("minecraft:cobblestone_wall", h));
        assert_eq!(props_of(s, &names), ["true", "none", "low", "none", "none"]);
        set(&chunks, 1, FLOOR + 1, 2, BlockKind::Stone, &[]);
        let s = place_at(&chunks, &on_floor("minecraft:cobblestone_wall", h));
        assert_eq!(props_of(s, &names), ["false", "none", "low", "none", "low"]);
        // a full block above makes both arms tall; two tall opposite arms keep the post down
        set(&chunks, 2, FLOOR + 2, 2, BlockKind::Stone, &[]);
        let s = place_at(&chunks, &on_floor("minecraft:cobblestone_wall", h));
        assert_eq!(props_of(s, &names), ["false", "none", "tall", "none", "tall"]);
        // a top slab above does not reach the cell's bottom edge: low arms (a bottom slab would)
        set(&chunks, 2, FLOOR + 2, 2, BlockKind::StoneSlab, &[("type", "top")]);
        let s = place_at(&chunks, &on_floor("minecraft:cobblestone_wall", h));
        assert_eq!(props_of(s, &names), ["false", "none", "low", "none", "low"]);
    }

    // FenceGateBlock#getStateForPlacement, SignalGetter#hasNeighborSignal
    #[test]
    fn fence_gates_face_the_player_and_see_walls_beside_and_unprovable_power_is_unknown() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        let s = place_at(&chunks, &on_floor("minecraft:oak_fence_gate", h));
        assert_eq!(props_of(s, &["facing", "open", "powered", "in_wall"]), ["south", "false", "false", "false"]);
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::CobblestoneWall, &[]);
        let s = place_at(&chunks, &on_floor("minecraft:oak_fence_gate", h));
        assert_eq!(s.get_property("in_wall"), Some("true"));
        // a redstone block beside the cell: the power is not provably absent
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::RedstoneBlock, &[]);
        assert_eq!(predict_place(&chunks, &on_floor("minecraft:oak_fence_gate", h)), Verdict::Unknown(UnknownWhy::Signal));
    }

    // TrapDoorBlock#getStateForPlacement
    #[test]
    fn trapdoors_take_face_half_and_water() {
        let (chunks, _keep) = world();
        let f = FLOOR as f64;
        let names = ["facing", "half", "open", "powered", "waterlogged"];
        let s = place_at(&chunks, &on_floor("minecraft:oak_trapdoor", f + 1.0));
        assert_eq!(props_of(s, &names), ["north", "bottom", "false", "false", "false"]);
        set(&chunks, 2, FLOOR + 1, 1, BlockKind::Stone, &[]);
        let r = side_click("minecraft:iron_trapdoor", (2, FLOOR + 1, 1), Direction::South, f + 1.7);
        assert_eq!(props_of(place_at(&chunks, &r), &names), ["south", "top", "false", "false", "false"]);
        let r = side_click("minecraft:iron_trapdoor", (2, FLOOR + 1, 1), Direction::South, f + 1.3);
        assert_eq!(props_of(place_at(&chunks, &r), &names), ["south", "bottom", "false", "false", "false"]);
        // placed against the underside of a block: top half
        set(&chunks, 6, FLOOR + 3, 6, BlockKind::Stone, &[]);
        let r = req("minecraft:oak_trapdoor", BlockPos::new(6, FLOOR + 3, 6), Direction::Down, Vec3::new(6.5, f + 3.0, 6.5), &[]);
        assert_eq!(props_of(place_at(&chunks, &r), &names), ["north", "top", "false", "false", "false"]);
        // a lever next to the cell can power it
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Lever, &[]);
        assert_eq!(predict_place(&chunks, &on_floor("minecraft:oak_trapdoor", f + 1.0)), Verdict::Unknown(UnknownWhy::Signal));
    }

    // LeavesBlock#getStateForPlacement, #updateDistance
    #[test]
    fn leaves_take_the_distance_from_logs_and_leaves() {
        let (chunks, _keep) = world();
        let h = FLOOR as f64 + 1.0;
        let names = ["distance", "persistent", "waterlogged"];
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:oak_leaves", h)), &names), ["7", "true", "false"]);
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::OakLeaves, &[("distance", "3")]);
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:oak_leaves", h)), &names), ["4", "true", "false"]);
        set(&chunks, 2, FLOOR + 2, 2, BlockKind::OakLog, &[]);
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:oak_leaves", h)), &names), ["1", "true", "false"]);
    }

    // ChestBlock#getStateForPlacement, #getChestType, #candidatePartnerFacing
    #[test]
    fn chests_join_a_single_chest_unless_sneaking_picks_the_clicked_side() {
        let (chunks, _keep) = world();
        let f = FLOOR as f64;
        let names = ["facing", "type", "waterlogged"];
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:chest", f + 1.0)), &names), ["north", "single", "false"]);
        // a chest facing the same way to the east: this one is its LEFT half (clockwise of north is east)
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Chest, &[("facing", "north"), ("type", "single")]);
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:chest", f + 1.0)), &names), ["north", "left", "false"]);
        // a trapped chest does not join a chest
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:trapped_chest", f + 1.0)), &names), ["north", "single", "false"]);
        // a chest to the west: RIGHT
        set(&chunks, 3, FLOOR + 1, 2, BlockKind::Air, &[]);
        set(&chunks, 1, FLOOR + 1, 2, BlockKind::Chest, &[("facing", "north"), ("type", "single")]);
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:chest", f + 1.0)), &names), ["north", "right", "false"]);
        // sneaking and clicking the east face of that west chest: the new chest copies its facing
        let mut r = side_click("minecraft:chest", (1, FLOOR + 1, 2), Direction::East, f + 1.5);
        r.secondary_use = Some(true);
        assert_eq!(props_of(place_at(&chunks, &r), &names), ["north", "right", "false"]);
        r.secondary_use = None;
        assert_eq!(predict_place(&chunks, &r), Verdict::Unknown(UnknownWhy::SecondaryUse));
    }

    // BaseRailBlock#getStateForPlacement, #canSurvive (canSupportRigidBlock)
    #[test]
    fn plain_rails_run_along_the_look_and_need_a_rigid_support() {
        let (chunks, _keep) = world();
        let f = FLOOR as f64;
        let names = ["shape", "waterlogged"];
        assert_eq!(props_of(place_at(&chunks, &on_floor("minecraft:rail", f + 1.0)), &names), ["north_south", "false"]);
        let mut r = on_floor("minecraft:rail", f + 1.0);
        r.yaw = 90.0;
        r.sent_yaw = 90.0;
        assert_eq!(props_of(place_at(&chunks, &r), &names), ["east_west", "false"]);
        // a fence post top is not a rigid support, a bottom slab neither
        set(&chunks, 6, FLOOR + 1, 6, BlockKind::OakFence, &[]);
        let r = req("minecraft:rail", BlockPos::new(6, FLOOR + 1, 6), Direction::Up, Vec3::new(6.5, f + 2.5, 6.5), &[]);
        assert!(matches!(predict_place(&chunks, &r), Verdict::Nothing(_)));
        // a powered rail runs redstone code on the client: not predicted
        assert_eq!(predict_place(&chunks, &on_floor("minecraft:powered_rail", f + 1.0)), Verdict::Unknown(UnknownWhy::Family));
    }
}
