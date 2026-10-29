use azalea_client::{
    ClientMovementState, input_log, input_log::InputLogDir, test_utils::prelude::*,
};
use azalea_core::position::{BlockPos, ChunkPos, Vec3};
use azalea_entity::LookDirection;
use azalea_physics::client_movement::WalkDirection;
use azalea_protocol::{
    common::movements::{PositionMoveRotation, RelativeMovements},
    packets::{
        ConnectionProtocol,
        game::{ClientboundBlockUpdate, ClientboundPlayerPosition},
    },
};
use azalea_registry::builtin::BlockKind;

// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD1 (the input log: one line per
// client tick, the input the physics step used)
#[test]
fn test_input_log_one_line_per_tick() {
    let _lock = init();
    let dir = std::env::temp_dir().join(format!("null-input-log-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut simulation = Simulation::new(ConnectionProtocol::Game);
    simulation.app.insert_resource(InputLogDir(dir.clone()));
    simulation.receive_packet(default_login_packet());
    simulation.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    simulation.receive_packet(ClientboundBlockUpdate {
        pos: BlockPos::new(0, 119, 0),
        block_state: BlockKind::Stone.into(),
    });
    simulation.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: Vec3::new(0.5, 120., 0.5),
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    simulation.tick();
    simulation.tick();

    input_log::mark("azalea", "row-start");
    simulation.with_component_mut::<ClientMovementState>(|mut m| {
        m.move_direction = WalkDirection::Forward;
        m.trying_to_sprint = true;
    });
    simulation.tick();
    simulation.tick();
    simulation
        .with_component_mut::<ClientMovementState>(|mut m| m.move_direction = WalkDirection::None);
    simulation.tick();

    let text = std::fs::read_to_string(dir.join("azalea.jsonl")).expect("the log file exists");
    let _ = std::fs::remove_dir_all(&dir);
    let lines: Vec<&str> = text.lines().collect();
    let samples: Vec<&&str> = lines
        .iter()
        .filter(|l| l.starts_with("{\"tick\""))
        .collect();
    // One sample per tick, numbered from 0 without a gap.
    for (i, l) in samples.iter().enumerate() {
        assert!(l.starts_with(&format!("{{\"tick\":{i},")), "line {i}: {l}");
    }
    let n = samples.len();
    assert!(n >= 5, "{text}");
    // The last three ticks: walking with sprint twice, then released.
    assert!(
        samples[n - 3].contains("\"f\":true") && samples[n - 3].contains("\"sprint\":true"),
        "{text}"
    );
    assert!(samples[n - 2].contains("\"f\":true"), "{text}");
    assert!(samples[n - 1].contains("\"f\":false"), "{text}");
    // The mark sits right before the first walking tick and names its number.
    let at = lines
        .iter()
        .position(|l| l.starts_with("{\"mark\":\"row-start\""))
        .expect("mark line");
    assert_eq!(
        lines[at],
        format!("{{\"mark\":\"row-start\",\"tick\":{}}}", n - 3)
    );
    assert!(lines[at + 1].contains("\"f\":true"));
}
