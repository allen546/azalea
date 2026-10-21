use azalea_client::{
    client_refusals::InputRefusal,
    respawn::RespawnLog,
    test_utils::prelude::*,
    unmodelled_disconnect::{DisconnectOnUnmodelledMovement, UnmodelledDisconnect},
};
use azalea_core::position::{BlockPos, ChunkPos, Vec3};
use azalea_entity::{Jumping, LookDirection, inventory::Inventory, metadata::FallFlying, unmodelled::UnmodelledInput};
use azalea_inventory::ItemStack;
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockUpdate, ClientboundPlayerPosition, ServerboundGamePacket},
    },
};
use azalea_registry::builtin::{BlockKind, ItemKind};

// Spec: specs/F5-version-layer.md §4.6 (client refusals: elytra, gliding set by the server)

fn setup(height: i32) -> (Simulation, SentPackets) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    s.tick();
    s.receive_packet(ClientboundBlockUpdate { pos: BlockPos::new(0, 2, 0), block_state: BlockKind::Stone.into() });
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: BlockPos::new(0, 3 + height, 0).center_bottom(),
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    for _ in 0..4 {
        s.tick();
    }
    (s, sent)
}

fn wear(s: &mut Simulation, stack: ItemStack) {
    s.with_component_mut::<Inventory>(|mut inv| {
        inv.inventory_menu.as_player_mut().armor[1] = stack;
    });
}

fn refused(s: &Simulation) -> Vec<(InputRefusal, u32)> {
    s.component::<RespawnLog>().refused_inputs
}

fn no_player_command(sent: &SentPackets) {
    assert!(
        !sent.list.lock().iter().any(|p| matches!(p, ServerboundGamePacket::PlayerCommand(_))),
        "a player command went out: {:?}",
        sent.list.lock()
    );
}

#[test]
fn a_fresh_jump_in_the_air_with_an_elytra_is_refused_and_nothing_is_sent() {
    let _lock = init();
    let (mut s, sent) = setup(20);
    wear(&mut s, ItemStack::from(ItemKind::Elytra));
    sent.clear();
    s.with_component_mut::<Jumping>(|mut j| **j = true);
    s.tick();
    assert!(!*s.component::<Jumping>(), "the jump should be cleared");
    assert_eq!(refused(&s), vec![(InputRefusal::ElytraStart, 1)]);
    no_player_command(&sent);
    assert!(!s.component::<FallFlying>().0, "no glide");
}

#[test]
fn a_held_jump_is_not_a_new_edge() {
    let _lock = init();
    let (mut s, sent) = setup(20);
    // jump without a glider: not refused, and the key state is remembered
    s.with_component_mut::<Jumping>(|mut j| **j = true);
    s.tick();
    assert!(*s.component::<Jumping>());
    wear(&mut s, ItemStack::from(ItemKind::Elytra));
    sent.clear();
    s.tick();
    assert!(*s.component::<Jumping>(), "a held jump stays");
    assert!(refused(&s).is_empty());
    no_player_command(&sent);
}

#[test]
fn a_broken_elytra_is_not_refused() {
    let _lock = init();
    let (mut s, _sent) = setup(20);
    wear(
        &mut s,
        ItemStack::from(ItemKind::Elytra).with_component(azalea_inventory::components::Damage { amount: 431 }),
    );
    s.with_component_mut::<Jumping>(|mut j| **j = true);
    s.tick();
    assert!(*s.component::<Jumping>());
    assert!(refused(&s).is_empty());
}

#[test]
fn on_the_ground_the_jump_is_not_refused() {
    let _lock = init();
    let (mut s, _sent) = setup(0);
    wear(&mut s, ItemStack::from(ItemKind::Elytra));
    s.with_component_mut::<Jumping>(|mut j| **j = true);
    s.tick();
    assert!(refused(&s).is_empty());
}

#[test]
fn the_server_setting_the_glide_flag_disconnects() {
    let _lock = init();
    let (mut s, _sent) = setup(0);
    let e = s.entity;
    s.app.world_mut().entity_mut(e).insert(DisconnectOnUnmodelledMovement);
    s.with_component_mut::<FallFlying>(|mut f| f.0 = true);
    s.tick();
    s.update();
    assert!(s.has_component::<UnmodelledDisconnect>());
    assert!(s.component::<UnmodelledDisconnect>().inputs.contains(&UnmodelledInput::Gliding));
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: the right-click bypass is closed)

fn spawn(s: &mut Simulation, id: i32, kind: azalea_registry::builtin::EntityKind) -> bevy_ecs::entity::Entity {
    s.receive_packet(azalea_protocol::packets::game::ClientboundAddEntity {
        id: id.into(),
        uuid: uuid::Uuid::new_v4(),
        entity_type: kind,
        position: Vec3::new(0.5, 4.0, 2.5),
        x_rot: 0,
        y_rot: 0,
        y_head_rot: 0,
        data: 0,
        movement: Default::default(),
    });
    s.tick();
    let mut found = None;
    s.with_component::<azalea_entity::indexing::EntityIdIndex>(|ids| {
        found = ids.get_by_minecraft_entity(azalea_core::entity_id::MinecraftEntityId(id));
    });
    found.expect("entity spawned")
}

fn interacts(sent: &SentPackets) -> usize {
    sent.list.lock().iter().filter(|p| matches!(p, ServerboundGamePacket::Interact(_))).count()
}

#[test]
fn the_entity_interact_event_sends_nothing_for_a_click_that_mounts() {
    use azalea_registry::builtin::EntityKind as K;
    let _lock = init();
    let (mut s, sent) = setup(0);
    let client = s.entity;
    for (id, kind) in [(50, K::OakBoat), (51, K::Horse), (52, K::Pig), (53, K::Camel), (54, K::OakChestBoat)] {
        let target = spawn(&mut s, id, kind);
        sent.clear();
        s.trigger(azalea_client::interact::EntityInteractEvent { client, target, location: None });
        s.tick();
        assert_eq!(interacts(&sent), 0, "{kind:?} was clicked");
    }
    // a container minecart is clicked as before (positive control)
    let target = spawn(&mut s, 60, K::ChestMinecart);
    sent.clear();
    s.trigger(azalea_client::interact::EntityInteractEvent { client, target, location: None });
    s.tick();
    assert_eq!(interacts(&sent), 2, "a chest minecart click sends INTERACT_AT and INTERACT");
}

// Spec: specs/F5-version-layer.md §4.6 (client refusals: riptide and item use speed)

fn hold(s: &mut Simulation, stack: ItemStack) {
    s.receive_packet(azalea_protocol::packets::game::ClientboundContainerSetSlot {
        container_id: 0,
        state_id: 1,
        slot: *azalea_inventory::Player::HOTBAR_SLOTS.start() as u16,
        item_stack: stack,
    });
    s.tick();
}

fn use_item_count(sent: &SentPackets) -> usize {
    sent.list.lock().iter().filter(|p| matches!(p, ServerboundGamePacket::UseItem(_))).count()
}

#[test]
fn a_use_with_other_use_effects_is_refused_and_a_plain_one_is_sent() {
    use azalea_client::item_use::UseItemQueued;
    use azalea_protocol::packets::game::s_interact::InteractionHand;
    let _lock = init();
    let (mut s, sent) = setup(0);
    let queue = |s: &mut Simulation| {
        let e = s.entity;
        s.app.world_mut().entity_mut(e).insert(UseItemQueued { hand: InteractionHand::MainHand });
        s.tick();
    };
    // a spear (speed 1.0, sprint) and an item given a custom speed
    hold(&mut s, ItemStack::from(ItemKind::IronSpear));
    sent.clear();
    queue(&mut s);
    assert_eq!(use_item_count(&sent), 0, "spear use sent");
    hold(
        &mut s,
        ItemStack::from(ItemKind::Apple).with_component(azalea_inventory::components::UseEffects {
            can_sprint: false,
            interact_vibrations: true,
            speed_multiplier: 1.0,
        }),
    );
    queue(&mut s);
    assert_eq!(use_item_count(&sent), 0, "custom speed use sent");
    assert_eq!(refused(&s), vec![(InputRefusal::UseEffects, 2)]);
    // a plain trident (strength 0 with no enchantments) and a plain apple are sent
    hold(&mut s, ItemStack::from(ItemKind::Trident));
    queue(&mut s);
    assert_eq!(use_item_count(&sent), 1, "a plain trident is thrown by the server alone");
    hold(&mut s, ItemStack::from(ItemKind::Apple));
    queue(&mut s);
    assert_eq!(use_item_count(&sent), 2);
}
