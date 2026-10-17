use azalea_client::{
    attack::{AttackQueued, AttackStrengthScale, TicksSinceLastAttack},
    test_utils::prelude::*,
};
use azalea_core::entity_id::MinecraftEntityId;
use azalea_entity::indexing::EntityIdIndex;
use azalea_protocol::packets::ConnectionProtocol;
use azalea_registry::builtin::EntityKind;

/// Fists: attack speed 4, so the delay is 5 ticks and the scale is (ticks + 0.5) / 5.
#[test]
fn cooldown_after_an_attack_matches_the_servers_count() {
    let _lock = init();
    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.receive_packet(default_login_packet());
    s.tick();
    s.receive_packet(make_basic_add_entity(EntityKind::Zombie, 123, (1., 64., 0.)));
    s.tick();
    for _ in 0..8 {
        s.tick();
    }
    assert_eq!(s.component::<AttackStrengthScale>().0, 1.);

    let target = s
        .query_self::<&EntityIdIndex, _>(|i| i.get_by_minecraft_entity(MinecraftEntityId(123)))
        .expect("zombie known");
    let e = s.entity;
    s.app.world_mut().entity_mut(e).insert(AttackQueued { target });
    s.tick();
    // The tick of the attack already reads 1 tick, as the server's counter does.
    assert_eq!(s.component::<TicksSinceLastAttack>().0, 1);
    assert!((s.component::<AttackStrengthScale>().0 - 0.3).abs() < 1e-6);
    s.tick();
    assert_eq!(s.component::<TicksSinceLastAttack>().0, 2);
    assert!((s.component::<AttackStrengthScale>().0 - 0.5).abs() < 1e-6);
}
