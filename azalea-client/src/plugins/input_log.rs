//! Null fork: records the bot's input per client tick, for the vanilla diff
//! (VD1). It changes nothing that is sent. Off unless the app holds an
//! [`InputLogDir`] resource.
//!
//! One JSON line per client tick,
//! `{"tick":N,"f":..,"b":..,"l":..,"r":..,"jump":..,"sneak":.., "sprint":..,"
//! yaw":..,"pitch":..,"slot":..,"attack":..}`. `N` counts the client ticks of the
//! connection from 0, the same way the vanilla-diff tap counts serverbound
//! `ClientTickEnd` packets, so the two logs align. The line is written where
//! vanilla's `LocalPlayer#aiStep` has just read the keys (`KeyboardInput#
//! tick`): after the screen hold and `tick_controls`, before the sprint
//! decision and the physics step, so it is the input the step used. [`mark`]
//! adds `{"mark":"..","tick":N}` lines, where `N` is the tick of the next
//! sample.
//!
//! Patch 46 adds what the player does besides moving. `"attack"` on the sample line is the attack
//! key held down in that tick (vanilla's `Minecraft#continueAttack`): Null holds it while it mines
//! (`continueDestroyBlock`), waits out the 5-tick destroy delay, or starts a break (the start
//! comes from the held key, so it has no click). Discrete actions are `{"ev":"..","tick":N}`
//! lines before the sample of tick `N`, noted by [`note`] where the fork sends the action (a
//! click the client refuses is not noted, it sends nothing): `use_click` (the use key,
//! `Minecraft#startUseItem`, for a block, an entity or an item), `use_item` (Null sends
//! `UseItem` for a hand whatever the crosshair shows: no key press does that with a block in
//! view), `attack_click` (`Minecraft#startAttack` on an entity), `swap_offhand`, `open_inventory`,
//! `click` (`win`, `slot`, `button`, `type`: the arguments of vanilla's
//! `MultiPlayerGameMode#handleInventoryMouseClick`) and `close` (`win`). Screen clicks happen
//! between ticks in vanilla, so the replay does them before the tick `N` starts.

use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
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

/// Set once the log is on, so [`note`] and [`hold_attack`] cost nothing without `InputLogDir`.
static ARMED: AtomicBool = AtomicBool::new(false);

/// What the player did in a tick besides moving, noted by the fork where it sends it.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// The use key, for a block, an entity or an item in view.
    UseClick,
    /// `UseItem` for a hand, sent whatever the crosshair shows.
    UseItem { off_hand: bool },
    /// The attack key, on an entity.
    AttackClick,
    SwapOffhand,
    OpenInventory,
    /// `MultiPlayerGameMode#handleInventoryMouseClick` arguments.
    Click { window: i32, slot: i16, button: u8, kind: u8 },
    Close { window: i32 },
}

impl InputEvent {
    /// The `{"ev":..}` line of this action.
    // Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2
    fn json(&self, tick: u64) -> String {
        match self {
            Self::UseClick => format!("{{\"ev\":\"use_click\",\"tick\":{tick}}}"),
            Self::UseItem { off_hand } => format!(
                "{{\"ev\":\"use_item\",\"hand\":\"{}\",\"tick\":{tick}}}",
                if *off_hand { "off" } else { "main" }
            ),
            Self::AttackClick => format!("{{\"ev\":\"attack_click\",\"tick\":{tick}}}"),
            Self::SwapOffhand => format!("{{\"ev\":\"swap_offhand\",\"tick\":{tick}}}"),
            Self::OpenInventory => format!("{{\"ev\":\"open_inventory\",\"tick\":{tick}}}"),
            Self::Click { window, slot, button, kind } => format!(
                "{{\"ev\":\"click\",\"win\":{window},\"slot\":{slot},\"button\":{button},\"type\":{kind},\"tick\":{tick}}}"
            ),
            Self::Close { window } => format!("{{\"ev\":\"close\",\"win\":{window},\"tick\":{tick}}}"),
        }
    }
}

/// What the systems noted for each player since its last sample: the events, and whether the
/// attack key is held in this tick.
struct Noted {
    events: Vec<(Entity, InputEvent)>,
    attack: Vec<Entity>,
}
static NOTED: Mutex<Noted> = Mutex::new(Noted { events: Vec::new(), attack: Vec::new() });

/// Notes that `entity` did `event` in this tick. A no-op when the log is off.
// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2
pub fn note(entity: Entity, event: InputEvent) {
    if ARMED.load(Ordering::Relaxed) {
        NOTED.lock().unwrap().events.push((entity, event));
    }
}

/// Notes that `entity` holds the attack key in this tick. A no-op when the log is off.
// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2
pub fn hold_attack(entity: Entity) {
    if ARMED.load(Ordering::Relaxed) {
        NOTED.lock().unwrap().attack.push(entity);
    }
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
    /// The attack key is held (see the module doc).
    pub attack: bool,
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

    /// Writes the mark lines, the event lines, then the sample line for the next tick.
    // Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2
    pub fn write(&mut self, marks: &[String], events: &[InputEvent], s: &InputSample) {
        for m in marks {
            let _ = writeln!(self.out, "{{\"mark\":{m:?},\"tick\":{}}}", self.tick);
        }
        for e in events {
            let _ = writeln!(self.out, "{}", e.json(self.tick));
        }
        let _ = writeln!(
            self.out,
            "{{\"tick\":{},\"f\":{},\"b\":{},\"l\":{},\"r\":{},\"jump\":{},\"sneak\":{},\"sprint\":{},\"yaw\":{:?},\"pitch\":{:?},\"slot\":{},\"attack\":{}}}",
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
            s.slot,
            s.attack
        );
        let _ = self.out.flush();
        self.tick += 1;
    }
}

pub struct InputLogPlugin;
impl Plugin for InputLogPlugin {
    // Spec: specs/plans/VANILLA_DIFF_PLAN.md VD1
    fn build(&self, app: &mut App) {
        app.add_systems(GameTick, arm_log.run_if(resource_exists::<InputLogDir>));
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

// Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2
fn arm_log() {
    ARMED.store(true, Ordering::Relaxed);
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
            attack: false,
        };
        let (events, attack) = {
            let mut noted = NOTED.lock().unwrap();
            let events: Vec<InputEvent> =
                noted.events.iter().filter(|(e, _)| *e == entity).map(|(_, ev)| ev.clone()).collect();
            noted.events.retain(|(e, _)| *e != entity);
            let attack = noted.attack.contains(&entity);
            noted.attack.retain(|e| *e != entity);
            (events, attack)
        };
        let sample = InputSample { attack, ..sample };
        let marks: Vec<String> = {
            let mut all = MARKS.lock().unwrap();
            let (mine, rest) = all.drain(..).partition(|(n, _)| *n == profile.name);
            *all = rest;
            mine.into_iter().map(|(_, l)| l).collect()
        };
        match log {
            Some(mut log) => log.write(&marks, &events, &sample),
            None => match open_log(&dir.0, &profile.name) {
                Ok(f) => {
                    let mut log = InputLog::new(Box::new(f));
                    log.write(&marks, &events, &sample);
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
            attack: false,
        }
    }

    #[test]
    fn lines_count_ticks_from_zero_and_marks_take_the_next_tick() {
        let buf = Buf::default();
        let mut log = InputLog::new(Box::new(buf.clone()));
        log.write(&[], &[], &sample(false, 0.0));
        log.write(&["start".to_owned()], &[], &sample(true, 90.0));
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("{\"tick\":0,\"f\":false,"));
        assert_eq!(lines[1], "{\"mark\":\"start\",\"tick\":1}");
        assert_eq!(
            lines[2],
            "{\"tick\":1,\"f\":true,\"b\":false,\"l\":false,\"r\":false,\"jump\":false,\"sneak\":false,\"sprint\":true,\"yaw\":90.0,\"pitch\":-12.5,\"slot\":3,\"attack\":false}"
        );
    }

    // Spec: specs/plans/VANILLA_DIFF_PLAN.md VD2
    #[test]
    fn events_come_before_the_sample_of_their_tick() {
        let buf = Buf::default();
        let mut log = InputLog::new(Box::new(buf.clone()));
        log.write(&[], &[], &sample(false, 0.0));
        let events = [InputEvent::SwapOffhand, InputEvent::Click { window: 0, slot: -999, button: 1, kind: 4 }];
        log.write(&[], &events, &InputSample { attack: true, ..sample(false, 0.0) });
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "{\"ev\":\"swap_offhand\",\"tick\":1}");
        assert_eq!(lines[2], "{\"ev\":\"click\",\"win\":0,\"slot\":-999,\"button\":1,\"type\":4,\"tick\":1}");
        assert!(lines[3].starts_with("{\"tick\":1,") && lines[3].ends_with("\"attack\":true}"));
    }
}
