//! Per-bot block prediction (Null fork patch 53).
//!
//! The shared [`World`] holds server data only. Each bot keeps its own predicted blocks in its
//! [`BlockStatePredictionHandler`] and reads them first through a [`BotWorld`].
// Spec: specs/B6-placement-breaking.md §4.10

use std::collections::HashMap;

use azalea_block::{BlockState, fluid_state::FluidState};
use azalea_core::position::{BlockPos, ChunkPos, Vec3};
use bevy_ecs::component::Component;

use crate::{ChunkStorage, World};

/// A read-only source of block states: shared truth (`ChunkStorage`) or one bot's view of it
/// (`BotWorld`). Read paths that must see a bot's own predictions are handed a `BotWorld`.
pub trait BlockView {
    fn get_block_state(&self, pos: BlockPos) -> Option<BlockState>;

    fn get_fluid_state(&self, pos: BlockPos) -> Option<FluidState> {
        self.get_block_state(pos).map(FluidState::from)
    }

    /// `FluidState#getHeight(BlockGetter, BlockPos)` for the fluid at `pos`: a full block when the
    /// cell above holds the same fluid (`FlowingFluid#getHeight`, `hasSameAbove`), else its own
    /// height. The flow code (`FlowingFluid#getFlow`) uses the own height instead.
    // Spec: specs/B1-movement.md §4.6 (patch 64: fluid height with the fluid above)
    fn fluid_height_at(&self, pos: BlockPos, fluid: &FluidState) -> f32 {
        let above = self.get_fluid_state(pos.up(1));
        match above {
            Some(a) if fluid.amount > 0 && a.amount > 0 && a.kind == fluid.kind => 1.0,
            _ => fluid.height(),
        }
    }
}

impl BlockView for ChunkStorage {
    fn get_block_state(&self, pos: BlockPos) -> Option<BlockState> {
        self.0.get_block_state(pos)
    }
}

impl BlockView for World {
    fn get_block_state(&self, pos: BlockPos) -> Option<BlockState> {
        self.chunks.get_block_state(pos)
    }
}

/// A component that contains information about our local block state predictions.
///
/// Vanilla keeps this per `ClientLevel` (`BlockStatePredictionHandler`); Null keeps it per bot,
/// and it also holds the predicted block itself, because the shared world must not.
#[derive(Clone, Component, Debug, Default)]
pub struct BlockStatePredictionHandler {
    /// The total number of changes that this client has made to blocks.
    seq: u32,
    /// The highest sequence number the server has acknowledged with
    /// `ClientboundBlockChangedAck`.
    acked_seq: u32,
    server_state: HashMap<BlockPos, ServerVerifiedState>,
}

#[derive(Clone, Debug)]
struct ServerVerifiedState {
    seq: u32,
    /// The last block the server told us about (the state the ack settles against).
    block_state: BlockState,
    /// Used for teleporting the player back if we're colliding with the block
    /// that got placed back.
    player_pos: Vec3,
    /// The block this bot shows itself. `None`: the override is off (the cell's chunk was
    /// re-sent or forgotten) but the retained state stays for the ack.
    predicted: Option<BlockState>,
}

impl BlockStatePredictionHandler {
    /// The sequence number of the latest change this client sent.
    pub fn seq(&self) -> u32 {
        self.seq
    }

    /// The highest sequence number the server has acknowledged so far.
    pub fn acked_seq(&self) -> u32 {
        self.acked_seq
    }

    /// Get the next sequence number that we're going to use and increment the
    /// value.
    pub fn start_predicting(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    /// Should be called right before the client updates a block with its prediction
    /// (`ClientLevel#setBlock` while predicting). `old_state` is what the bot sees now.
    ///
    /// The retained state lets the ack settle the cell when the server sends no update.
    pub fn retain_known_server_state(
        &mut self,
        pos: BlockPos,
        old_state: BlockState,
        player_pos: Vec3,
    ) {
        self.server_state
            .entry(pos)
            .and_modify(|s| s.seq = self.seq)
            .or_insert(ServerVerifiedState {
                seq: self.seq,
                block_state: old_state,
                player_pos,
                predicted: None,
            });
    }

    /// Show `state` at `pos` to this bot only. The cell must be retained first.
    pub fn set_predicted(&mut self, pos: BlockPos, state: BlockState) {
        if let Some(s) = self.server_state.get_mut(&pos) {
            s.predicted = Some(state);
        }
    }

    /// The block this bot predicted at `pos`, if its override is on.
    pub fn predicted(&self, pos: BlockPos) -> Option<BlockState> {
        self.server_state.get(&pos).and_then(|s| s.predicted)
    }

    /// Save this update as the correct server state so when the server sends a
    /// `ClientboundBlockChangedAck` we don't roll back this new update
    /// (`ClientLevel#setServerVerifiedBlockState`). Returns whether the cell is retained.
    /// The caller still writes the update to the shared world.
    pub fn update_known_server_state(&mut self, pos: BlockPos, state: BlockState) -> bool {
        if let Some(s) = self.server_state.get_mut(&pos) {
            s.block_state = state;
            true
        } else {
            false
        }
    }

    /// Turn the overrides in a chunk off, keeping the retained states for the ack. Used when
    /// this bot's chunk packet arrives (vanilla `ClientPacketListener#updateLevelChunk` replaces
    /// the chunk, so the packet's blocks win) and when it forgets the chunk.
    pub fn clear_predictions_in_chunk(&mut self, chunk: ChunkPos) {
        for (pos, s) in self.server_state.iter_mut() {
            if ChunkPos::from(*pos) == chunk {
                s.predicted = None;
            }
        }
    }

    /// Drop everything: a new `ClientLevel` (dimension change) starts with a new handler.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether an override lies inside the block range `min..=max` (both corners included).
    /// This is one `is_empty` check when nothing is pending.
    pub fn has_override_in(&self, min: BlockPos, max: BlockPos) -> bool {
        !self.server_state.is_empty()
            && self.server_state.iter().any(|(p, s)| {
                s.predicted.is_some()
                    && (min.x..=max.x).contains(&p.x)
                    && (min.y..=max.y).contains(&p.y)
                    && (min.z..=max.z).contains(&p.z)
            })
    }

    /// Settle every retained cell up to `seq` (vanilla `BlockStatePredictionHandler#endPredictionsUpTo`
    /// then the compare in `ClientLevel#syncBlockState`) and return the cells whose shown state
    /// differs from the retained one, oldest first, as `(cell, retained state, player position
    /// when first retained)`. The bot's shown state is its prediction, else the shared block.
    /// Nothing is written: the shared world already holds the server's block, so dropping the
    /// prediction is the rollback. The caller does the player snap-back.
    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    pub fn settle_up_to(&mut self, seq: u32, world: &World) -> Vec<(BlockPos, BlockState, Vec3)> {
        self.acked_seq = self.acked_seq.max(seq);
        // oldest first, so the newest prediction decides the snap when several cells collide
        let mut settled: Vec<(BlockPos, ServerVerifiedState)> = self
            .server_state
            .iter()
            .filter(|(_, s)| s.seq <= seq)
            .map(|(p, s)| (*p, s.clone()))
            .collect();
        settled.sort_by_key(|(p, s)| (s.seq, p.x, p.y, p.z));
        let mut changed = Vec::new();
        for (pos, state) in settled {
            self.server_state.remove(&pos);
            let client_block_state = state
                .predicted
                .unwrap_or_else(|| world.get_block_state(pos).unwrap_or_default());
            if client_block_state != state.block_state {
                changed.push((pos, state.block_state, state.player_pos));
            }
        }
        changed
    }
}

/// The world as one bot sees it: shared server data plus this bot's pending cells.
#[derive(Clone, Copy)]
pub struct BotWorld<'a> {
    /// Shared server truth. Use it for chunk presence, height, entities and registries, never
    /// to read a block that the bot's own predictions must show.
    pub world: &'a World,
    pending: &'a BlockStatePredictionHandler,
}

impl<'a> BotWorld<'a> {
    pub fn new(world: &'a World, pending: &'a BlockStatePredictionHandler) -> Self {
        Self { world, pending }
    }

    /// Whether any of this bot's overrides lies in the block range (`min` and `max` included).
    pub fn has_override_in(&self, min: BlockPos, max: BlockPos) -> bool {
        self.pending.has_override_in(min, max)
    }
}

impl BlockView for BotWorld<'_> {
    fn get_block_state(&self, pos: BlockPos) -> Option<BlockState> {
        // `server_state` empty is the common case: one check, then the shared read
        if let Some(predicted) = self.pending.predicted(pos) {
            return Some(predicted);
        }
        self.world.chunks.get_block_state(pos)
    }
}

impl BotWorld<'_> {
    pub fn get_block_state(&self, pos: BlockPos) -> Option<BlockState> {
        BlockView::get_block_state(self, pos)
    }

    pub fn get_fluid_state(&self, pos: BlockPos) -> Option<FluidState> {
        BlockView::get_fluid_state(self, pos)
    }
}

static NOTHING_PENDING: std::sync::LazyLock<BlockStatePredictionHandler> =
    std::sync::LazyLock::new(BlockStatePredictionHandler::default);

impl<'a> BotWorld<'a> {
    /// A view with no pending cells: shared truth. For entities that are not a bot of ours and
    /// for readers (pathfinder, block search) that keep to shared truth.
    pub fn truth(world: &'a World) -> Self {
        Self { world, pending: &NOTHING_PENDING }
    }
}

impl<'a> BotWorld<'a> {
    /// The view for an entity that may have no handler (a physics-only entity): shared truth then.
    pub fn of(world: &'a World, pending: Option<&'a BlockStatePredictionHandler>) -> Self {
        match pending {
            Some(pending) => Self::new(world, pending),
            None => Self::truth(world),
        }
    }
}
