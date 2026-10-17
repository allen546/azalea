use azalea_client::{attackers::RecentAttackers, test_utils::prelude::*};
use azalea_core::entity_id::MinecraftEntityId;
use azalea_protocol::packets::{
    ConnectionProtocol,
    game::{
        ClientboundDamageEvent, ClientboundHurtAnimation, c_damage_event::OptionalEntityId,
    },
};

#[test]
fn damage_event_names_the_attacker() {
    let _lock = init();
    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.receive_packet(default_login_packet());
    s.tick();
    let me = s.minecraft_entity_id();

    // Melee by entity 7, then an arrow (entity 9) shot by entity 7, then a hit on someone else.
    for (cause, direct, target) in [(Some(7), Some(7), me), (Some(7), Some(9), me), (Some(3), Some(3), MinecraftEntityId(99))] {
        s.receive_packet(ClientboundDamageEvent {
            entity_id: target,
            source_type_id: 4,
            source_cause_id: OptionalEntityId(cause),
            source_direct_id: OptionalEntityId(direct),
            source_position: None,
        });
        s.tick();
    }
    s.receive_packet(ClientboundHurtAnimation { id: me, yaw: 90. });
    s.tick();

    let r = s.component::<RecentAttackers>();
    assert_eq!(r.hits.len(), 2);
    assert_eq!(r.hits[0].attacker(), Some(MinecraftEntityId(7)));
    assert_eq!(r.hits[1].cause, Some(MinecraftEntityId(7)));
    assert_eq!(r.hits[1].direct, Some(MinecraftEntityId(9)));
    assert!(r.hits[1].tick > r.hits[0].tick);
    assert_eq!(r.last_hurt.map(|h| h.1), Some(90.));
    assert_eq!(r.within(3).count(), 1);
    assert_eq!(r.within(50).count(), 2);
}
