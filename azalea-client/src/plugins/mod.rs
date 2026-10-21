use bevy_app::{PluginGroup, PluginGroupBuilder};

pub mod attack;
pub mod attackers;
pub mod block_actions;
pub mod entity_actions;
pub mod block_update;
pub mod brand;
#[cfg(feature = "online-mode")]
pub mod chat_signing;
pub mod chunks;
pub mod client_chat;
pub mod client_information;
pub mod connection;
pub mod cookies;
pub mod disconnect;
pub mod interact;
pub mod interpolation;
pub mod item_use;
pub mod inventory;
pub mod join;
pub mod loading;
pub mod login;
pub mod mining;
pub mod movement;
pub mod packet;
pub mod pong;
pub mod pushes;
pub mod respawn;
pub mod tags;
pub mod task_pool;
pub mod tick_counter;
pub mod tick_clock;
pub mod tick_end;
pub mod unmodelled_disconnect;

/// This plugin group will add all the default plugins necessary for Azalea to
/// work.
pub struct DefaultPlugins;

impl PluginGroup for DefaultPlugins {
    fn build(self) -> PluginGroupBuilder {
        #[allow(unused_mut)]
        let mut group = PluginGroupBuilder::start::<Self>()
            .add(crate::client::AmbiguityLoggerPlugin)
            .add(bevy_time::TimePlugin)
            .add(packet::PacketPlugin)
            .add(crate::client::AzaleaPlugin)
            .add(azalea_entity::EntityPlugin)
            .add(azalea_physics::PhysicsPlugin)
            .add(task_pool::TaskPoolPlugin::default())
            .add(inventory::InventoryPlugin)
            .add(client_chat::ChatPlugin)
            .add(disconnect::DisconnectPlugin)
            .add(movement::MovementPlugin)
            .add(interact::InteractPlugin)
            .add(respawn::RespawnPlugin)
            .add(mining::MiningPlugin)
            .add(block_actions::BlockActionsPlugin)
            .add(entity_actions::EntityActionsPlugin)
            .add(attack::AttackPlugin)
            .add(attackers::AttackersPlugin)
            .add(interpolation::InterpolationPlugin)
            .add(pushes::PushesPlugin)
            .add(item_use::ItemUsePlugin)
            .add(chunks::ChunksPlugin)
            .add(block_update::BlockUpdatePlugin)
            .add(tick_end::TickEndPlugin)
            .add(loading::PlayerLoadedPlugin)
            .add(brand::BrandPlugin)
            .add(client_information::ClientInformationPlugin)
            .add(tick_counter::TickCounterPlugin)
            .add(unmodelled_disconnect::UnmodelledDisconnectPlugin)
            .add(pong::PongPlugin)
            .add(connection::ConnectionPlugin)
            .add(login::LoginPlugin)
            .add(join::JoinPlugin)
            .add(cookies::CookiesPlugin);
        #[cfg(feature = "online-mode")]
        {
            group = group.add(chat_signing::ChatSigningPlugin);
        }
        #[cfg(feature = "log")]
        {
            group = group.add(bevy_log::LogPlugin::default());
        }
        group
    }
}
