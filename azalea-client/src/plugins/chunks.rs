//! Used for Minecraft's chunk batching introduced in 23w31a (1.20.2). It's used
//! for making the server spread out how often it sends us chunk packets
//! depending on our receiving speed.

use std::{
    io::Cursor,
    time::{Duration, Instant},
};

use azalea_core::position::ChunkPos;
use azalea_protocol::packets::game::{
    c_level_chunk_with_light::ClientboundLevelChunkWithLight,
    s_chunk_batch_received::ServerboundChunkBatchReceived,
};
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tracing::{error, trace};

use crate::{
    inventory::InventorySystems, local_player::WorldHolder, packet::game::SendGamePacketEvent,
    respawn::perform_respawn,
};

pub struct ChunksPlugin;
impl Plugin for ChunksPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                handle_chunk_batch_start_event,
                handle_receive_chunk_event,
                handle_chunk_batch_finished_event,
            )
                .chain()
                .before(InventorySystems)
                .before(perform_respawn)
                // patch 53: the chunk packet turns this bot's overrides off before they are read
                .before(azalea_entity::update_on_climbable),
        )
        .add_message::<ReceiveChunkEvent>()
        .add_message::<ChunkBatchStartEvent>()
        .add_message::<ChunkBatchFinishedEvent>();
    }
}

#[derive(Message)]
pub struct ReceiveChunkEvent {
    pub entity: Entity,
    pub packet: ClientboundLevelChunkWithLight,
    /// The world the bot held when the packet arrived. A respawn into another dimension swaps
    /// it, and a chunk of the old level must not be decoded into the new one.
    ///
    /// Weak, so a queued event does not keep a dropped level alive (that would make a later
    /// respawn into a same-named dimension find the old level).
    pub world: std::sync::Weak<parking_lot::RwLock<azalea_world::World>>,
}

#[derive(Clone, Component, Debug)]
pub struct ChunkBatchInfo {
    pub start_time: Instant,
    pub aggregated_duration_per_chunk: Duration,
    pub old_samples_weight: u32,
}

#[derive(Message)]
pub struct ChunkBatchStartEvent {
    pub entity: Entity,
}
#[derive(Message)]
pub struct ChunkBatchFinishedEvent {
    pub entity: Entity,
    pub batch_size: u32,
}

pub fn handle_receive_chunk_event(
    mut events: MessageReader<ReceiveChunkEvent>,
    mut query: Query<(&WorldHolder, &mut crate::interact::BlockStatePredictionHandler)>,
) {
    let _bt = crate::bench_metrics::Timer::new(crate::bench_metrics::Kind::ChunkEvent);
    for event in events.read() {
        let pos = ChunkPos::new(event.packet.x, event.packet.z);

        let (local_player, mut prediction) = query.get_mut(event.entity).unwrap();
        // `ClientPacketListener` handles a chunk packet in packet order, against the level of
        // that moment; a later respawn builds a new level. A chunk of a level the bot has left
        // is dropped, as vanilla's write into the old level is lost with it.
        // Spec: specs/F5-version-layer.md §4.6 (What Null computes)
        if !std::sync::Weak::ptr_eq(&event.world, &std::sync::Arc::downgrade(&local_player.shared)) {
            trace!("Dropping chunk {pos:?} of a level the bot has left");
            continue;
        }
        // `ClientPacketListener#updateLevelChunk`: the packet's blocks win over a pending
        // prediction; the retained state stays for the ack.
        // Spec: specs/B6-placement-breaking.md §4.10
        prediction.clear_predictions_in_chunk(pos);

        let mut world = local_player.shared.write();
        let mut partial_world = local_player.partial.write();

        // Every bot parses its own packet: a bot links a section only when its own packet held
        // the same content (section store), never because another bot had the chunk.
        // Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 4
        let heightmaps = &event.packet.chunk_data.heightmaps;

        if let Err(e) = partial_world.chunks.replace_with_packet_data(
            &pos,
            &mut Cursor::new(&event.packet.chunk_data.data),
            heightmaps,
            &mut world.chunks,
        ) {
            error!(
                "Couldn't set chunk data: {e}. World height: {}",
                world.chunks.height()
            );
        }
    }
}

impl ChunkBatchInfo {
    pub fn batch_finished(&mut self, batch_size: u32) {
        if batch_size == 0 {
            return;
        }
        let batch_duration = self.start_time.elapsed();
        let duration_per_chunk = batch_duration / batch_size;
        let clamped_duration = Duration::clamp(
            duration_per_chunk,
            self.aggregated_duration_per_chunk / 3,
            self.aggregated_duration_per_chunk * 3,
        );
        self.aggregated_duration_per_chunk =
            ((self.aggregated_duration_per_chunk * self.old_samples_weight) + clamped_duration)
                / (self.old_samples_weight + 1);
        self.old_samples_weight = u32::min(49, self.old_samples_weight + 1);
    }

    pub fn desired_chunks_per_tick(&self) -> f32 {
        (7000000. / self.aggregated_duration_per_chunk.as_nanos() as f64) as f32
    }
}

pub fn handle_chunk_batch_start_event(
    mut query: Query<&mut ChunkBatchInfo>,
    mut events: MessageReader<ChunkBatchStartEvent>,
) {
    for event in events.read() {
        if let Ok(mut chunk_batch_info) = query.get_mut(event.entity) {
            chunk_batch_info.start_time = Instant::now();
        }
    }
}

pub fn handle_chunk_batch_finished_event(
    mut query: Query<&mut ChunkBatchInfo>,
    mut events: MessageReader<ChunkBatchFinishedEvent>,
    mut commands: Commands,
) {
    for event in events.read() {
        if let Ok(mut chunk_batch_info) = query.get_mut(event.entity) {
            chunk_batch_info.batch_finished(event.batch_size);
            let desired_chunks_per_tick = chunk_batch_info.desired_chunks_per_tick();
            commands.trigger(SendGamePacketEvent::new(
                event.entity,
                ServerboundChunkBatchReceived {
                    desired_chunks_per_tick,
                },
            ));
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChunkReceiveSpeedAccumulator {
    batch_sizes: Vec<u32>,
    /// as milliseconds
    batch_durations: Vec<u32>,
    index: usize,
    filled_size: usize,
}
impl ChunkReceiveSpeedAccumulator {
    pub fn new(capacity: usize) -> Self {
        Self {
            batch_sizes: vec![0; capacity],
            batch_durations: vec![0; capacity],
            index: 0,
            filled_size: 0,
        }
    }

    pub fn accumulate(&mut self, batch_size: u32, batch_duration: Duration) {
        self.batch_sizes[self.index] = batch_size;
        self.batch_durations[self.index] =
            f32::clamp(batch_duration.as_millis() as f32, 0., 15000.) as u32;
        self.index = (self.index + 1) % self.batch_sizes.len();
        if self.filled_size < self.batch_sizes.len() {
            self.filled_size += 1;
        }
    }

    pub fn get_millis_per_chunk(&self) -> f64 {
        let mut total_batch_size = 0;
        let mut total_batch_duration = 0;
        for i in 0..self.filled_size {
            total_batch_size += self.batch_sizes[i];
            total_batch_duration += self.batch_durations[i];
        }
        if total_batch_size == 0 {
            return 0.;
        }
        total_batch_duration as f64 / total_batch_size as f64
    }
}

impl Default for ChunkBatchInfo {
    fn default() -> Self {
        Self {
            start_time: Instant::now(),
            aggregated_duration_per_chunk: Duration::from_millis(2),
            old_samples_weight: 1,
        }
    }
}
