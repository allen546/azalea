use azalea_client::{
    respawn::{DeathScreen, PerformRespawnEvent, RespawnLog},
    test_utils::prelude::*,
};
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
    assert_eq!(respawns(&sent), 1);
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
