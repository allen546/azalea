//! Place or break a block and learn whether the server accepted it.
//!
//! Vanilla answers a placement or a finished break with block updates and a
//! [`ClientboundBlockChangedAck`] for the sequence number we sent. After the ack the world holds the
//! server's truth: if the target block changed, the action was accepted, otherwise it was refused
//! (no support, an entity in the way). Before anything is sent, the click is checked as a vanilla
//! client would have to make it: the block is within reach, the face is visible from the eye, and
//! the crosshair ray hits that face. If not, the action is refused on the client, with a
//! [`BlockActionRefusal`], and no packet goes out (anticheats flag such clicks). Place with [`PlaceBlockQueued`]; start a break
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
use azalea_entity::{Attributes, Position, dimensions::EntityDimensions};
use azalea_physics::PhysicsSystems;
use azalea_protocol::packets::game::{
    ServerboundPlayerAction,
    s_interact::InteractionHand,
    s_player_action::Action,
    s_use_item_on::ServerboundUseItemOn,
};
use azalea_registry::{builtin::BlockKind, tags};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::{
    interact::{
        BlockStatePredictionHandler, SwingArmEvent, handle_start_use_item_queued,
        pick::HitResultComponent,
    },
    inventory::ensure_has_sent_carried_item,
    local_player::WorldHolder,
    mining::{BlockBreakSent, MineProgress, Mining, MiningQueued, MiningSystems},
    packet::game::SendGamePacketEvent,
};

/// Without an ack this many ticks after the action was sent, it is judged on the block alone.
const ACK_TIMEOUT_TICKS: u32 = 60;

pub struct BlockActionsPlugin;
impl Plugin for BlockActionsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BlockActionFinished>()
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockActionOutcome {
    /// Sent; the server's answer has not been read yet.
    Pending,
    /// The server acknowledged it and the block is as intended.
    Accepted,
    /// The server acknowledged it (or the wait timed out) and the block is unchanged.
    Refused,
    /// A break that was aborted: by [`StopMiningQueued`], or mining ended without finishing.
    Cancelled,
}

/// Why a click was refused on the client, before any packet was sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockActionRefusal {
    OutOfReach,
    /// The eye is not on the outer side of the face, so no ray can hit it.
    FaceNotVisible,
    /// The crosshair does not hit that block (and face, for a placement): look at it first.
    NotLookingAtIt,
}

impl BlockActionRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutOfReach => "out of reach",
            Self::FaceNotVisible => "face not visible from the eye",
            Self::NotLookingAtIt => "crosshair does not hit it",
        }
    }
}

/// The state of one placement or break, as of the last `GameTick`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockActionResult {
    /// Counts up per client, so a caller can tell a new action from the previous one.
    pub id: u32,
    /// The block that changes: for a placement the clicked block's neighbour, for a break the
    /// block being mined.
    pub position: BlockPos,
    pub outcome: BlockActionOutcome,
    /// Set when the outcome is `Refused` by the client's own checks (nothing was sent).
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

/// The latest placement and break results of a client.
#[derive(Clone, Component, Debug, Default)]
pub struct BlockActionResults {
    pub last_place: Option<BlockActionResult>,
    pub last_break: Option<BlockActionResult>,
    next_id: u32,
}

/// Sent once when a placement or break settles.
#[derive(Debug, Message)]
pub struct BlockActionFinished {
    pub entity: Entity,
    pub is_break: bool,
    pub result: BlockActionResult,
}

/// Start breaking the block at `position` in this `GameTick`. Unlike [`StartMiningBlockEvent`]
/// (an `Update` message), it reaches the server in the tick it was inserted.
///
/// [`StartMiningBlockEvent`]: crate::mining::StartMiningBlockEvent
#[derive(Component, Debug)]
pub struct BreakBlockQueued {
    pub position: BlockPos,
}

/// Turns [`BreakBlockQueued`] into forced mining, with the face from the crosshair when it is on
/// that block (as `handle_start_mining_block_event` does); [`begin_break_result`] then checks it.
pub fn handle_break_block_queued(
    mut commands: Commands,
    query: Query<(Entity, &BreakBlockQueued, &HitResultComponent)>,
) {
    for (entity, queued, hit_result) in &query {
        let direction = hit_result
            .as_block_hit_result_if_not_miss()
            .filter(|h| h.block_pos == queued.position)
            .map_or(Direction::Down, |h| h.direction);
        commands
            .entity(entity)
            .remove::<BreakBlockQueued>()
            .insert((MiningQueued { position: queued.position, direction, force: true }, CheckBreakClick));
    }
}

/// Marks mining queued by [`BreakBlockQueued`], so only those breaks get the vanilla-click check;
/// `StartMiningBlockEvent { force: true }` keeps its "mine regardless" contract.
#[derive(Component, Debug)]
pub struct CheckBreakClick;

/// Abort the break in progress on the next `GameTick`.
#[derive(Component, Debug, Default)]
pub struct StopMiningQueued;

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

/// Distance from the eye to the nearest point of the block's cube.
fn distance_to_block(eye: Vec3, pos: BlockPos) -> f64 {
    let d = |e: f64, lo: i32| (lo as f64 - e).max(e - (lo as f64 + 1.0)).max(0.0);
    let (dx, dy, dz) = (d(eye.x, pos.x), d(eye.y, pos.y), d(eye.z, pos.z));
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Whether the eye is on the outer side of the block's `face` plane.
fn face_visible(eye: Vec3, pos: BlockPos, face: Direction) -> bool {
    let n = face.normal();
    let c = |e: f64, lo: i32, n: i32| match n {
        1 => e - (lo as f64 + 1.0),
        -1 => lo as f64 - e,
        _ => 0.0,
    };
    c(eye.x, pos.x, n.x) + c(eye.y, pos.y, n.y) + c(eye.z, pos.z, n.z) > 0.0
}

/// Vanilla-click checks (reach, then visibility, then the crosshair ray). Returns the real
/// raycast hit to send.
fn check_click<'a>(
    eye: Vec3,
    range: f64,
    pos: BlockPos,
    face: Option<Direction>,
    hit: &'a HitResult,
) -> Result<&'a BlockHitResult, BlockActionRefusal> {
    if distance_to_block(eye, pos) > range {
        return Err(BlockActionRefusal::OutOfReach);
    }
    if let Some(f) = face
        && !face_visible(eye, pos, f)
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
    )>,
    mut finished: MessageWriter<BlockActionFinished>,
) {
    for (entity, queued, mut prediction, mut results, hit_result, world, position, dims, attrs) in
        &mut query
    {
        commands.entity(entity).remove::<PlaceBlockQueued>();

        let clicked = world.shared.read().get_block_state(queued.position).unwrap_or_default();
        let target = if tags::blocks::REPLACEABLE.contains(&BlockKind::from(clicked)) {
            queued.position
        } else {
            queued.position.offset_with_direction(queued.direction)
        };
        let before = world.shared.read().get_block_state(target).unwrap_or_default();
        let eye = position.up(dims.eye_height.into());
        let range = attrs.block_interaction_range.calculate();
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
        match check_click(eye, range, queued.position, Some(queued.direction), hit_result) {
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

/// Opens a break result when mining is queued on a block (not again for the block already being
/// mined). A block we could not click as a vanilla client is refused here and the queued mining
/// is dropped before it sends anything.
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
            Has<CheckBreakClick>,
        ),
        Added<MiningQueued>,
    >,
    mut finished: MessageWriter<BlockActionFinished>,
) {
    for (entity, queued, mut results, world, position, dims, attrs, hit_result, checked) in &mut query {
        commands.entity(entity).remove::<CheckBreakClick>();
        if results.last_break.is_some_and(|b| {
            b.outcome == BlockActionOutcome::Pending && b.position == queued.position
        }) {
            continue;
        }
        let before = world.shared.read().get_block_state(queued.position).unwrap_or_default();
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
            before,
        };
        if checked
            && let Err(why) = check_click(eye, range, queued.position, None, hit_result)
        {
            result.outcome = BlockActionOutcome::Refused;
            result.refusal = Some(why);
            commands.entity(entity).remove::<MiningQueued>();
            finished.write(BlockActionFinished { entity, is_break: true, result });
        }
        results.last_break = Some(result);
    }
}

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
                    BlockActionOutcome::Refused
                };
                finished.write(BlockActionFinished { entity, is_break, result: *r });
            }
        }
    }
}
