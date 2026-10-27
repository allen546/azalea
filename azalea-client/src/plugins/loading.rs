use azalea_core::{position::ChunkPos, tick::GameTick};
use azalea_entity::{HasClientLoaded, InLoadedChunk, LocalEntity, Position};
use azalea_physics::PhysicsSystems;
use azalea_protocol::packets::game::ServerboundPlayerLoaded;
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::{local_player::WorldHolder, mining::MiningSystems, packet::game::SendGamePacketEvent};

pub struct PlayerLoadedPlugin;
impl Plugin for PlayerLoadedPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(GameTick, update_in_loaded_chunk_of_clients);
        app.add_systems(
            GameTick,
            // vanilla runs this on gameMode.tick()
            player_loaded_packet
                .after(update_in_loaded_chunk_of_clients)
                .before(PhysicsSystems)
                .before(MiningSystems)
                .before(crate::movement::send_position),
        );
    }
}

// this component is removed on respawn or disconnect
// (notably, it's not removed on login)

// mojmap interchangeably calls it 'has client loaded' and 'has player loaded',
// i prefer the client one because it makes it clear that the component is only
// present on our own clients

#[allow(clippy::type_complexity)]
pub fn player_loaded_packet(
    mut commands: Commands,
    query: Query<
        Entity,
        (
            With<LocalEntity>,
            Without<HasClientLoaded>,
            // the vanilla client waits for the chunk mesh to be "compiled" for the renderer (or
            // some other conditions) before sending PlayerLoaded. see LevelLoadStatusManager.tick
            // in the decompiled source
            With<InLoadedChunk>,
        ),
    >,
) {
    for entity in query.iter() {
        commands.trigger(SendGamePacketEvent::new(entity, ServerboundPlayerLoaded));
        commands.entity(entity).insert(HasClientLoaded);
    }
}

// Patch 43, `LevelLoadStatusManager#tick`: a vanilla client sends `PlayerLoaded` when the chunk it
// stands in is in its own level. In a swarm the shared `World` holds a chunk while any bot holds it,
// so after a respawn (which empties this bot's own chunks) the shared storage still answered "loaded"
// and the bot ticked and moved before the server had sent its chunks again (Grim `Simulation`
// alerts after the respawn). The bot's own `PartialWorld` decides.
// Spec: specs/F5-version-layer.md §4.6 (What Null computes)
pub fn update_in_loaded_chunk_of_clients(
    mut commands: Commands,
    query: Query<(Entity, &Position, &WorldHolder, Has<InLoadedChunk>), With<LocalEntity>>,
) {
    for (entity, position, world_holder, was_loaded) in &query {
        let loaded = world_holder.partial.read().chunks.has_chunk(&ChunkPos::from(position));
        if loaded && !was_loaded {
            commands.entity(entity).insert(InLoadedChunk);
        } else if !loaded && was_loaded {
            commands.entity(entity).remove::<InLoadedChunk>();
        }
    }
}
