//! Block prediction as vanilla's `ClientLevel` does it (Null fork patch 76).
//!
//! A prediction is a write into the bot's own [`World`]. The [`BlockStatePredictionHandler`]
//! keeps, per pending cell, the state the server last told the bot about, so the ack can settle
//! the cell. Design: specs/proposals/CLIENT_LEVEL_MODEL.md.
// Spec: specs/B6-placement-breaking.md §4.10

use std::collections::HashMap;

use azalea_block::{BlockState, fluid_state::FluidState};
use azalea_core::position::{BlockPos, Vec3};
use bevy_ecs::component::Component;

use crate::{ChunkStorage, World};

/// A read-only source of block states: a bot's `World`, or its chunk storage. The bot's own
/// predictions are in the `World`, so every reader sees them, as vanilla's readers do.
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

/// A component that contains information about our local block state predictions
/// (vanilla `BlockStatePredictionHandler`, one per `ClientLevel`).
///
/// The predicted block itself is in the bot's own [`World`]; the handler keeps what the server
/// last said about each pending cell.
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
    /// The last block the server told us about (the state the ack writes back when the cell
    /// shows something else).
    block_state: BlockState,
    /// Used for teleporting the player back if we're colliding with the block
    /// that got placed back.
    player_pos: Vec3,
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

    /// A block write inside a prediction scope (`ClientLevel#setBlock` while predicting): write
    /// the bot's world, and when the write changed the cell, keep the old state for the ack
    /// (`BlockStatePredictionHandler#retainKnownServerState`). A cell that is already pending
    /// keeps its first old state and takes the current sequence. Returns whether the cell changed.
    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    pub fn predict_block(
        &mut self,
        world: &World,
        pos: BlockPos,
        state: BlockState,
        player_pos: Vec3,
    ) -> bool {
        let Some(old_state) = world.set_block_state(pos, state) else {
            return false;
        };
        if old_state == state {
            return false;
        }
        self.server_state
            .entry(pos)
            .and_modify(|s| s.seq = self.seq)
            .or_insert(ServerVerifiedState { seq: self.seq, block_state: old_state, player_pos });
        true
    }

    /// Server block updates (`ClientLevel#setServerVerifiedBlockState`): a pending cell takes the
    /// state as the server's last word and the world is not written; any other cell is written.
    /// The edits go in together, one section copy per section.
    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    pub fn set_server_verified(&mut self, world: &World, edits: &[(BlockPos, BlockState)]) {
        let mut to_write = Vec::with_capacity(edits.len());
        for &(pos, state) in edits {
            if !self.update_known_server_state(pos, state) {
                to_write.push((pos, state));
            }
        }
        world.chunks.set_block_states(&to_write);
    }

    /// Save this update as the server's last word on a pending cell
    /// (`BlockStatePredictionHandler#updateKnownServerState`). Returns whether the cell is
    /// pending. The caller writes the world when it is not.
    pub fn update_known_server_state(&mut self, pos: BlockPos, state: BlockState) -> bool {
        if let Some(s) = self.server_state.get_mut(&pos) {
            s.block_state = state;
            true
        } else {
            false
        }
    }

    /// Whether a cell is pending (predicted, not yet acked).
    pub fn is_pending(&self, pos: BlockPos) -> bool {
        self.server_state.contains_key(&pos)
    }

    /// The state of a cell as the server has it: the kept state if the cell is pending, else the
    /// world's. The world shows this bot's own prediction while the cell is pending, so Null's
    /// judgement of a sent place or break reads here (B6 §4.6). Vanilla has no such reader.
    // Spec: specs/B6-placement-breaking.md §4.6 (settle from the ack and the block left behind)
    pub fn server_block_state(&self, world: &World, pos: BlockPos) -> Option<BlockState> {
        match self.server_state.get(&pos) {
            Some(s) => Some(s.block_state),
            None => world.get_block_state(pos),
        }
    }

    /// Drop everything: a new `ClientLevel` (dimension change) starts with a new handler.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// `BlockStatePredictionHandler#endPredictionsUpTo`: remove every pending cell up to `seq`;
    /// where the world shows something else than the kept state, write the kept state
    /// (`ClientLevel#syncBlockState`). Returns the cells that differed, oldest first, as
    /// `(cell, kept state, player position when first kept)`: the caller does the player snap-back
    /// test. A cell in a chunk the bot has forgotten reads as missing, so it differs, and the write
    /// does nothing.
    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    pub fn end_predictions_up_to(&mut self, seq: u32, world: &World) -> Vec<(BlockPos, BlockState, Vec3)> {
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
            if world.get_block_state(pos) != Some(state.block_state) {
                world.set_block_state(pos, state.block_state);
                changed.push((pos, state.block_state, state.player_pos));
            }
        }
        changed
    }
}
