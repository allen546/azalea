//! The server's tags, kept so other code can resolve a tag that a component names (a shield's
//! `bypassed_by`). The server sends them during configuration and again after `/reload`; each
//! packet replaces the registries it lists (vanilla `ClientPacketListener#handleUpdateTags`).

use std::collections::HashMap;

use azalea_protocol::common::tags::TagMap;
use azalea_registry::identifier::Identifier;
use bevy_ecs::prelude::*;

/// Tag contents per registry, as the server sent them: protocol ids into that registry.
#[derive(Clone, Component, Debug, Default)]
pub struct ServerTags {
    pub registries: HashMap<Identifier, HashMap<Identifier, Vec<u32>>>,
}

impl ServerTags {
    /// The protocol ids in `tag` of `registry`, or None if the server sent no such tag.
    pub fn get(&self, registry: &Identifier, tag: &Identifier) -> Option<&[u32]> {
        self.registries.get(registry)?.get(tag).map(Vec::as_slice)
    }

    fn apply(&mut self, tags: &TagMap) {
        for (registry, list) in &tags.0 {
            let map = list.iter().map(|t| (t.name.clone(), t.elements.iter().map(|&i| i as u32).collect())).collect();
            self.registries.insert(registry.clone(), map);
        }
    }
}

pub(crate) fn handle_update_tags(ecs: &mut World, player: Entity, tags: &TagMap) {
    let Ok(mut e) = ecs.get_entity_mut(player) else { return };
    if let Some(mut t) = e.get_mut::<ServerTags>() {
        t.apply(tags);
    } else {
        let mut t = ServerTags::default();
        t.apply(tags);
        e.insert(t);
    }
}
