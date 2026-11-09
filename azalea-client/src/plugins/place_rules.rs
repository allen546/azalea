//! Placement rules that read neighbours, the look or redstone (Null fork patch 71).
//!
//! Each rule follows one vanilla `getStateForPlacement` (Mojang names are in the comments). A rule
//! returns the state the client writes, a client FAIL (`Verdict::Nothing`), or `Unknown` when it
//! would have to guess: a support shape the fork does not model, redstone power it cannot rule out,
//! or a look that differs from the one the server knows. A wrong prediction is worse than none.
//!
//! Grim (`BlockPlace`, `CheckManagerListener`) works out the same state itself; a different
//! state near the bot costs `Simulation`.
//!
//! Spec: specs/B6-placement-breaking.md §4.10 (place prediction, patch 71)

use azalea_block::{
    BlockState, PlaceFamily, gives_no_direct_signal, gives_no_signal, torch_wall_block,
};
use azalea_core::{direction::Direction, position::BlockPos};
use azalea_physics::collision::{
    FaceRegion, SupportType, collision_face_covers, is_face_sturdy, is_face_supported,
};
use azalea_registry::{builtin::BlockKind, tags};
use azalea_world::BlockView;

use super::place_prediction::{
    PlaceRequest, UnknownWhy, axis_name, clockwise, counter_clockwise, direction_name,
    horizontal_direction, is_water_source, with_properties,
};

/// What a rule says.
pub enum Rule {
    State(BlockState),
    /// The client's own try fails (`getStateForPlacement` null or `canSurvive` false): the packet
    /// goes out and nothing is predicted.
    Fail(&'static str),
    Unknown(UnknownWhy),
}

/// What a rule can read.
pub struct Cx<'a, V: BlockView> {
    pub chunks: &'a V,
    pub req: &'a PlaceRequest<'a>,
    pub kind: BlockKind,
    /// `BlockPlaceContext#getClickedPos`: the cell the block goes into.
    pub target: BlockPos,
    /// The state in that cell now.
    pub before: BlockState,
    /// `BlockPlaceContext#replacingClickedOnBlock`.
    pub replace_clicked: bool,
}

type Res<T> = Result<T, Rule>;

fn unknown<T>(why: UnknownWhy) -> Res<T> {
    Err(Rule::Unknown(why))
}

impl<V: BlockView> Cx<'_, V> {
    fn state(&self, pos: BlockPos) -> Res<BlockState> {
        match self.chunks.get_block_state(pos) {
            Some(s) => Ok(s),
            None => unknown(UnknownWhy::Unloaded),
        }
    }

    fn water(&self) -> &'static str {
        if is_water_source(self.before) { "true" } else { "false" }
    }

    /// `isFaceSturdy(FULL)` of the block at `pos` on `face`.
    fn sturdy(&self, pos: BlockPos, face: Direction) -> Res<bool> {
        match is_face_sturdy(self.state(pos)?, face) {
            Some(b) => Ok(b),
            None => unknown(UnknownWhy::Support),
        }
    }

    /// `Entity#getDirection` for the server: the yaw it knows must give the same direction.
    fn horizontal(&self) -> Res<Direction> {
        let now = horizontal_direction(self.req.yaw);
        if now != horizontal_direction(self.req.sent_yaw) {
            return unknown(UnknownWhy::FacingYaw);
        }
        Ok(now)
    }

    /// `BlockPlaceContext#getNearestLookingDirections`.
    fn looking(&self) -> Res<[Direction; 6]> {
        let (Some(now), Some(sent)) = (
            ordered_by_nearest(self.req.yaw, self.req.pitch),
            ordered_by_nearest(self.req.sent_yaw, self.req.sent_pitch),
        ) else {
            return unknown(UnknownWhy::FacingYaw);
        };
        if now != sent {
            return unknown(UnknownWhy::FacingYaw);
        }
        let mut dirs = now;
        if !self.replace_clicked {
            // The face we clicked, turned round, goes first.
            let first = self.req.face.opposite();
            if let Some(i) = dirs.iter().position(|d| *d == first) {
                dirs[..=i].rotate_right(1);
            }
        }
        Ok(dirs)
    }

    /// `SignalGetter#hasNeighborSignal` for the target cell, or unknown. A neighbour is trusted
    /// to give no signal only when its class never outputs one, and a touching block only when
    /// none of its own neighbours (this cell included) can power it directly.
    fn no_neighbor_signal(&self) -> Res<()> {
        for d in ALL {
            let n = self.target.offset_with_direction(d);
            if !gives_no_signal(BlockKind::from(self.state(n)?)) {
                return unknown(UnknownWhy::Signal);
            }
            for d2 in ALL {
                let m = n.offset_with_direction(d2);
                let state = if m == self.target { self.before } else { self.state(m)? };
                if !gives_no_direct_signal(BlockKind::from(state)) {
                    return unknown(UnknownWhy::Signal);
                }
            }
        }
        Ok(())
    }
}

const ALL: [Direction; 6] = [
    Direction::Down,
    Direction::Up,
    Direction::North,
    Direction::South,
    Direction::West,
    Direction::East,
];

const SIDES: [Direction; 4] = [Direction::North, Direction::East, Direction::South, Direction::West];

/// `Direction#orderedByNearest`: all six directions, nearest to the look first. `None` when two
/// compared lengths are within a hair of each other: vanilla reads its sine table there.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 71: torches and ladders)
pub fn ordered_by_nearest(yaw: f32, pitch: f32) -> Option<[Direction; 6]> {
    const TIE: f64 = 1e-3;
    let (f, g) = ((pitch as f64).to_radians(), -(yaw as f64).to_radians());
    let (h, i, j, k) = (f.sin(), f.cos(), g.sin(), g.cos());
    let (l, m, n) = (j.abs(), h.abs(), k.abs());
    let (o, p) = (l * i, n * i);
    // Both exactly zero (yaw 0 and pitch 0) is no tie: the table gives exact zeros there.
    let near = |a: f64, b: f64| (a - b).abs() < TIE && !(a == 0.0 && b == 0.0);
    if near(l, n) || near(m, o) || near(m, p) {
        return None;
    }
    let x = if j > 0.0 { Direction::East } else { Direction::West };
    let y = if h < 0.0 { Direction::Up } else { Direction::Down };
    let z = if k > 0.0 { Direction::South } else { Direction::North };
    let (a, b, c) = if l > n {
        if m > o {
            (y, x, z)
        } else if p > m {
            (x, z, y)
        } else {
            (x, y, z)
        }
    } else if m > p {
        (y, z, x)
    } else if o > m {
        (z, x, y)
    } else {
        (z, y, x)
    };
    Some([a, b, c, c.opposite(), b.opposite(), a.opposite()])
}

/// The state this family's item places, as the vanilla client works it out.
// Spec: specs/B6-placement-breaking.md §4.10 (patch 71)
pub fn state_for<V: BlockView>(cx: &Cx<V>, family: PlaceFamily) -> Rule {
    let res = match family {
        PlaceFamily::Torch => torch(cx),
        PlaceFamily::Ladder => ladder(cx),
        PlaceFamily::Fence => fence(cx),
        PlaceFamily::Pane => pane(cx),
        PlaceFamily::Wall => wall(cx),
        PlaceFamily::FenceGate => fence_gate(cx),
        PlaceFamily::Trapdoor => trapdoor(cx),
        PlaceFamily::Leaves => leaves(cx),
        PlaceFamily::Chest => chest(cx),
        PlaceFamily::Rail => rail(cx),
        _ => unknown(UnknownWhy::Family),
    };
    match res {
        Ok(Some(s)) => Rule::State(s),
        Ok(None) => Rule::Fail("the block cannot be placed or survive here (getStateForPlacement)"),
        Err(rule) => rule,
    }
}

fn props(cx: &Cx<impl BlockView>, kind: BlockKind, p: &[(&str, &str)]) -> Res<Option<BlockState>> {
    let _ = cx;
    match with_properties(kind, p) {
        Some(s) => Ok(Some(s)),
        None => unknown(UnknownWhy::Family),
    }
}

// --- torches (StandingAndWallBlockItem#getPlacementState, WallTorchBlock#getStateForPlacement) ---

/// A torch's survival: `BaseTorchBlock#canSurvive` wants a centre support below.
fn standing_torch_survives<V: BlockView>(cx: &Cx<V>) -> Res<bool> {
    let below = cx.state(cx.target.down(1))?;
    match is_face_supported(below, Direction::Up, SupportType::Center) {
        Some(b) => Ok(b),
        None => unknown(UnknownWhy::Support),
    }
}

fn torch<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let Some(wall) = torch_wall_block(cx.kind) else { return unknown(UnknownWhy::Family) };
    let dirs = cx.looking()?;
    // WallTorchBlock#getStateForPlacement: the first horizontal look direction whose far side is
    // sturdy, facing back toward the player.
    let wall_state = {
        let mut found = None;
        for d in dirs {
            if !matches!(d, Direction::North | Direction::South | Direction::East | Direction::West) {
                continue;
            }
            // canSurvive(facing = d.opposite): the block at target + d, sturdy on face d.opposite
            if cx.sturdy(cx.target.offset_with_direction(d), d.opposite())? {
                found = Some(d.opposite());
                break;
            }
        }
        found
    };
    for d in dirs {
        if d == Direction::Up {
            continue; // attachmentDirection.getOpposite()
        }
        if d == Direction::Down {
            if standing_torch_survives(cx)? {
                return Ok(Some(cx_default(cx.kind)));
            }
        } else if let Some(facing) = wall_state {
            return props(cx, wall, &[("facing", direction_name(facing))]);
        }
    }
    Ok(None)
}

fn cx_default(kind: BlockKind) -> BlockState {
    BlockState::from(kind)
}

// --- ladders (LadderBlock#getStateForPlacement) ---

fn ladder<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    if !cx.replace_clicked {
        // a ladder on the clicked block facing the clicked face: null
        let behind = cx.state(cx.req.clicked)?;
        if BlockKind::from(behind) == cx.kind && behind.get_property("facing") == Some(direction_name(cx.req.face)) {
            return Ok(None);
        }
    }
    for d in cx.looking()? {
        if !SIDES.contains(&d) {
            continue;
        }
        // facing d.opposite; canAttachTo(the block at target + d, face d.opposite)
        if cx.sturdy(cx.target.offset_with_direction(d), d.opposite())? {
            return props(cx, cx.kind, &[("facing", direction_name(d.opposite())), ("waterlogged", cx.water())]);
        }
    }
    Ok(None)
}

// --- fences, panes, walls: connections ---

/// `Block#isExceptionForConnection`.
fn connection_exception(kind: BlockKind) -> bool {
    kind.to_str().ends_with("_leaves")
        || matches!(
            kind,
            BlockKind::Barrier
                | BlockKind::CarvedPumpkin
                | BlockKind::JackOLantern
                | BlockKind::Melon
                | BlockKind::Pumpkin
        )
        || tags::blocks::SHULKER_BOXES.contains(&kind)
}

fn is_gate(kind: BlockKind) -> bool {
    kind.to_str().ends_with("_fence_gate")
}

/// `FenceGateBlock.connectsToDirection`: the gate's facing axis is the one across `direction`.
fn gate_connects(gate: BlockState, direction: Direction) -> bool {
    let facing = gate.get_property("facing").and_then(super::place_prediction::direction_from_name);
    let across = clockwise(direction);
    matches!((facing, across), (Some(f), a) if axis_name(f) == axis_name(a))
}

/// `IronBarsBlock` (glass panes, iron bars, copper bars).
fn is_bars(kind: BlockKind) -> bool {
    let n = kind.to_str();
    n.ends_with("glass_pane") || n.ends_with("iron_bars") || n.ends_with("copper_bars")
}

/// The neighbour on `side` of the target, its state, and the face of it that touches us.
fn side_neighbour<V: BlockView>(cx: &Cx<V>, side: Direction) -> Res<(BlockPos, BlockState)> {
    let pos = cx.target.offset_with_direction(side);
    Ok((pos, cx.state(pos)?))
}

fn fence<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let own_wooden = tags::blocks::WOODEN_FENCES.contains(&cx.kind);
    let mut values = [("", "false"); 4];
    for (i, side) in SIDES.into_iter().enumerate() {
        let (pos, st) = side_neighbour(cx, side)?;
        let kind = BlockKind::from(st);
        // FenceBlock#connectsTo(state, sturdy, direction = the face that looks back at us)
        let back = side.opposite();
        let same = tags::blocks::FENCES.contains(&kind) && tags::blocks::WOODEN_FENCES.contains(&kind) == own_wooden;
        let gate = is_gate(kind) && gate_connects(st, back);
        let connects = if same || gate {
            true
        } else if connection_exception(kind) {
            false
        } else {
            cx.sturdy(pos, back)?
        };
        values[i] = (direction_name(side), if connects { "true" } else { "false" });
    }
    props(cx, cx.kind, &[values[0], values[1], values[2], values[3], ("waterlogged", cx.water())])
}

fn pane<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let mut values = [("", "false"); 4];
    for (i, side) in SIDES.into_iter().enumerate() {
        let (pos, st) = side_neighbour(cx, side)?;
        let kind = BlockKind::from(st);
        let back = side.opposite();
        // IronBarsBlock#attachsTo
        let attaches = if is_bars(kind) || tags::blocks::WALLS.contains(&kind) {
            true
        } else if connection_exception(kind) {
            false
        } else {
            cx.sturdy(pos, back)?
        };
        values[i] = (direction_name(side), if attaches { "true" } else { "false" });
    }
    props(cx, cx.kind, &[values[0], values[1], values[2], values[3], ("waterlogged", cx.water())])
}

// --- walls (WallBlock#getStateForPlacement, #updateShape, #shouldRaisePost) ---

#[derive(Clone, Copy, Eq, PartialEq)]
enum Arm {
    None,
    Low,
    Tall,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Tall => "tall",
        }
    }
}

const SIXTEENTH: f64 = 1.0 / 16.0;

/// The test shape of one arm seen from below: a 2 wide strip from the middle to the edge, in the
/// plane of the face (x then z).
fn arm_region(side: Direction) -> FaceRegion {
    let (m0, m1) = (7.0 * SIXTEENTH, 9.0 * SIXTEENTH);
    match side {
        Direction::North => (m0, m1, 0.0, m1),
        Direction::South => (m0, m1, m0, 1.0),
        Direction::West => (0.0, m1, m0, m1),
        _ => (m0, 1.0, m0, m1),
    }
}

fn wall<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let mut connected = [false; 4];
    for (i, side) in SIDES.into_iter().enumerate() {
        let (pos, st) = side_neighbour(cx, side)?;
        let kind = BlockKind::from(st);
        let back = side.opposite();
        // WallBlock#connectsTo
        connected[i] = if tags::blocks::WALLS.contains(&kind) || is_bars(kind) {
            true
        } else if is_gate(kind) && gate_connects(st, back) {
            true
        } else if connection_exception(kind) {
            false
        } else {
            cx.sturdy(pos, back)?
        };
    }
    let above = cx.state(cx.target.up(1))?;
    let mut arms = [Arm::None; 4];
    for (i, side) in SIDES.into_iter().enumerate() {
        // makeWallState: tall when the face of the block above covers the arm's test shape
        arms[i] = if !connected[i] {
            Arm::None
        } else if collision_face_covers(above, Direction::Down, arm_region(side)) {
            Arm::Tall
        } else {
            Arm::Low
        };
    }
    // shouldRaisePost
    let [n, e, s, w] = arms;
    let above_kind = BlockKind::from(above);
    let up = if tags::blocks::WALLS.contains(&above_kind) && above.get_property("up") == Some("true") {
        true
    } else {
        let (sn, ss, se, sw) = (n == Arm::None, s == Arm::None, e == Arm::None, w == Arm::None);
        if (sn && ss && sw && se) || sn != ss || sw != se {
            true
        } else if (n == Arm::Tall && s == Arm::Tall) || (e == Arm::Tall && w == Arm::Tall) {
            false
        } else {
            let m = (7.0 * SIXTEENTH, 9.0 * SIXTEENTH, 7.0 * SIXTEENTH, 9.0 * SIXTEENTH);
            tags::blocks::WALL_POST_OVERRIDE.contains(&above_kind)
                || collision_face_covers(above, Direction::Down, m)
        }
    };
    props(
        cx,
        cx.kind,
        &[
            ("up", if up { "true" } else { "false" }),
            ("north", n.name()),
            ("east", e.name()),
            ("south", s.name()),
            ("west", w.name()),
            ("waterlogged", cx.water()),
        ],
    )
}

// --- fence gates, trapdoors (hasNeighborSignal) ---

fn fence_gate<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    cx.no_neighbor_signal()?; // powered = open = false
    let dir = cx.horizontal()?;
    let is_wall = |side: Direction| -> Res<bool> {
        Ok(tags::blocks::WALLS.contains(&BlockKind::from(cx.state(cx.target.offset_with_direction(side))?)))
    };
    // axis Z (facing north or south): walls to the west or east; axis X: to the north or south
    let in_wall = if axis_name(dir) == "z" {
        is_wall(Direction::West)? || is_wall(Direction::East)?
    } else {
        is_wall(Direction::North)? || is_wall(Direction::South)?
    };
    props(
        cx,
        cx.kind,
        &[
            ("facing", direction_name(dir)),
            ("open", "false"),
            ("powered", "false"),
            ("in_wall", if in_wall { "true" } else { "false" }),
        ],
    )
}

fn trapdoor<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    cx.no_neighbor_signal()?;
    let face = cx.req.face;
    let (facing, half) = if !cx.replace_clicked && axis_name(face) != "y" {
        let top = cx.req.hit.y - cx.target.y as f64 > 0.5;
        (face, if top { "top" } else { "bottom" })
    } else {
        (cx.horizontal()?.opposite(), if face == Direction::Up { "bottom" } else { "top" })
    };
    props(
        cx,
        cx.kind,
        &[
            ("facing", direction_name(facing)),
            ("half", half),
            ("open", "false"),
            ("powered", "false"),
            ("waterlogged", cx.water()),
        ],
    )
}

// --- leaves (LeavesBlock#getStateForPlacement, #updateDistance) ---

fn leaves<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let mut distance = 7u8;
    for d in ALL {
        let st = cx.state(cx.target.offset_with_direction(d))?;
        let at = if tags::blocks::LOGS.contains(&BlockKind::from(st)) {
            0
        } else {
            st.get_property("distance").and_then(|v| v.parse::<u8>().ok()).unwrap_or(7)
        };
        distance = distance.min(at + 1);
        if distance == 1 {
            break;
        }
    }
    let distance = distance.to_string();
    props(cx, cx.kind, &[("persistent", "true"), ("waterlogged", cx.water()), ("distance", &distance)])
}

// --- chests (ChestBlock#getStateForPlacement) ---

fn chest<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let Some(sneaking) = cx.req.secondary_use else { return unknown(UnknownWhy::SecondaryUse) };
    let mut facing = cx.horizontal()?.opposite();
    // candidatePartnerFacing: a single chest of the same block next to `pos` on `side`
    let partner = |pos: BlockPos, side: Direction| -> Res<Option<Direction>> {
        let st = cx.state(pos.offset_with_direction(side))?;
        if BlockKind::from(st) == cx.kind && st.get_property("type") == Some("single") {
            Ok(st.get_property("facing").and_then(super::place_prediction::direction_from_name))
        } else {
            Ok(None)
        }
    };
    let mut ty = "single";
    let face = cx.req.face;
    if sneaking && axis_name(face) != "y" {
        if let Some(d3) = partner(cx.target, face.opposite())? {
            if axis_name(d3) != axis_name(face) {
                facing = d3;
                ty = if counter_clockwise(d3) == face.opposite() { "right" } else { "left" };
            }
        }
    }
    if ty == "single" && !sneaking {
        // getChestType
        if partner(cx.target, clockwise(facing))? == Some(facing) {
            ty = "left";
        } else if partner(cx.target, counter_clockwise(facing))? == Some(facing) {
            ty = "right";
        }
    }
    props(cx, cx.kind, &[("facing", direction_name(facing)), ("type", ty), ("waterlogged", cx.water())])
}

// --- plain rails (BaseRailBlock#getStateForPlacement, #canSurvive) ---

fn rail<V: BlockView>(cx: &Cx<V>) -> Res<Option<BlockState>> {
    let below = cx.state(cx.target.down(1))?;
    // canSupportRigidBlock
    match is_face_supported(below, Direction::Up, SupportType::Rigid) {
        None => return unknown(UnknownWhy::Support),
        Some(false) => return Ok(None),
        Some(true) => {}
    }
    let dir = cx.horizontal()?;
    let shape = if axis_name(dir) == "x" { "east_west" } else { "north_south" };
    props(cx, cx.kind, &[("shape", shape), ("waterlogged", cx.water())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_order_follows_the_look() {
        // yaw 0 faces south, level: south first, then up or down by pitch sign
        let o = ordered_by_nearest(0.0, 10.0).unwrap();
        assert_eq!(o[0], Direction::South);
        assert_eq!(o[5], Direction::North);
        // looking straight down: down first
        assert_eq!(ordered_by_nearest(0.0, 80.0).unwrap()[0], Direction::Down);
        assert_eq!(ordered_by_nearest(0.0, -80.0).unwrap()[0], Direction::Up);
    }
}
