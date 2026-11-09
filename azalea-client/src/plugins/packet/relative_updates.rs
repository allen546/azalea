// How entity updates are processed. Each bot has its own world and its own copy of every entity
// (specs/plans/LOAD_CONTRACT.md "Swarm world design" item 1), so a bot applies the updates in
// its own packets and no arbitration between clients is needed.

use std::sync::Arc;

use azalea_core::entity_id::MinecraftEntityId;
use azalea_entity::LocalEntity;
use azalea_world::PartialWorld;
use bevy_ecs::prelude::*;
use derive_more::{Deref, DerefMut};
use parking_lot::RwLock;
use tracing::debug;

use crate::packet::as_system;

/// An [`EntityCommand`] that applies a "relative update" to an entity, which
/// means this update won't be run multiple times by different clients in the
/// same world.
///
/// This is used to avoid a bug where when there's multiple clients in the same
/// world and an entity sends a relative move packet to all clients, its
/// position gets desynced since the relative move is applied multiple times.
///
/// Don't use this unless you actually got an entity update packet that all
/// other clients within render distance will get too. You usually don't need
/// this when the change isn't relative either.
pub struct RelativeEntityUpdate {
    pub partial_world: Arc<RwLock<PartialWorld>>,
    // a function that takes the entity and updates it
    pub update: Box<dyn FnOnce(&mut EntityWorldMut) + Send + Sync>,
}
impl RelativeEntityUpdate {
    pub fn new(
        partial_world: Arc<RwLock<PartialWorld>>,
        update: impl FnOnce(&mut EntityWorldMut) + Send + Sync + 'static,
    ) -> Self {
        Self {
            partial_world,
            update: Box::new(update),
        }
    }
}

/// A component that counts the number of times this entity has been modified.
///
/// This is used for making sure two clients don't do the same relative update
/// on an entity.
///
/// If an entity is local (i.e. it's a client/LocalEntity), this component
/// should NOT be present in the entity.
#[derive(Component, Debug, Deref, DerefMut)]
pub struct UpdatesReceived(u32);

pub type EntityUpdateQuery<'world, 'state, 'a> =
    Query<'world, 'state, (&'a MinecraftEntityId, Option<&'a LocalEntity>)>;

/// See [`RelativeEntityUpdate`] for details.
// Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 1
///
/// Calling this function will have the same effect as using the Command, but
/// it's more performant than the Command.
pub fn should_apply_entity_update(
    partial_world: &mut PartialWorld,
    entity: Entity,
    entity_update_query: EntityUpdateQuery,
) -> bool {
    let partial_entity_infos = &mut partial_world.entity_infos;

    if Some(entity) == partial_entity_infos.owner_entity {
        // if the entity owns this partial world, it's always allowed to update itself
        return true;
    };

    let Ok((_minecraft_entity_id, local_entity)) = entity_update_query.get(entity) else {
        // this can happen when the entity despawns in the same Update that we got a
        // relative update for it
        debug!("called should_apply_entity_update on an entity with missing components {entity:?}");
        return false;
    };

    // A client never moves another client's entity. Every other entity is in this bot's own
    // world, so this bot's packets are the only source of its updates.
    local_entity.is_none()
}

impl EntityCommand for RelativeEntityUpdate {
    fn apply(self, mut entity_mut: EntityWorldMut) {
        let partial_world = self.partial_world.clone();
        let mut should_update = false;
        let entity = entity_mut.id();

        entity_mut.world_scope(|ecs| {
            as_system::<EntityUpdateQuery>(ecs, |query| {
                should_update = should_apply_entity_update(
                    &mut partial_world.write(),
                    entity,
                    query,
                );
            });
        });

        if should_update {
            (self.update)(&mut entity_mut);
        }
    }
}
