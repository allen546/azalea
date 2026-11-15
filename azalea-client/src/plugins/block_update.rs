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
            handle_block_update_event
                .after(handle_receive_chunk_event)
                // readers of a bot's block state run after the writers
                .before(azalea_entity::update_on_climbable)
                .before(crate::interact::pick::insert_initial_hit_result_component),
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
    /// Apply the queued updates in order: a pending cell takes the update as the server's last
    /// word (`ClientLevel#setServerVerifiedBlockState`, which does not write the world), any
    /// other cell whose chunk is loaded is written. An update for a chunk that is not loaded yet
    /// stays queued.
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
        let mut edits = Vec::new();
        for (pos, block_state) in self.list.drain(..) {
            if prediction.is_pending(pos) || world.get_block_state(pos).is_some() {
                edits.push((pos, block_state));
            } else {
                kept.push((pos, block_state));
            }
        }
        // One frame's edits go in together: one section copy per section (design item 5).
        prediction.set_server_verified(world, &edits);
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
        // Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 5
        let edits: Vec<_> = queued.list.drain(..).collect();
        prediction_handler.set_server_verified(&world, &edits);
    }
}
