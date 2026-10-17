//! Place or break a block and learn whether the server accepted it.
//!
//! Vanilla answers a placement or a finished break with block updates and a
//! [`ClientboundBlockChangedAck`] for the sequence number we sent. After the ack the world holds the
//! server's truth: if the target block changed, the action was accepted, otherwise it was refused
//! (no support, an entity in the way). Before anything is sent, the click is checked as a vanilla
//! client would have to make it: the block is within reach, the face is visible from the eye, and
//! the crosshair ray hits that face. If not, the action is refused on the client, with a
//! [`BlockActionRefusal`], and no packet goes out (anticheats flag such clicks). The ray and the
//! visibility test use the block's real outline shape (a carpet, a chain, a stair), not the unit
//! cube; only the reach figure is the cube, because the server and Grim measure it that way.
//! Aim at a visible point of the real shape with [`LookAtBlockQueued`]. Place with [`PlaceBlockQueued`]; start a break
//! with [`BreakBlockQueued`] (or [`StartMiningBlockEvent`], a tick later) and abort it with
//! [`StopMiningQueued`]; read both results from [`BlockActionResults`].
//!
//! [`StartMiningBlockEvent`]: crate::mining::StartMiningBlockEvent
//!
//! [`ClientboundBlockChangedAck`]: azalea_protocol::packets::game::ClientboundBlockChangedAck

use azalea_block::BlockState;
use azalea_core::{
    direction::Direction,
    hit_result::{BlockHitResult, HitResult},
    position::{BlockPos, Vec3},
    tick::GameTick,
};
use azalea_entity::{
    Attributes, LookDirection, Position, dimensions::EntityDimensions, inventory::Inventory,
};
use azalea_physics::{PhysicsSystems, collision::{BlockWithShape, VoxelShape}};
use azalea_protocol::packets::game::{
    ServerboundPlayerAction,
    s_interact::InteractionHand,
    s_player_action::Action,
    s_use_item_on::ServerboundUseItemOn,
};
use azalea_registry::{builtin::BlockKind, tags};
use azalea_world::ChunkStorage;
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::{
    interact::{
        BlockStatePredictionHandler, SwingArmEvent, check_is_interaction_restricted,
        handle_start_use_item_queued,
        pick::{HitResultComponent, pick_block},
    },
    inventory::ensure_has_sent_carried_item,
    local_player::{LocalGameMode, WorldHolder},
    mining::{
        BlockBreakSent, MineDelay, MineProgress, Mining, MiningQueued, MiningSystems,
        SkipStartTickProgress,
    },
    packet::game::SendGamePacketEvent,
};

/// Without an ack this many ticks after the action was sent, it is judged on the block alone.
const ACK_TIMEOUT_TICKS: u32 = 60;

// Spec: specs/B6-placement-breaking.md §4.6 (acknowledgement and readback of block actions)
pub struct BlockActionsPlugin;
impl Plugin for BlockActionsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BlockActionFinished>()
            .add_systems(GameTick, handle_look_at_block_queued.before(handle_break_block_queued))
            .add_systems(
                GameTick,
                (handle_break_block_queued, begin_break_result, handle_stop_mining_queued)
                    .chain()
                    .before(MiningSystems),
            )
            .add_systems(
                GameTick,
                (handle_place_block_queued, settle_block_actions)
                    .chain()
                    .after(MiningSystems)
                    .after(ensure_has_sent_carried_item)
                    .before(handle_start_use_item_queued)
                    .before(PhysicsSystems),
            )
            .add_observer(record_break_sent);
    }
}

// Spec: specs/B6-placement-breaking.md §4.6 (acknowledgement and readback)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockActionOutcome {
    /// Sent; the server's answer has not been read yet.
    Pending,
    /// The server acknowledged it and the block is as intended.
    Accepted,
    /// The client's own checks refused it (nothing sent, see `refusal`), or the server acknowledged
    /// it and the block is unchanged, or no answer came in time (`refusal` is `NoAck`).
    Refused,
    /// A break that was aborted: by [`StopMiningQueued`], or mining ended without finishing.
    Cancelled,
}

// Spec: specs/B6-placement-breaking.md §4.1 (reach and visibility checks before a click)
/// Why a click was refused on the client, before any packet was sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockActionRefusal {
    OutOfReach,
    /// The eye is not on the outer side of the face, so no ray can hit it.
    FaceNotVisible,
    /// The crosshair does not hit that block (and face, for a placement): look at it first.
    NotLookingAtIt,
    /// Breaking is not allowed here (adventure mode without CanDestroy, spectator).
    NotAllowed,
    /// Sent, but no acknowledgement came within [`ACK_TIMEOUT_TICKS`] and the block did not change.
    NoAck,
}

impl BlockActionRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutOfReach => "out of reach",
            Self::FaceNotVisible => "face not visible from the eye",
            Self::NotLookingAtIt => "crosshair does not hit it",
            Self::NotAllowed => "breaking not allowed in this game mode",
            Self::NoAck => "no ack",
        }
    }
}

/// The state of one placement or break, as of the last `GameTick`.
// Spec: specs/B6-placement-breaking.md §4.6 (acknowledgement and readback)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockActionResult {
    /// Counts up per client, so a caller can tell a new action from the previous one.
    pub id: u32,
    /// The block that changes: for a placement the clicked block's neighbour, for a break the
    /// block being mined.
    pub position: BlockPos,
    pub outcome: BlockActionOutcome,
    /// Why a `Refused` action was refused: the client's own checks (nothing was sent), or `NoAck`.
    pub refusal: Option<BlockActionRefusal>,
    /// Sequence number of the packet whose ack settles the action; for a break, `None` until the
    /// finishing packet is sent.
    pub seq: Option<u32>,
    /// Break only: ticks from the start to the tick the finishing packet was sent.
    pub finish_ticks: Option<u32>,
    /// Ticks from sending (for a break, from the finishing packet) to the tick the ack was read;
    /// `None` while unacknowledged.
    pub ack_ticks: Option<u32>,
    /// Ticks since it was sent (for a break: since mining started, then since the finishing
    /// packet); stops counting when settled.
    pub elapsed_ticks: u32,
    /// The target block's state when we clicked.
    before: BlockState,
}

// Spec: specs/B6-placement-breaking.md §4.6 (acknowledgement and readback)
/// The latest placement and break results of a client.
#[derive(Clone, Component, Debug, Default)]
pub struct BlockActionResults {
    pub last_place: Option<BlockActionResult>,
    pub last_break: Option<BlockActionResult>,
    next_id: u32,
}

// Spec: specs/B6-placement-breaking.md §4.6 (acknowledgement and readback)
/// Sent once when a placement or break settles.
#[derive(Debug, Message)]
pub struct BlockActionFinished {
    pub entity: Entity,
    pub is_break: bool,
    pub result: BlockActionResult,
}

// Spec: specs/B6-placement-breaking.md §4.4 (start a break in the same tick)
/// Start breaking the block at `position` in this `GameTick`. Unlike [`StartMiningBlockEvent`]
/// (an `Update` message), it reaches the server in the tick it was inserted.
///
/// [`StartMiningBlockEvent`]: crate::mining::StartMiningBlockEvent
#[derive(Component, Debug)]
pub struct BreakBlockQueued {
    pub position: BlockPos,
}

// Spec: specs/B6-placement-breaking.md §4.4 (break timing; face from the crosshair)
/// Turns [`BreakBlockQueued`] into forced mining, with the face from the crosshair when it is on
/// that block (as `handle_start_mining_block_event` does); [`begin_break_result`] then checks it.
pub fn handle_break_block_queued(
    mut commands: Commands,
    query: Query<(Entity, &BreakBlockQueued, &HitResultComponent, &MineDelay, Option<&Mining>)>,
) {
    for (entity, queued, hit_result, delay, mining) in &query {
        // Already breaking it: a repeated command must not restart it (nor skip another tick).
        if mining.is_some_and(|m| m.pos == queued.position) {
            commands.entity(entity).remove::<BreakBlockQueued>();
            continue;
        }
        // Vanilla's held-button mining waits out `destroyDelay` (5 ticks) after a finished break,
        // so the next start comes 6 ticks (300 ms) later; Grim's FastBreak expects that gap.
        if **delay > 0 {
            continue;
        }
        let direction = hit_result
            .as_block_hit_result_if_not_miss()
            .filter(|h| h.block_pos == queued.position)
            .map_or(Direction::Down, |h| h.direction);
        commands
            .entity(entity)
            .remove::<BreakBlockQueued>()
            .insert((
                MiningQueued { position: queued.position, direction, force: true },
                CheckBreakClick,
                SkipStartTickProgress,
            ));
    }
}

// Spec: specs/B6-placement-breaking.md §4.1 (only our own breaks get the vanilla-click check)
/// Marks mining queued by [`BreakBlockQueued`], so only those breaks get the vanilla-click check;
/// `StartMiningBlockEvent { force: true }` keeps its "mine regardless" contract.
#[derive(Component, Debug)]
pub struct CheckBreakClick;

// Spec: specs/B6-placement-breaking.md §4.4 (abort a break)
/// Abort the break in progress on the next `GameTick`.
#[derive(Component, Debug, Default)]
pub struct StopMiningQueued;

// Spec: specs/B6-placement-breaking.md §4.2 (place on a chosen face)
/// Right-click the `direction` face of the block at `position` with the held item on the next
/// `GameTick`; the result appears in [`BlockActionResults::last_place`].
///
/// The hit point comes from the crosshair raycast if it is on that face, else it is the face centre.
/// The bot should look at the face first so the server's reach and line checks agree.
#[derive(Component, Debug)]
pub struct PlaceBlockQueued {
    pub position: BlockPos,
    pub direction: Direction,
}

// Spec: specs/B6-placement-breaking.md §4.2 (click choice: aim at a visible point of the real shape)
/// Turn to a point where the crosshair hits the block at `position` (on `direction`, or on any
/// face when `None`) on the next `GameTick`. Does nothing if no such point is visible from here;
/// the click that follows is then refused as [`BlockActionRefusal::NotLookingAtIt`]. Allow two
/// ticks for the look to land before clicking.
#[derive(Component, Debug)]
pub struct LookAtBlockQueued {
    pub position: BlockPos,
    pub direction: Option<Direction>,
}

// Spec: specs/B6-placement-breaking.md §4.2 (click choice: aim at a visible point of the real shape)
pub fn handle_look_at_block_queued(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &LookAtBlockQueued,
        &mut LookDirection,
        &WorldHolder,
        &Position,
        &EntityDimensions,
        &Attributes,
    )>,
) {
    for (entity, queued, mut look, world, position, dims, attrs) in &mut query {
        commands.entity(entity).remove::<LookAtBlockQueued>();
        let eye = position.up(dims.eye_height.into());
        let range = attrs.block_interaction_range.calculate();
        let world = world.shared.read();
        if let Some(aim) = aim_at_block(&world.chunks, eye, range, *look, queued.position, queued.direction) {
            look.update(aim);
        }
    }
}

// Spec: specs/B6-placement-breaking.md §4.2 (click choice: aim at a visible point of the real shape)
/// The look (to pass to `LookDirection::update` from `current`) whose crosshair ray hits the block at `pos`, on `face` if given. Tries
/// points of each outline box's faces (middle first, kept just inside the box so rounding cannot
/// slip past it) and keeps the first whose real ray, interaction shape included, lands there.
pub fn aim_at_block(
    chunks: &ChunkStorage,
    eye: Vec3,
    range: f64,
    current: LookDirection,
    pos: BlockPos,
    face: Option<Direction>,
) -> Option<LookDirection> {
    let state = chunks.get_block_state(pos)?;
    let boxes = state.outline_shape().to_aabbs();
    let faces = face.map_or(
        vec![
            Direction::Up,
            Direction::North,
            Direction::South,
            Direction::East,
            Direction::West,
            Direction::Down,
        ],
        |f| vec![f],
    );
    // (u, v) fractions across the face rectangle
    const SPOTS: [(f64, f64); 5] = [(0.5, 0.5), (0.3, 0.3), (0.7, 0.3), (0.3, 0.7), (0.7, 0.7)];
    let at = |a0: f64, a1: f64, t: f64| a0 + (a1 - a0) * t;
    // just inside the box, so rounding cannot slip past the face
    let inset = |a0: f64, a1: f64| ((a1 - a0) * 0.25).min(0.005);
    for (u, v) in SPOTS {
        for f in &faces {
            for b in &boxes {
                let (lo, hi) = (b.min, b.max);
                let p = match f {
                    Direction::Up => Vec3::new(at(lo.x, hi.x, u), hi.y - inset(lo.y, hi.y), at(lo.z, hi.z, v)),
                    Direction::Down => Vec3::new(at(lo.x, hi.x, u), lo.y + inset(lo.y, hi.y), at(lo.z, hi.z, v)),
                    Direction::North => Vec3::new(at(lo.x, hi.x, u), at(lo.y, hi.y, v), lo.z + inset(lo.z, hi.z)),
                    Direction::South => Vec3::new(at(lo.x, hi.x, u), at(lo.y, hi.y, v), hi.z - inset(lo.z, hi.z)),
                    Direction::West => Vec3::new(lo.x + inset(lo.x, hi.x), at(lo.y, hi.y, v), at(lo.z, hi.z, u)),
                    Direction::East => Vec3::new(hi.x - inset(lo.x, hi.x), at(lo.y, hi.y, v), at(lo.z, hi.z, u)),
                };
                let to = Vec3::new(pos.x as f64 + p.x, pos.y as f64 + p.y, pos.z as f64 + p.z);
                let d = to - eye;
                let flat = (d.x * d.x + d.z * d.z).sqrt();
                let look = LookDirection::new(
                    (-d.x).atan2(d.z).to_degrees() as f32,
                    (-d.y).atan2(flat).to_degrees() as f32,
                );
                // a look update snaps to 0.15 degree steps from where we face now; test that one
                let mut landed = current;
                landed.update(look);
                let hit = pick_block(landed, eye, chunks, range);
                if !hit.miss && hit.block_pos == pos && face.is_none_or(|f| hit.direction == f) {
                    return Some(look);
                }
            }
        }
    }
    None
}

/// Distance from the eye to the nearest point of the block's unit cube. Not the real shape: the
/// server (`Player.isWithinBlockInteractionRange`) and Grim (`FarPlace`, `FarBreak`) both measure
/// to the cube. The crosshair ray, which is shape-exact, already limits the hit point to the range.
fn distance_to_block(eye: Vec3, pos: BlockPos) -> f64 {
    let d = |e: f64, lo: i32| (lo as f64 - e).max(e - (lo as f64 + 1.0)).max(0.0);
    let (dx, dy, dz) = (d(eye.x, pos.x), d(eye.y, pos.y), d(eye.z, pos.z));
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Whether the eye is on the outer side of the plane of some box of the block's outline shape
/// that faces `face` (a carpet's top is 1/16 up, a stair's lower step top is at 1/2). Grim's
/// `PositionPlace` flags an eye below the lowest such plane, so this never passes a flagged click.
fn face_visible(eye: Vec3, pos: BlockPos, face: Direction, shape: &VoxelShape) -> bool {
    let (x, y, z) = (pos.x as f64, pos.y as f64, pos.z as f64);
    shape.to_aabbs().iter().any(|b| match face {
        Direction::Up => eye.y > y + b.max.y,
        Direction::Down => eye.y < y + b.min.y,
        Direction::South => eye.z > z + b.max.z,
        Direction::North => eye.z < z + b.min.z,
        Direction::East => eye.x > x + b.max.x,
        Direction::West => eye.x < x + b.min.x,
    })
}

/// Vanilla-click checks (reach, then visibility, then the crosshair ray). Returns the real
/// raycast hit to send.
fn check_click<'a>(
    eye: Vec3,
    range: f64,
    pos: BlockPos,
    face: Option<Direction>,
    shape: &VoxelShape,
    hit: &'a HitResult,
) -> Result<&'a BlockHitResult, BlockActionRefusal> {
    if distance_to_block(eye, pos) > range {
        return Err(BlockActionRefusal::OutOfReach);
    }
    if let Some(f) = face
        && !face_visible(eye, pos, f, shape)
    {
        return Err(BlockActionRefusal::FaceNotVisible);
    }
    match hit {
        HitResult::Block(r)
            if !r.miss && r.block_pos == pos && face.is_none_or(|f| r.direction == f) =>
        {
            Ok(r)
        }
        _ => Err(BlockActionRefusal::NotLookingAtIt),
    }
}

/// Whether a placement clicked on `state` goes into that block (vanilla `canBeReplaced`) rather
/// than next to it. `held` is the item in hand as `"minecraft:snow"` (`""` for none).
fn click_replaces(state: BlockState, held: &str, face: Direction, hit_y: f64) -> bool {
    let kind = BlockKind::from(state);
    let same_item = !held.is_empty() && held == kind.to_str();
    let horizontal = !matches!(face, Direction::Up | Direction::Down);
    if kind == BlockKind::Snow {
        // layers 1-7 take another layer from a snow item (top face only); anything else only
        // replaces a single layer
        let layers: u8 = state.get_property("layers").and_then(|l| l.parse().ok()).unwrap_or(8);
        return if same_item && layers < 8 { face == Direction::Up } else { layers == 1 };
    }
    if kind.to_str().ends_with("_slab") {
        // a slab item of the same kind completes a half slab
        return match state.get_property("type") {
            Some("bottom") if same_item => face == Direction::Up || (hit_y > 0.5 && horizontal),
            Some("top") if same_item => face == Direction::Down || (hit_y <= 0.5 && horizontal),
            _ => false,
        };
    }
    tags::blocks::REPLACEABLE.contains(&kind) && !same_item
}

// Spec: specs/B6-placement-breaking.md §4.2 (place on a chosen face, vanilla target block)
#[allow(clippy::type_complexity)]
pub fn handle_place_block_queued(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &PlaceBlockQueued,
        &mut BlockStatePredictionHandler,
        &mut BlockActionResults,
        &HitResultComponent,
        &WorldHolder,
        &Position,
        &EntityDimensions,
        &Attributes,
        &Inventory,
    )>,
    mut finished: MessageWriter<BlockActionFinished>,
) {
    for (entity, queued, mut prediction, mut results, hit_result, world, position, dims, attrs, inv) in
        &mut query
    {
        commands.entity(entity).remove::<PlaceBlockQueued>();

        let world = world.shared.read();
        let clicked = world.get_block_state(queued.position).unwrap_or_default();
        let eye = position.up(dims.eye_height.into());
        let range = attrs.block_interaction_range.calculate();
        let checked = check_click(
            eye,
            range,
            queued.position,
            Some(queued.direction),
            clicked.outline_shape(),
            hit_result,
        );
        // The target depends on where the click lands (a slab) and what is held (snow).
        let held = match inv.held_item() {
            azalea_inventory::ItemStack::Present(d) => d.kind.to_str(),
            _ => "",
        };
        let hit_y = checked.map_or(0.5, |r| r.location.y - queued.position.y as f64);
        let target = if click_replaces(clicked, held, queued.direction, hit_y) {
            queued.position
        } else {
            queued.position.offset_with_direction(queued.direction)
        };
        let before = world.get_block_state(target).unwrap_or_default();
        results.next_id += 1;
        let mut result = BlockActionResult {
            id: results.next_id,
            position: target,
            outcome: BlockActionOutcome::Pending,
            refusal: None,
            seq: None,
            finish_ticks: None,
            ack_ticks: None,
            elapsed_ticks: 0,
            before,
        };
        match checked {
            Err(why) => {
                result.outcome = BlockActionOutcome::Refused;
                result.refusal = Some(why);
                finished.write(BlockActionFinished { entity, is_break: false, result });
            }
            Ok(r) => {
                let seq = prediction.start_predicting();
                result.seq = Some(seq);
                commands.trigger(SendGamePacketEvent::new(
                    entity,
                    ServerboundUseItemOn { hand: InteractionHand::MainHand, block_hit: r.into(), seq },
                ));
                commands.trigger(SwingArmEvent { entity });
            }
        }
        results.last_place = Some(result);
    }
}

// Spec: specs/B6-placement-breaking.md §4.4 (break: refused clicks and game modes, result opened here)
/// Opens a break result when mining is queued on a block (not again for the block already being
/// mined). A block we could not click as a vanilla client, or may not break in this game mode, is
/// refused here and the queued mining is dropped before it sends anything.
#[allow(clippy::type_complexity)]
pub fn begin_break_result(
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &MiningQueued,
            &mut BlockActionResults,
            &WorldHolder,
            &Position,
            &EntityDimensions,
            &Attributes,
            &HitResultComponent,
            (&LocalGameMode, &Inventory),
            Has<CheckBreakClick>,
        ),
        Added<MiningQueued>,
    >,
    mut finished: MessageWriter<BlockActionFinished>,
) {
    for (entity, queued, mut results, world, position, dims, attrs, hit_result, (mode, inv), checked) in
        &mut query
    {
        commands.entity(entity).remove::<CheckBreakClick>();
        if results.last_break.is_some_and(|b| {
            b.outcome == BlockActionOutcome::Pending && b.position == queued.position
        }) {
            continue;
        }
        let world = world.shared.read();
        let state = world.get_block_state(queued.position).unwrap_or_default();
        let eye = position.up(dims.eye_height.into());
        let range = attrs.block_interaction_range.calculate();
        results.next_id += 1;
        let mut result = BlockActionResult {
            id: results.next_id,
            position: queued.position,
            outcome: BlockActionOutcome::Pending,
            refusal: None,
            seq: None,
            finish_ticks: None,
            ack_ticks: None,
            elapsed_ticks: 0,
            before: state,
        };
        // mining.rs drops a restricted break silently; say so
        let refusal = if check_is_interaction_restricted(&world, queued.position, &mode.current, inv) {
            Some(BlockActionRefusal::NotAllowed)
        } else if checked {
            check_click(eye, range, queued.position, None, state.outline_shape(), hit_result).err()
        } else {
            None
        };
        if let Some(why) = refusal {
            result.outcome = BlockActionOutcome::Refused;
            result.refusal = Some(why);
            commands.entity(entity).remove::<MiningQueued>();
            finished.write(BlockActionFinished { entity, is_break: true, result });
        }
        results.last_break = Some(result);
    }
}

// Spec: specs/B6-placement-breaking.md §4.6 (remember the finishing packet's sequence number)
/// The finishing packet of a break went out: remember its sequence number.
pub fn record_break_sent(sent: On<BlockBreakSent>, mut query: Query<&mut BlockActionResults>) {
    let Ok(mut results) = query.get_mut(sent.entity) else { return };
    if let Some(b) = results.last_break.as_mut()
        && b.outcome == BlockActionOutcome::Pending
        && b.position == sent.position
    {
        b.seq = Some(sent.seq);
        b.finish_ticks = Some(b.elapsed_ticks);
        // the timeout counts from here
        b.elapsed_ticks = 0;
    }
}

// Spec: specs/B6-placement-breaking.md §4.4 (abort a break)
pub fn handle_stop_mining_queued(
    mut commands: Commands,
    mut query: Query<
        (Entity, &mut BlockActionResults, &mut MineProgress, Option<&Mining>),
        With<StopMiningQueued>,
    >,
    mut finished: MessageWriter<BlockActionFinished>,
) {
    for (entity, mut results, mut progress, mining) in &mut query {
        commands.entity(entity).remove::<(StopMiningQueued, Mining, MiningQueued)>();
        let Some(mining) = mining else { continue };
        commands.trigger(SendGamePacketEvent::new(
            entity,
            ServerboundPlayerAction {
                action: Action::AbortDestroyBlock,
                pos: mining.pos,
                direction: Direction::Down,
                seq: 0,
            },
        ));
        **progress = 0.;
        if let Some(b) = results.last_break.as_mut()
            && b.outcome == BlockActionOutcome::Pending
        {
            b.outcome = BlockActionOutcome::Cancelled;
            finished.write(BlockActionFinished { entity, is_break: true, result: *b });
        }
    }
}

// Spec: specs/B6-placement-breaking.md §4.6 (settle from the ack and the block left behind)
/// Counts ticks and settles pending actions from the ack and the block the server left behind.
pub fn settle_block_actions(
    mut query: Query<(
        Entity,
        &mut BlockActionResults,
        &BlockStatePredictionHandler,
        &WorldHolder,
        Has<Mining>,
        Has<MiningQueued>,
    )>,
    mut finished: MessageWriter<BlockActionFinished>,
) {
    for (entity, mut results, prediction, world, mining, queued) in &mut query {
        let results = &mut *results;
        for (is_break, slot) in [(false, &mut results.last_place), (true, &mut results.last_break)] {
            let Some(r) = slot.as_mut().filter(|r| r.outcome == BlockActionOutcome::Pending) else {
                continue;
            };
            r.elapsed_ticks += 1;
            let Some(seq) = r.seq else {
                // A break whose mining ended without a finishing packet was aborted (we looked
                // away, the block changed under us).
                if is_break && !mining && !queued {
                    r.outcome = BlockActionOutcome::Cancelled;
                    finished.write(BlockActionFinished { entity, is_break, result: *r });
                }
                continue;
            };
            let acked = prediction.acked_seq() >= seq;
            if acked && r.ack_ticks.is_none() {
                r.ack_ticks = Some(r.elapsed_ticks);
            }
            if acked || r.elapsed_ticks > ACK_TIMEOUT_TICKS {
                let now = world.shared.read().get_block_state(r.position).unwrap_or_default();
                r.outcome = if now != r.before {
                    BlockActionOutcome::Accepted
                } else {
                    // a timeout is not the server saying no
                    if !acked {
                        r.refusal = Some(BlockActionRefusal::NoAck);
                    }
                    BlockActionOutcome::Refused
                };
                finished.write(BlockActionFinished { entity, is_break, result: *r });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use azalea_block::{blocks, properties};
    use azalea_physics::collision::{Shapes, box_shape};

    use super::*;

    #[test]
    fn thin_shape_is_visible_from_between_its_top_and_the_cell_top() {
        let carpet = box_shape(0., 0., 0., 1., 0.0625, 1.);
        let pos = BlockPos::new(0, 64, 0);
        // eye half-way up the cell: above the carpet, below the unit cube's top
        let eye = Vec3::new(0.5, 64.5, 3.5);
        assert!(face_visible(eye, pos, Direction::Up, &carpet));
        assert!(!face_visible(eye, pos, Direction::Down, &carpet));
        assert!(face_visible(eye, pos, Direction::South, &carpet));
    }

    #[test]
    fn stair_lower_step_top_counts_as_visible() {
        let pos = BlockPos::new(0, 64, 0);
        let stairs = Shapes::or(box_shape(0., 0., 0., 1., 0.5, 1.), box_shape(0.5, 0.5, 0., 1., 1., 1.));
        let eye = Vec3::new(0.2, 64.8, 3.0);
        assert!(face_visible(eye, pos, Direction::Up, &stairs));
    }

    #[test]
    fn snow_and_slab_replacement_follow_the_held_item() {
        let snow = |n| BlockState::from(blocks::Snow { layers: n });
        assert!(click_replaces(snow(properties::Layers::_1), "minecraft:stone", Direction::Up, 0.5));
        assert!(!click_replaces(snow(properties::Layers::_3), "minecraft:stone", Direction::Up, 0.5));
        assert!(click_replaces(snow(properties::Layers::_3), "minecraft:snow", Direction::Up, 0.5));
        assert!(!click_replaces(snow(properties::Layers::_3), "minecraft:snow", Direction::North, 0.5));
        assert!(!click_replaces(snow(properties::Layers::_8), "minecraft:snow", Direction::Up, 0.5));
        let slab = |t| BlockState::from(blocks::OakSlab { kind: t, waterlogged: false });
        let bottom = slab(properties::SlabKind::Bottom);
        assert!(click_replaces(bottom, "minecraft:oak_slab", Direction::Up, 0.5));
        assert!(!click_replaces(bottom, "minecraft:stone", Direction::Up, 0.5));
        assert!(click_replaces(bottom, "minecraft:oak_slab", Direction::North, 0.75));
        assert!(!click_replaces(bottom, "minecraft:oak_slab", Direction::North, 0.25));
        assert!(!click_replaces(slab(properties::SlabKind::Double), "minecraft:oak_slab", Direction::Up, 0.5));
        assert!(click_replaces(BlockState::from(BlockKind::ShortGrass), "minecraft:stone", Direction::Up, 0.5));
        assert!(!click_replaces(BlockState::from(BlockKind::ShortGrass), "minecraft:short_grass", Direction::Up, 0.5));
    }
}
