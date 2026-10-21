//! Interpolation of other entities and the bot-to-bot view (fork patch 31).
//!
//! Vanilla 1.21.11: `Minecraft#tick` applies the tick's packets, casts the crosshair ray
//! (`GameRenderer#pick`), handles the clicks (`handleKeybinds`), and only then ticks the entities
//! (`ClientLevel#tickEntities`), where each entity's `InterpolationHandler#interpolate` runs. So a
//! click in the tick a packet arrives is judged against the positions from before that packet's
//! first step, and the next tick's ray sees the moved box. The systems here run after the
//! physics set (the end of the bot's own tick) for that reason; the per-bot tick order among
//! entities is patch 32.
//!
//! Spec: specs/F5-version-layer.md §4.6 (What Null computes), specs/B1-movement.md §4.2

use azalea_core::tick::GameTick;
use azalea_entity::interpolation::{interpolate_entities, update_peer_views};
use azalea_physics::{PhysicsSystems, collision::entity_collisions::update_last_bounding_box};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

pub struct InterpolationPlugin;
impl Plugin for InterpolationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            (
                interpolate_entities,
                update_peer_views,
                // `LastBoundingBox` mirrors `Physics::bounding_box`; refresh it now, not in the
                // next `Update`, so the box does not lag a tick behind the position.
                update_last_bounding_box,
            )
                .chain()
                .after(PhysicsSystems)
                // Both read or write `Physics`, which these systems also touch.
                .after(crate::attack::handle_attack_queued)
                .after(crate::movement::send_position),
        );
    }
}
