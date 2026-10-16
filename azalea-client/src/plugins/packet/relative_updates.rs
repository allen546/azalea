// How entity updates are processed (to avoid issues with shared worlds)
// - Null fork: one client at a time, recorded in `UpdatedBy`, applies relative
//   updates for each non-local entity; packets for it from other clients are
//   ignored.
// - when the updater stops tracking the entity (it leaves the updater's
//   `LoadedBy`), the next client that gets an update for it takes over.
// - the old per-client counters were never synced when a client loaded an
//   entity, so after the leading client died or moved away, no client could
//   update the entity and it froze in place.

use std::sync::Arc;

use azalea_core::entity_id::MinecraftEntityId;
use azalea_entity::LoadedBy;
use azalea_entity::LocalEntity;
use azalea_world::PartialWorld;
use bevy_ecs::prelude::*;
use derive_more::{Deref, DerefMut};
use parking_lot::RwLock;
use tracing::{debug, warn};

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

/// The client whose packets currently drive this entity's relative updates.
///
/// Null fork: replaces the per-client update counters, which froze an entity
/// for every client once the client that was ahead stopped receiving updates
/// for it (it died, respawned, disconnected or moved out of range). Another
/// client takes over as soon as the updater no longer has it in `LoadedBy`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatedBy(pub Entity);

pub type EntityUpdateQuery<'world, 'state, 'a> = Query<
    'world,
    'state,
    (
        &'a MinecraftEntityId,
        Option<&'a UpdatedBy>,
        Option<&'a LoadedBy>,
        Option<&'a LocalEntity>,
    ),
>;

/// See [`RelativeEntityUpdate`] for details.
///
/// Calling this function will have the same effect as using the Command, but
/// it's more performant than the Command.
pub fn should_apply_entity_update(
    commands: &mut Commands,
    partial_world: &mut PartialWorld,
    entity: Entity,
    entity_update_query: EntityUpdateQuery,
) -> bool {
    let partial_entity_infos = &mut partial_world.entity_infos;

    if Some(entity) == partial_entity_infos.owner_entity {
        // if the entity owns this partial world, it's always allowed to update itself
        return true;
    };

    let Ok((_minecraft_entity_id, updated_by, loaded_by, local_entity)) =
        entity_update_query.get(entity)
    else {
        // this can happen when the entity despawns in the same Update that we got a
        // relative update for it
        debug!("called should_apply_entity_update on an entity with missing components {entity:?}");
        return false;
    };

    if local_entity.is_some() {
        // a client tried to update another client, which isn't allowed
        return false;
    }

    let Some(this_client) = partial_entity_infos.owner_entity else {
        // a world with no owner (tests): there is only one source of updates
        return true;
    };

    let can_update = match updated_by {
        None => true,
        Some(UpdatedBy(c)) if *c == this_client => true,
        // the updater no longer tracks this entity, so its packets stopped
        Some(UpdatedBy(c)) => !loaded_by.is_some_and(|l| l.contains(c)),
    };
    if can_update && updated_by != Some(&UpdatedBy(this_client)) {
        commands.entity(entity).insert(UpdatedBy(this_client));
    }
    can_update
}

impl EntityCommand for RelativeEntityUpdate {
    fn apply(self, mut entity_mut: EntityWorldMut) {
        let partial_world = self.partial_world.clone();
        let mut should_update = false;
        let entity = entity_mut.id();

        entity_mut.world_scope(|ecs| {
            as_system::<(Commands, EntityUpdateQuery)>(ecs, |(mut commands, query)| {
                should_update = should_apply_entity_update(
                    &mut commands,
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

/// A system that logs a warning if an entity has both [`UpdatedBy`]
/// and [`LocalEntity`].
pub fn debug_detect_updates_received_on_local_entities(
    query: Query<Entity, (With<LocalEntity>, With<UpdatedBy>)>,
) {
    for entity in &query {
        warn!("Entity {entity:?} has both LocalEntity and UpdatedBy");
    }
}
