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

// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2 (patch 46: the attack key held while mining, and the
// discrete actions the player did)
#[test]
fn test_input_log_actions() {
    use azalea_client::{
        inventory::{ContainerClickEvent, OpenInventoryEvent},
        item_use::SwapOffhandQueued,
        mining::StartMiningBlockEvent,
    };
    use azalea_inventory::operations::{ClickOperation, PickupClick};

    let _lock = init();
    let dir = std::env::temp_dir().join(format!("null-input-log-actions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut s = Simulation::new(ConnectionProtocol::Game);
    s.app.insert_resource(InputLogDir(dir.clone()));
    s.receive_packet(default_login_packet());
    s.receive_packet(make_basic_empty_chunk(ChunkPos::new(0, 0), (384 + 64) / 16));
    let stone = BlockPos::new(0, 2, 0);
    s.receive_packet(ClientboundBlockUpdate { pos: stone, block_state: BlockKind::Stone.into() });
    s.receive_packet(ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: stone.up(1).center_bottom(),
            delta: Vec3::ZERO,
            look_direction: LookDirection::default(),
        },
        relative: RelativeMovements::all_absolute(),
    });
    s.tick();
    s.with_component_mut::<LookDirection>(|mut look| look.update_x_rot(90.));
    s.tick();
    s.tick();
    s.tick();
    input_log::mark("azalea", "idle-end");
    s.tick();
    input_log::mark("azalea", "mining");
    s.write_message(StartMiningBlockEvent { entity: s.entity, position: stone, force: false });
    for _ in 0..3 {
        s.tick();
    }
    input_log::mark("azalea", "swap");
    let e = s.entity;
    s.app.world_mut().entity_mut(e).insert(SwapOffhandQueued);
    s.tick();
    input_log::mark("azalea", "stop");
    s.app.world_mut().entity_mut(e).insert(azalea_client::block_actions::StopMiningQueued);
    s.tick();
    s.tick();
    input_log::mark("azalea", "screen");
    s.trigger(OpenInventoryEvent { entity: e });
    s.tick();
    s.tick();
    s.trigger(ContainerClickEvent {
        entity: e,
        window_id: 0,
        operation: ClickOperation::Pickup(PickupClick::Left { slot: Some(36) }),
    });
    s.tick();

    let text = std::fs::read_to_string(dir.join("azalea.jsonl")).expect("the log file exists");
    let _ = std::fs::remove_dir_all(&dir);
    // The lines between two marks.
    let part = |from: &str, to: Option<&str>| -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        let a = lines.iter().position(|l| l.starts_with(&format!("{{\"mark\":\"{from}\""))).unwrap();
        let b = to
            .and_then(|t| lines.iter().position(|l| l.starts_with(&format!("{{\"mark\":\"{t}\""))))
            .unwrap_or(lines.len());
        lines[a + 1..b].iter().map(|l| l.to_string()).collect()
    };
    let held = |lines: &[String]| {
        lines.iter().filter(|l| l.starts_with("{\"tick\"")).map(|l| l.contains("\"attack\":true")).collect::<Vec<_>>()
    };
    // Nothing held before the break; held from the tick it starts; no events from mining.
    assert!(held(&part("idle-end", Some("mining"))).iter().all(|h| !h), "{text}");
    let mining = part("mining", Some("swap"));
    assert_eq!(held(&mining), [true, true, true], "{text}");
    assert!(mining.iter().all(|l| !l.contains("\"ev\"")), "{text}");
    // The swap sits before the sample of its tick and holds no key.
    let swap = part("swap", Some("stop"));
    assert!(swap[0].starts_with("{\"ev\":\"swap_offhand\",\"tick\":"), "{text}");
    // The abort lets the key go: not held from the tick after the stop on.
    assert_eq!(held(&part("stop", Some("screen"))), [false, false], "{text}");
    // The screen opens at a tick; the click is a later one and names its window, slot and type.
    let screen = part("screen", None);
    assert!(screen[0].starts_with("{\"ev\":\"open_inventory\""), "{text}");
    let click = screen.iter().find(|l| l.contains("\"ev\":\"click\"")).expect("a click line");
    assert!(click.contains("\"win\":0,\"slot\":36,\"button\":0,\"type\":0"), "{text}");
    assert!(screen.iter().position(|l| l == click) > Some(2), "{text}");
}
