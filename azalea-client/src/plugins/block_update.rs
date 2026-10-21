use azalea_block::BlockState;
use azalea_core::position::BlockPos;
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::{
    chunks::handle_receive_chunk_event, interact::BlockStatePredictionHandler,
    local_player::WorldHolder,
};

pub struct BlockUpdatePlugin;
impl Plugin for BlockUpdatePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            // has to be after ReceiveChunkEvent is handled so if we get chunk+blockupdate in one
            // Update then the block update actually gets applied
            handle_block_update_event.after(handle_receive_chunk_event),
        );
    }
}

/// A component that holds the list of block updates that need to be handled.
///
/// This is updated by `read_packets` (in `PreUpdate`) and handled/cleared by
/// [`handle_block_update_event`] (`Update`).
///
/// This is a component instead of an ECS event for performance reasons.
#[derive(Clone, Component, Debug, Default)]
pub struct QueuedServerBlockUpdates {
    pub list: Vec<(BlockPos, BlockState)>,
}

impl QueuedServerBlockUpdates {
    /// Apply the queued updates whose chunk is loaded, in order: into the known server state of a
    /// retained cell (`ClientLevel#setServerVerifiedBlockState`), else into the world. An update
    /// for a chunk that is not loaded yet stays queued.
    ///
    /// The ack handler runs this first: vanilla applies a block update the moment its packet
    /// arrives, so an update that came before the ack is already in the known state when the ack
    /// settles the prediction (else the cell would flash back to its old state first).
    // Spec: specs/B6-placement-breaking.md §4.10 (write and roll back)
    pub fn apply_loaded(
        &mut self,
        world: &azalea_world::World,
        prediction: &mut BlockStatePredictionHandler,
    ) {
        let mut kept = Vec::new();
        for (pos, block_state) in self.list.drain(..) {
            if world.get_block_state(pos).is_none() {
                kept.push((pos, block_state));
            } else if !prediction.update_known_server_state(pos, block_state) {
                world.chunks.set_block_state(pos, block_state);
            }
        }
        self.list = kept;
    }
}

pub fn handle_block_update_event(
    mut query: Query<(
        &mut QueuedServerBlockUpdates,
        &WorldHolder,
        &mut BlockStatePredictionHandler,
    )>,
) {
    for (mut queued, world_holder, mut prediction_handler) in query.iter_mut() {
        let world = world_holder.shared.read();
        for (pos, block_state) in queued.list.drain(..) {
            if !prediction_handler.update_known_server_state(pos, block_state) {
                world.chunks.set_block_state(pos, block_state);
            }
        }
    }
}
