use azalea_auth::game_profile::GameProfile;
use azalea_chat::FormattedText;
use azalea_core::game_type::GameMode;
use bevy_ecs::{
    component::Component,
    message::MessageReader,
    system::{Commands, Query},
};
use derive_more::{Deref, DerefMut};
use uuid::Uuid;

use crate::{local_player::WorldHolder, packet::game::AddPlayerEvent};

/// A player in the tab list.
#[derive(Clone, Debug)]
pub struct PlayerInfo {
    /// Information about the player's Minecraft account, including their
    /// username.
    pub profile: GameProfile,
    /// The player's UUID.
    pub uuid: Uuid,
    /// The current gamemode of the player, like survival or creative.
    pub gamemode: GameMode,
    /// The player's latency in milliseconds.
    ///
    /// The bars in the tab screen depend on this.
    pub latency: i32,
    /// The player's display name in the tab list, but only if it's different
    /// from the player's normal username.
    ///
    /// Use [`GameProfile::name`] to get the player's actual username.
    pub display_name: Option<Box<FormattedText>>,
}

/// A component only present in players that contains the [`GameProfile`] (which
/// you can use to get a player's name).
///
/// Note that it's possible for this to be missing in a player if the server
/// never sent the player info for them (though this is uncommon).
#[derive(Clone, Component, Debug, Deref, DerefMut)]
pub struct GameProfileComponent(pub GameProfile);

/// Add a [`GameProfileComponent`] when an [`AddPlayerEvent`] is received.
/// Usually the `GameProfileComponent` will be added from the
/// `ClientboundGamePacket::AddPlayer` handler though.
// Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 1
pub fn retroactively_add_game_profile_component(
    mut commands: Commands,
    mut events: MessageReader<AddPlayerEvent>,
    holders: Query<&WorldHolder>,
) {
    for event in events.read() {
        // Look in the world of the bot that got the packet, never in another bot's world.
        let Ok(holder) = holders.get(event.entity) else {
            continue;
        };
        let found = holder.shared.read().entity_by_uuid.get(&event.info.uuid).copied();
        if let Some(entity) = found {
            commands
                .entity(entity)
                .insert(GameProfileComponent(event.info.profile.clone()));
        }
    }
}
