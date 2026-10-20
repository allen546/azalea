use azalea_client::{interact::pick::HitResultComponent, test_utils::prelude::*};
use azalea_core::{
    position::{BlockPos, ChunkPos, Vec3},
    tick::GameTick,
};
use azalea_entity::LookDirection;
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockUpdate, ClientboundPlayerPosition},
    },
};
use azalea_registry::builtin::BlockKind;
use bevy_ecs::prelude::*;

#[derive(Resource, Default)]
struct Seen(Vec<(f32, bool)>);

// Stand-in for the look planner: turns the look down on even ticks and level on odd ticks.
fn turn(mut query: Query<&mut LookDirection>, mut n: Local<u32>) {
    for mut look in &mut query {
        look.update_x_rot(if *n % 2 == 0 { 90. } else { 0. });
    }
    *n += 1;
}

fn record(query: Query<(&LookDirection, &HitResultComponent)>, mut seen: ResMut<Seen>) {
    for (look, hit) in &query {
        seen.0.push((look.x_rot(), hit.miss()));
    }
}

// Spec: specs/F5-version-layer.md §4.6 (What Null computes: the crosshair ray is cast once per tick)
#[test]
fn test_pick_every_tick() {
    let _lock = init();

    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.receive_packet(default_login_packet());
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    simulation.tick();

    let floor = BlockPos::new(0, 2, 0);
    simulation.receive_packet(ClientboundBlockUpdate {
        pos: floor,
        block_state: BlockKind::Stone.into(),
    });
    simulation.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: floor.up(1).center_bottom(),
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    simulation.tick();
    simulation.tick();

    simulation.app.init_resource::<Seen>();
    simulation.app.add_systems(
        GameTick,
        (
            turn.before(azalea_client::interact::pick::update_hit_result_component)
                .ambiguous_with_all(),
            record
                .after(azalea_client::interact::pick::update_hit_result_component)
                .ambiguous_with_all(),
        ),
    );

    // One update, then four GameTicks back to back (a catch-up after a stall).
    simulation.app.update();
    for _ in 0..4 {
        simulation.app.world_mut().run_schedule(GameTick);
    }

    let seen = simulation.app.world().resource::<Seen>().0.clone();
    assert_eq!(seen.len(), 4);
    for (i, (pitch, miss)) in seen.iter().enumerate() {
        // pitch 90 looks at the floor under the feet; pitch 0 looks at empty air
        assert_eq!(*pitch, if i % 2 == 0 { 90. } else { 0. });
        assert_eq!(*miss, i % 2 == 1, "tick {i}: hit does not match the look");
    }
}
