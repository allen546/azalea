//! Null fork: records the bot's input per client tick, for the vanilla diff
//! (VD1). It changes nothing that is sent. Off unless the app holds an
//! [`InputLogDir`] resource.
//!
//! One JSON line per client tick,
//! `{"tick":N,"f":..,"b":..,"l":..,"r":..,"jump":..,"sneak":.., "sprint":..,"
//! yaw":..,"pitch":..,"slot":..}`. `N` counts the client ticks of the
//! connection from 0, the same way the vanilla-diff tap counts serverbound
//! `ClientTickEnd` packets, so the two logs align. The line is written where
//! vanilla's `LocalPlayer#aiStep` has just read the keys (`KeyboardInput#
//! tick`): after the screen hold and `tick_controls`, before the sprint
//! decision and the physics step, so it is the input the step used. [`mark`]
//! adds `{"mark":"..","tick":N}` lines, where `N` is the tick of the next
//! sample.

use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

use azalea_core::tick::GameTick;
use azalea_entity::{Jumping, LocalEntity, LookDirection, inventory::Inventory};
use azalea_physics::{PhysicsSystems, client_movement::ClientMovementState};
use azalea_world::WorldName;
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::{
    movement::{local_player_ai_step, tick_controls},
    player::GameProfileComponent,
};

/// Turns the log on: one file per connection is written into this directory.
#[derive(Clone, Debug, Resource)]
pub struct InputLogDir(pub PathBuf);

/// Marks asked for by other threads (the ability check): (player name, label).
/// The next sample of that player's log takes them.
static MARKS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Asks the log of `player` to write a mark line before its next sample. A
/// no-op when the log is off.
// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD1
pub fn mark(player: &str, label: &str) {
    MARKS
        .lock()
        .unwrap()
        .push((player.to_owned(), label.to_owned()));
}

/// What a player did in one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputSample {
    pub forward: bool,
    pub backward: bool,
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub sneak: bool,
    pub sprint: bool,
    pub yaw: f32,
    pub pitch: f32,
    pub slot: u8,
}

/// The log file of one connection.
#[derive(Component)]
pub struct InputLog {
    out: Box<dyn Write + Send + Sync>,
    tick: u64,
}

impl InputLog {
    pub fn new(out: Box<dyn Write + Send + Sync>) -> Self {
        Self { out, tick: 0 }
    }

    /// Writes the mark lines, then the sample line for the next tick.
    // Spec: specs/plans/VANILLA_DIFF_PLAN.md VD1
    pub fn write(&mut self, marks: &[String], s: &InputSample) {
        for m in marks {
            let _ = writeln!(self.out, "{{\"mark\":{m:?},\"tick\":{}}}", self.tick);
        }
        let _ = writeln!(
            self.out,
            "{{\"tick\":{},\"f\":{},\"b\":{},\"l\":{},\"r\":{},\"jump\":{},\"sneak\":{},\"sprint\":{},\"yaw\":{:?},\"pitch\":{:?},\"slot\":{}}}",
            self.tick,
            s.forward,
            s.backward,
            s.left,
            s.right,
            s.jump,
            s.sneak,
            s.sprint,
            s.yaw,
            s.pitch,
            s.slot
        );
        let _ = self.out.flush();
        self.tick += 1;
    }
}

pub struct InputLogPlugin;
impl Plugin for InputLogPlugin {
    // Spec: specs/plans/VANILLA_DIFF_PLAN.md VD1
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            record_input
                .run_if(resource_exists::<InputLogDir>)
                .in_set(PhysicsSystems)
                .after(tick_controls)
                .before(local_player_ai_step),
        );
    }
}

/// Opens `<dir>/<player>.jsonl`, or `<player>.2.jsonl` and so on when that
/// exists: a new connection never overwrites an older log.
fn open_log(dir: &PathBuf, player: &str) -> std::io::Result<File> {
    fs::create_dir_all(dir)?;
    for n in 1.. {
        let name = if n == 1 {
            format!("{player}.jsonl")
        } else {
            format!("{player}.{n}.jsonl")
        };
        match File::create_new(dir.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            r => return r,
        }
    }
    unreachable!()
}

// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD1
#[allow(clippy::type_complexity)]
pub fn record_input(
    dir: Res<InputLogDir>,
    mut query: Query<
        (
            Entity,
            &GameProfileComponent,
            &ClientMovementState,
            &Jumping,
            &LookDirection,
            &Inventory,
            Option<&mut InputLog>,
        ),
        (With<LocalEntity>, With<WorldName>),
    >,
    mut commands: Commands,
) {
    for (entity, profile, mv, jumping, look, inventory, log) in &mut query {
        let d = mv.move_direction;
        let sample = InputSample {
            forward: d.forward(),
            backward: d.backward(),
            left: d.left(),
            right: d.right(),
            jump: **jumping,
            sneak: mv.trying_to_crouch,
            sprint: mv.trying_to_sprint,
            yaw: look.y_rot(),
            pitch: look.x_rot(),
            slot: inventory.selected_hotbar_slot,
        };
        let marks: Vec<String> = {
            let mut all = MARKS.lock().unwrap();
            let (mine, rest) = all.drain(..).partition(|(n, _)| *n == profile.name);
            *all = rest;
            mine.into_iter().map(|(_, l)| l).collect()
        };
        match log {
            Some(mut log) => log.write(&marks, &sample),
            None => match open_log(&dir.0, &profile.name) {
                Ok(f) => {
                    let mut log = InputLog::new(Box::new(f));
                    log.write(&marks, &sample);
                    commands.entity(entity).insert(log);
                }
                Err(e) => tracing::warn!("input log: cannot open a file in {:?}: {e}", dir.0),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn sample(forward: bool, yaw: f32) -> InputSample {
        InputSample {
            forward,
            backward: false,
            left: false,
            right: false,
            jump: false,
            sneak: false,
            sprint: true,
            yaw,
            pitch: -12.5,
            slot: 3,
        }
    }

    #[test]
    fn lines_count_ticks_from_zero_and_marks_take_the_next_tick() {
        let buf = Buf::default();
        let mut log = InputLog::new(Box::new(buf.clone()));
        log.write(&[], &sample(false, 0.0));
        log.write(&["start".to_owned()], &sample(true, 90.0));
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("{\"tick\":0,\"f\":false,"));
        assert_eq!(lines[1], "{\"mark\":\"start\",\"tick\":1}");
        assert_eq!(
            lines[2],
            "{\"tick\":1,\"f\":true,\"b\":false,\"l\":false,\"r\":false,\"jump\":false,\"sneak\":false,\"sprint\":true,\"yaw\":90.0,\"pitch\":-12.5,\"slot\":3}"
        );
    }
}
