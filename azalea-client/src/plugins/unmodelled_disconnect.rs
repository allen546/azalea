//! The disconnect for an unmodelled input that moves the bot by itself (patch 30, AD-F5.5).
//!
//! A `Movement` entry (see [`azalea_entity::unmodelled`]) means a server-set input changes the
//! bot's motion in a way the fork cannot predict, so the next position the bot sends would be
//! wrong. A vanilla player can always quit, so the bot does: at the start of the first
//! `GameTick` after the input arrives, before any system that computes or sends anything, it
//! closes the connection and sends nothing more. The play-state vanilla client sends no quit
//! packet either: `Connection#disconnect` only closes the channel.
//!
//! The gate acts only on a client that has [`DisconnectOnUnmodelledMovement`]; `mc-adapter`
//! inserts it for every bot. What happens next (stay off, or rejoin) is decided above the fork.
//! Spec: specs/F5-version-layer.md §4.6, specs/B1-movement.md §4.2.

use azalea_core::tick::GameTick;
use azalea_entity::{
    metadata::FallFlying, ActiveEffects, Attributes, LocalEntity, Physics,
    unmodelled::{
        RidingVehicle, UnmodelledContacts, gliding_entry, UnmodelledInput, UnmodelledKind, WorldBorderState,
        unmodelled_inputs,
    },
};
use azalea_physics::{PhysicsSystems, unmodelled_contacts::unmodelled_block_contacts};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use tracing::info;

use crate::{
    connection::RawConnection, disconnect::{DisconnectEvent, RemoveOnDisconnectBundle}, local_player::WorldHolder,
    mining::MiningSystems, tick_counter::TicksConnected,
};

/// Marks a client whose connection the gate may close.
#[derive(Clone, Component, Copy, Debug, Default)]
pub struct DisconnectOnUnmodelledMovement;

/// Why and when the gate closed the connection. It stays on the entity after the disconnect, so
/// the layer above can read it; whoever rejoins removes it first.
#[derive(Clone, Component, Debug, PartialEq)]
pub struct UnmodelledDisconnect {
    /// The client's tick count (`TicksConnected`) when the gate closed the connection.
    pub tick: u64,
    /// The `Movement` entries that caused it.
    pub inputs: Vec<UnmodelledInput>,
}

/// How many packets the client tried to send after the gate closed the connection. They are
/// dropped, never written; the ability check requires 0, which shows that no system that
/// computes a position or an action ran after the trigger.
#[derive(Clone, Component, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketsAfterUnmodelledDisconnect(pub u32);

pub struct UnmodelledDisconnectPlugin;
impl Plugin for UnmodelledDisconnectPlugin {
    fn build(&self, app: &mut App) {
        // Ordered before the counter (which every death-screen, click and look system follows), the
        // physics, mining, movement and tick-end systems. The gate also removes the connection and
        // world components at once, so any system it does not precede finds nothing to act on.
        app.add_systems(
            GameTick,
            (derive_unmodelled_contacts, disconnect_on_unmodelled_movement)
                .chain()
                .before(crate::tick_counter::increment_counter)
                .before(PhysicsSystems)
                .before(MiningSystems)
                .before(crate::movement::send_position)
                .before(crate::tick_end::game_tick_packet)
                .before(crate::loading::player_loaded_packet)
                .before(crate::respawn::tick_death_screen),
        );
    }
}

/// Derive the vehicle, block and border entries of [`UnmodelledContacts`] from the state at the
/// end of the last tick and the packets applied since.
// Spec: specs/F5-version-layer.md §4.6
#[allow(clippy::type_complexity)]
pub fn derive_unmodelled_contacts(
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &Physics,
            &WorldHolder,
            &TicksConnected,
            Option<&RidingVehicle>,
            Option<&WorldBorderState>,
            Option<&mut UnmodelledContacts>,
            Option<&FallFlying>,
        ),
        With<LocalEntity>,
    >,
) {
    for (entity, physics, world_holder, ticks, vehicle, border, contacts, fall_flying) in &mut query {
        let mut list = Vec::new();
        if vehicle.is_some() {
            list.push(UnmodelledInput::Passenger);
        }
        // Patch 34: the server set the glide flag on the bot.
        list.extend(gliding_entry(fall_flying.map(|f| f.0)));
        if border.is_some_and(|b| b.touches(&physics.bounding_box, ticks.0)) {
            list.push(UnmodelledInput::WorldBorder);
        }
        list.extend(unmodelled_block_contacts(&world_holder.shared.read(), &physics.bounding_box));
        match contacts {
            Some(mut c) => {
                if c.0 != list {
                    c.0 = list;
                }
            }
            None => {
                commands.entity(entity).insert(UnmodelledContacts(list));
            }
        }
    }
}

/// Close the connection of a client with [`DisconnectOnUnmodelledMovement`] when a `Movement`
/// entry is in force, and record [`UnmodelledDisconnect`].
// Spec: specs/F5-version-layer.md §4.6
#[allow(clippy::type_complexity)]
pub fn disconnect_on_unmodelled_movement(
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &Attributes,
            &ActiveEffects,
            &UnmodelledContacts,
            &TicksConnected,
            &mut RawConnection,
        ),
        (
            With<LocalEntity>,
            With<DisconnectOnUnmodelledMovement>,
            Without<UnmodelledDisconnect>,
        ),
    >,
    mut events: MessageWriter<DisconnectEvent>,
) {
    for (entity, attributes, effects, contacts, ticks, mut raw) in &mut query {
        let inputs: Vec<UnmodelledInput> = unmodelled_inputs(attributes, effects)
            .into_iter()
            .chain(contacts.0.iter().copied())
            .filter(|i| i.kind() == UnmodelledKind::Movement)
            .collect();
        if inputs.is_empty() {
            continue;
        }
        info!("unmodelled movement input in force, disconnecting {entity:?}: {inputs:?}");
        // Dropping the network side closes the socket; no packet is written first.
        raw.network = None;
        raw.is_alive = false;
        commands.entity(entity).insert((
            UnmodelledDisconnect { tick: ticks.0, inputs },
            PacketsAfterUnmodelledDisconnect::default(),
        ));
        // The same removal a `DisconnectEvent` does in `PostUpdate`, applied now so that the
        // systems after the gate in this tick find no world, position or connection to work on.
        commands.entity(entity).remove::<RemoveOnDisconnectBundle>();
        events.write(DisconnectEvent { entity, reason: None });
    }
}
