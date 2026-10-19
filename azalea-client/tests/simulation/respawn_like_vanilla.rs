use azalea_registry::DataRegistry;
use azalea_client::{
    ClientMovementState, WalkDirection,
    attack::AttackQueued,
    block_actions::{BlockActionRefusal, BlockActionResults, BreakBlockQueued, PlaceBlockQueued},
    inventory::OpenScreen,
    item_use::{SwapOffhandQueued, UseItemQueued},
    respawn::{DeadAction, DeathScreen, PerformRespawnEvent, RespawnLog},
    test_utils::prelude::*,
};
use azalea_core::direction::Direction;
use azalea_core::position::BlockPos;
use azalea_entity::{Jumping, LookDirection, inventory::Inventory, metadata::Health};
use azalea_protocol::packets::game::{ClientboundSetHealth, s_interact::InteractionHand};
use azalea_chat::FormattedText;
use azalea_protocol::packets::{
    ConnectionProtocol,
    game::{
        ClientboundGameEvent, ClientboundPlayerCombatKill, ServerboundGamePacket,
        c_game_event::EventType, s_client_command::Action,
    },
};

fn setup() -> (Simulation, SentPackets) {
    let mut s = Simulation::new(ConnectionProtocol::Game);
    let sent = SentPackets::new(&mut s);
    s.receive_packet(default_login_packet());
    s.tick();
    (s, sent)
}

fn kill(s: &mut Simulation) {
    s.receive_packet(ClientboundPlayerCombatKill {
        player_id: s.minecraft_entity_id(),
        message: FormattedText::from("Bot was killed"),
    });
    s.tick();
}

fn respawns(sent: &SentPackets) -> usize {
    sent.list
        .lock()
        .iter()
        .filter(|p| matches!(p, ServerboundGamePacket::ClientCommand(c) if c.action == Action::PerformRespawn))
        .count()
}

fn ask(s: &mut Simulation) {
    let entity = s.entity;
    s.app.world_mut().write_message(PerformRespawnEvent { entity });
}

#[test]
fn a_request_waits_for_the_twentieth_screen_tick() {
    let _lock = init();
    let (mut s, sent) = setup();
    kill(&mut s);
    assert!(s.has_component::<DeathScreen>());
    assert_eq!(s.component::<RespawnLog>().deaths, 1);
    sent.clear();
    ask(&mut s);
    let ticks_so_far = s.component::<DeathScreen>().ticks;
    for _ in ticks_so_far..19 {
        s.tick();
    }
    assert_eq!(respawns(&sent), 0, "held until the button is active");
    s.tick();
    assert_eq!(respawns(&sent), 1);
    for _ in 0..30 {
        s.tick();
    }
    assert_eq!(respawns(&sent), 1, "one packet per death screen");
    assert_eq!(s.component::<RespawnLog>().last_respawn.as_ref().unwrap().screen_ticks, 20);
}

#[test]
fn immediate_respawn_sends_at_once_without_a_request() {
    let _lock = init();
    let (mut s, sent) = setup();
    s.receive_packet(ClientboundGameEvent { event: EventType::ImmediateRespawn, param: 1.0 });
    s.tick();
    sent.clear();
    kill(&mut s);
    // the kill packet's handler, then the tick (see the repeat test below)
    assert_eq!(respawns(&sent), 2);
    let r = s.component::<RespawnLog>().last_respawn.clone().unwrap();
    assert!(r.immediate);
    assert_eq!(r.screen_ticks, 0);
}

#[test]
fn a_request_while_alive_is_refused_and_sends_nothing() {
    let _lock = init();
    let (mut s, sent) = setup();
    sent.clear();
    ask(&mut s);
    s.tick();
    assert_eq!(respawns(&sent), 0);
    assert!(s.component::<RespawnLog>().last_refusal.is_some());
}

fn names_sent(sent: &SentPackets) -> Vec<String> {
    sent.list
        .lock()
        .iter()
        .map(|p| format!("{p:?}").split(['(', ' ', '{']).next().unwrap().to_string())
        .collect()
}

fn count(sent: &SentPackets, name: &str) -> usize {
    names_sent(sent).iter().filter(|n| *n == name).count()
}

// Patch 22: `Minecraft#handleKeybinds` does not run with the death screen open.
#[test]
fn input_is_refused_and_sends_nothing_while_dead() {
    let _lock = init();
    let (mut s, sent) = setup();
    kill(&mut s);
    sent.clear();
    let e = s.entity;
    let eid = s.minecraft_entity_id();
    let _ = eid;
    {
        let mut w = s.app.world_mut().entity_mut(e);
        w.insert((
            AttackQueued { target: e },
            UseItemQueued { hand: InteractionHand::MainHand },
            SwapOffhandQueued,
            BreakBlockQueued { position: BlockPos::new(0, 60, 0) },
            PlaceBlockQueued { position: BlockPos::new(0, 60, 0), direction: Direction::Up, auto_crouch: false },
        ));
        let mut mv = w.get_mut::<ClientMovementState>().unwrap();
        mv.move_direction = WalkDirection::Forward;
        mv.trying_to_sprint = true;
        mv.trying_to_crouch = true;
        **w.get_mut::<Jumping>().unwrap() = true;
        let mut look = w.get_mut::<LookDirection>().unwrap();
        *look = LookDirection::new(90., 10.);
        w.get_mut::<Inventory>().unwrap().selected_hotbar_slot = 4;
    }
    s.tick();
    let log = s.component::<RespawnLog>();
    for action in [
        DeadAction::Attack,
        DeadAction::UseItem,
        DeadAction::SwapOffhand,
        DeadAction::BreakBlock,
        DeadAction::PlaceBlock,
        DeadAction::Walk,
        DeadAction::Sprint,
        DeadAction::Sneak,
        DeadAction::Jump,
        DeadAction::Look,
        DeadAction::SelectSlot,
    ] {
        assert!(log.refused_while_dead.iter().any(|(a, _)| *a == action), "{action:?} not refused");
    }
    let results = s.component::<BlockActionResults>();
    assert_eq!(results.last_place.unwrap().refusal, Some(BlockActionRefusal::Dead));
    assert_eq!(results.last_break.unwrap().refusal, Some(BlockActionRefusal::Dead));
    s.with_component(|inv: &Inventory| assert_eq!(inv.selected_hotbar_slot, 0));
    s.with_component(|l: &LookDirection| assert_eq!(*l, LookDirection::default()));
    for name in ["Swing", "Interact", "UseItem", "UseItemOn", "PlayerAction", "SetCarriedItem", "ContainerClick"] {
        assert_eq!(count(&sent, name), 0, "{name} went out while dead: {:?}", names_sent(&sent));
    }
    assert_eq!(count(&sent, "PlayerCommand"), 0, "no sprint start: {:?}", names_sent(&sent));
}

// `Minecraft#tick` -> `setScreen(null)` -> `LocalPlayer#respawn` runs every tick while dead with no
// screen; the kill packet's handler sent the first.
#[test]
fn immediate_respawn_repeats_every_tick_until_the_respawn() {
    let _lock = init();
    let (mut s, sent) = setup();
    s.receive_packet(ClientboundGameEvent { event: EventType::ImmediateRespawn, param: 1.0 });
    s.tick();
    sent.clear();
    kill(&mut s);
    assert_eq!(respawns(&sent), 2, "the packet handler, then the tick");
    for _ in 0..3 {
        s.tick();
    }
    assert_eq!(respawns(&sent), 5);
    assert_eq!(s.component::<RespawnLog>().last_respawn.unwrap().sends, 5);
}

// 0 health with no kill packet: only `Minecraft#tick` notices, so the first send is in the tick.
#[test]
fn zero_health_with_immediate_respawn_sends_from_the_tick() {
    let _lock = init();
    let (mut s, sent) = setup();
    s.receive_packet(ClientboundGameEvent { event: EventType::ImmediateRespawn, param: 1.0 });
    s.tick();
    sent.clear();
    s.receive_packet(ClientboundSetHealth { health: 0., food: 20, saturation: 5. });
    s.tick();
    assert!(s.has_component::<DeathScreen>());
    assert_eq!(respawns(&sent), 1);
    s.tick();
    assert_eq!(respawns(&sent), 2);
}

// `AbstractContainerScreen#tick` closes a screen of a dead player, and sends the close.
#[test]
fn zero_health_closes_the_open_screen_with_a_packet() {
    let _lock = init();
    let (mut s, sent) = setup();
    let e = s.entity;
    s.app.world_mut().entity_mut(e).insert(OpenScreen::default());
    sent.clear();
    s.receive_packet(ClientboundSetHealth { health: 0., food: 20, saturation: 5. });
    s.tick();
    assert!(s.has_component::<DeathScreen>());
    assert!(!s.has_component::<OpenScreen>());
    assert_eq!(count(&sent, "ContainerClose"), 1, "{:?}", names_sent(&sent));
    let _ = Health(0.);
}

// `ClientPacketListener#handleRespawn` makes a new player for every respawn packet.
#[test]
fn a_dimension_respawn_starts_a_new_inventory() {
    let _lock = init();
    let (mut s, _sent) = setup();
    s.with_component_mut(|inv: &mut Inventory| inv.selected_hotbar_slot = 5);
    s.receive_packet(make_basic_respawn_packet(
        azalea_registry::data::DimensionKind::new_raw(0),
        azalea_registry::identifier::Identifier::new("minecraft:overworld"),
    ));
    s.tick();
    s.with_component(|inv: &Inventory| assert_eq!(inv.selected_hotbar_slot, 0));
}
