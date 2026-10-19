//! Dying and respawning like a vanilla client (Null fork, patch 21).
//!
//! Vanilla opens the death screen on `ClientboundPlayerCombatKill` for our own id
//! (`ClientPacketListener#handlePlayerCombatKill`) or when a tick finds health at or below 0
//! (`Minecraft#setScreen`). The respawn button turns on at the 20th screen tick
//! (`DeathScreen#tick`), and a click sends `PERFORM_RESPAWN` (`LocalPlayer#respawn`). With the
//! "immediate respawn" rule the client sends it at once and shows no screen. The screen replaces
//! any open container screen, so all keys are released while dead.
//!
//! Patch 22 (nothing while dead). With a screen open `Minecraft#tick` skips `handleKeybinds` and
//! `setScreen` releases the mouse and every key (`KeyMapping#releaseAll`), so a dead client makes
//! no movement, look change, slot select, swap, drop, attack, use, break or place. What it still
//! sends each tick is the position packet (`LocalPlayer#sendPosition`, falls and slides included),
//! the client tick end, a sprint stop and one input packet for the released keys, answers to the
//! server (keep-alive, pong, teleport), and the respawn packet. [`DeadAction`] lists what the fork
//! refuses; [`RespawnLog::sent_while_dead`] counts what it sent.

use azalea_core::{position::Vec3, tick::GameTick};
use azalea_entity::{Jumping, LookDirection, inventory::Inventory};
use azalea_physics::client_movement::{ClientMovementState, WalkDirection};
use azalea_inventory::ItemStack;
use azalea_protocol::packets::game::s_client_command::{self, ServerboundClientCommand};
use azalea_world::WorldName;
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tracing::debug;

use crate::{
    attack::{AttackQueued, handle_attack_queued},
    block_actions::{
        AutoCrouchHold, LookAtBlockQueued, StopMiningQueued, handle_break_block_queued,
        handle_look_at_block_queued,
    },
    interact::{StartUseItemQueued, handle_start_use_item_queued},
    inventory::{ensure_has_sent_carried_item, hold_still_while_screen_open},
    item_use::{ReleaseUseItemQueued, SwapOffhandQueued, UseItemQueued, handle_use_item_queued},
    mining::{LeftClickMine, Mining, MiningQueued, MiningSystems},
    packet::game::SendGamePacketEvent,
    tick_counter::TicksConnected,
};

/// The respawn button is active from this screen tick (`DeathScreen#tick`: `delayTicker == 20`).
pub const RESPAWN_BUTTON_TICKS: u32 = 20;

/// Tell the server that we're respawning.
///
/// Held until the death screen has been up for [`RESPAWN_BUTTON_TICKS`] ticks, as the button is.
/// Refused (nothing sent, see [`RespawnLog::last_refusal`]) when we are not dead, in hardcore, or
/// when a request was already sent for this death.
#[derive(Clone, Debug, Message)]
pub struct PerformRespawnEvent {
    pub entity: Entity,
}

/// Whether the client shows the death screen: the login packet's `showDeathScreen`, then the
/// `IMMEDIATE_RESPAWN` game event (`ClientPacketListener#handleGameEvent`).
#[derive(Clone, Component, Copy, Debug, Eq, PartialEq)]
pub struct ShowDeathScreen(pub bool);
impl Default for ShowDeathScreen {
    fn default() -> Self {
        Self(true)
    }
}

/// The server's hardcore flag from the login packet.
#[derive(Clone, Component, Copy, Debug, Default, Eq, PartialEq)]
pub struct IsHardcore(pub bool);

/// Present while we are dead and waiting to respawn: the death screen, or (when the rules show
/// none) the short wait for the respawn packet. A second kill packet replaces it, which resets the
/// counter as `DeathScreen#init` does.
#[derive(Clone, Component, Debug)]
pub struct DeathScreen {
    /// Screen ticks since it opened (the respawn button turns on at [`RESPAWN_BUTTON_TICKS`]).
    pub ticks: u32,
    /// `false`: the client shows no screen and respawned by itself.
    pub shown: bool,
    /// The death message as plain text.
    pub message: String,
    /// A `PERFORM_RESPAWN` went out for this screen.
    pub respawn_sent: bool,
    /// A request came before the button was active and waits for it.
    pub held_request: bool,
    pub opened_tick: u64,
    /// The look and held slot when the screen opened: a vanilla client cannot change them now.
    /// Server updates (teleport, held slot packet) replace them.
    pub locked_look: LookDirection,
    pub locked_slot: u8,
}

/// What we remember of a death. `tick` counts like [`TicksConnected`].
#[derive(Clone, Debug)]
pub struct DeathRecord {
    pub tick: u64,
    pub position: Vec3,
    pub dimension: WorldName,
    pub message: String,
    /// The player window's slots when the kill packet arrived (before the server's drops).
    pub slots: Vec<ItemStack>,
}

/// A respawn packet we sent.
#[derive(Clone, Debug)]
pub struct RespawnRecord {
    pub sent_tick: u64,
    /// Screen ticks at the send; 0 for an immediate respawn.
    pub screen_ticks: u32,
    pub immediate: bool,
    /// The tick the server's respawn packet arrived.
    pub completed_tick: Option<u64>,
    /// Respawn packets sent for this death. With immediate respawn a vanilla client sends one in
    /// the kill packet's handler and one per tick after, until the respawn arrives
    /// (`Minecraft#tick` -> `setScreen(null)` -> `LocalPlayer#respawn`).
    pub sends: u32,
}

/// An input-driven action that a vanilla client cannot make while the death screen is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadAction {
    Walk,
    Sprint,
    Sneak,
    Jump,
    Look,
    SelectSlot,
    SwapOffhand,
    Attack,
    UseItem,
    ReleaseUse,
    BreakBlock,
    StopBreak,
    PlaceBlock,
    UseBlock,
    LookAtBlock,
    InteractEntity,
    Swing,
    Goto,
}

impl DeadAction {
    pub fn name(self) -> &'static str {
        match self {
            DeadAction::Walk => "walk",
            DeadAction::Sprint => "sprint",
            DeadAction::Sneak => "sneak",
            DeadAction::Jump => "jump",
            DeadAction::Look => "look",
            DeadAction::SelectSlot => "select_slot",
            DeadAction::SwapOffhand => "swap_offhand",
            DeadAction::Attack => "attack",
            DeadAction::UseItem => "use_item",
            DeadAction::ReleaseUse => "release_use",
            DeadAction::BreakBlock => "break_block",
            DeadAction::StopBreak => "stop_break",
            DeadAction::PlaceBlock => "place_block",
            DeadAction::UseBlock => "use_block",
            DeadAction::LookAtBlock => "look_at_block",
            DeadAction::InteractEntity => "interact_entity",
            DeadAction::Swing => "swing",
            DeadAction::Goto => "goto",
        }
    }

    pub fn reason(self) -> &'static str {
        "the death screen is open: a vanilla client has no keys or mouse then"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RespawnRefusal {
    NotDead,
    Hardcore,
    AlreadySent,
}

impl RespawnRefusal {
    pub fn reason(self) -> &'static str {
        match self {
            RespawnRefusal::NotDead => "not dead",
            RespawnRefusal::Hardcore => "hardcore: the button spectates",
            RespawnRefusal::AlreadySent => "respawn already requested for this death",
        }
    }
}

/// The record of deaths and respawns of one client.
#[derive(Clone, Component, Debug, Default)]
pub struct RespawnLog {
    pub deaths: u32,
    pub respawns: u32,
    pub last_death: Option<DeathRecord>,
    pub last_respawn: Option<RespawnRecord>,
    pub last_refusal: Option<RespawnRefusal>,
    /// Actions refused since the last death began, with how many times (patch 22).
    pub refused_while_dead: Vec<(DeadAction, u32)>,
    /// Game packets sent since the last death began, by name, up to the respawn packet.
    pub sent_while_dead: Vec<(String, u32)>,
}

impl RespawnLog {
    /// Notes a refused action (nothing was sent).
    pub fn refuse(&mut self, action: DeadAction) {
        debug!("{} refused on the client: {}", action.name(), action.reason());
        match self.refused_while_dead.iter_mut().find(|(a, _)| *a == action) {
            Some((_, n)) => *n += 1,
            None => self.refused_while_dead.push((action, 1)),
        }
    }

    /// Notes a packet sent while the death screen was open.
    pub fn note_sent(&mut self, name: &str) {
        match self.sent_while_dead.iter_mut().find(|(n, _)| n == name) {
            Some((_, n)) => *n += 1,
            None => self.sent_while_dead.push((name.to_string(), 1)),
        }
    }
}

/// Whether a request may go out, and whether it must wait (pure; the tests cover it).
pub fn check_respawn_request(
    screen: Option<&DeathScreen>,
    hardcore: bool,
) -> Result<bool, RespawnRefusal> {
    let Some(screen) = screen else {
        return Err(RespawnRefusal::NotDead);
    };
    if hardcore {
        return Err(RespawnRefusal::Hardcore);
    }
    if screen.respawn_sent {
        return Err(RespawnRefusal::AlreadySent);
    }
    Ok(screen.ticks >= RESPAWN_BUTTON_TICKS)
}

impl DeathScreen {
    /// `shown` follows [`ShowDeathScreen`].
    pub fn open(shown: bool, message: String, tick: u64) -> Self {
        Self {
            ticks: 0,
            shown,
            message,
            respawn_sent: false,
            held_request: false,
            opened_tick: tick,
            locked_look: LookDirection::default(),
            locked_slot: 0,
        }
    }
}

/// Opens the death state: the first death is recorded, a repeat only resets the screen.
/// Returns true when this is the first of the death. Shared by the packet handler and the
/// 0-health tick. `from_packet`: the kill packet's handler sends the respawn itself when the rules
/// show no screen (`ClientPacketListener#handlePlayerCombatKill`); the 0-health path leaves it to
/// the tick (`Minecraft#tick`).
// Spec: specs/B2-survival-upkeep.md §4.7
pub fn begin_death(
    commands: &mut Commands,
    entity: Entity,
    first: bool,
    show: bool,
    from_packet: bool,
    message: String,
    tick: u64,
    log: &mut RespawnLog,
    position: Vec3,
    look: LookDirection,
    dimension: &WorldName,
    inventory: &Inventory,
) {
    if first {
        log.deaths += 1;
        log.refused_while_dead.clear();
        log.sent_while_dead.clear();
        log.last_death = Some(DeathRecord {
            tick,
            position,
            dimension: dimension.clone(),
            message: message.clone(),
            slots: inventory.inventory_menu.slots(),
        });
        // The screen releases every key (`KeyMapping#releaseAll`); that is not a refusal.
        commands.entity(entity).queue(|mut e: EntityWorldMut| {
            if let Some(mut mv) = e.get_mut::<ClientMovementState>() {
                mv.move_direction = WalkDirection::None;
                mv.trying_to_sprint = false;
                mv.trying_to_crouch = false;
            }
            if let Some(mut jumping) = e.get_mut::<Jumping>() {
                **jumping = false;
            }
        });
    }
    let mut screen = DeathScreen::open(show, message, tick);
    screen.locked_look = look;
    screen.locked_slot = inventory.selected_hotbar_slot;
    if !show && from_packet {
        // No screen: the client respawns at once (`LocalPlayer#respawn`).
        send_respawn(commands, entity, &mut screen, log, tick);
    }
    // The screen replaces an open container screen, without a close packet (`removed()`).
    commands.entity(entity).remove::<crate::inventory::OpenScreen>().insert(screen);
}

fn send_respawn(
    commands: &mut Commands,
    entity: Entity,
    screen: &mut DeathScreen,
    log: &mut RespawnLog,
    tick: u64,
) {
    commands.trigger(SendGamePacketEvent::new(
        entity,
        ServerboundClientCommand { action: s_client_command::Action::PerformRespawn },
    ));
    if screen.respawn_sent && let Some(r) = log.last_respawn.as_mut() {
        r.sends += 1;
        return;
    }
    screen.respawn_sent = true;
    screen.held_request = false;
    log.last_respawn = Some(RespawnRecord {
        sent_tick: tick,
        screen_ticks: screen.ticks,
        immediate: !screen.shown,
        completed_tick: None,
        sends: 1,
    });
}

/// A plugin that makes [`PerformRespawnEvent`] send the packet to respawn, when a vanilla client
/// could.
pub struct RespawnPlugin;
impl Plugin for RespawnPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PerformRespawnEvent>()
            .add_systems(Update, perform_respawn.after(crate::packet::death_event_on_0_health))
            .add_systems(
                GameTick,
                tick_death_screen.after(crate::tick_counter::increment_counter),
            )
            // After whatever sets input (the host, the pathfinder), before everything that acts on it.
            .add_systems(
                GameTick,
                refuse_input_while_dead
                    .after(tick_death_screen)
                    .after(crate::block_actions::refuse_block_actions_while_dead)
                    .before(handle_attack_queued)
                    .before(handle_use_item_queued)
                    .before(handle_start_use_item_queued)
                    .before(handle_break_block_queued)
                    .before(handle_look_at_block_queued)
                    .before(ensure_has_sent_carried_item)
                    .before(hold_still_while_screen_open)
                    .before(MiningSystems),
            );
    }
}

// Spec: specs/F1-swarm-runtime.md §4.4 (dead, then respawning; one packet per death)
pub fn perform_respawn(
    mut events: MessageReader<PerformRespawnEvent>,
    mut query: Query<(&mut RespawnLog, Option<&mut DeathScreen>, &IsHardcore, &TicksConnected)>,
    mut commands: Commands,
) {
    for event in events.read() {
        let Ok((mut log, screen, hardcore, ticks)) = query.get_mut(event.entity) else {
            continue;
        };
        match check_respawn_request(screen.as_deref(), hardcore.0) {
            // A repeat of a request already sent (the host asks at every death): nothing to say.
            Err(RespawnRefusal::AlreadySent) => {}
            Err(refusal) => {
                debug!("Respawn refused on the client: {}", refusal.reason());
                log.last_refusal = Some(refusal);
            }
            Ok(true) => {
                send_respawn(&mut commands, event.entity, &mut screen.unwrap(), &mut log, ticks.0);
            }
            Ok(false) => screen.unwrap().held_request = true,
        }
    }
}

/// Counts screen ticks (`DeathScreen#tick`) and sends a held request when the button turns on.
// Spec: specs/F1-swarm-runtime.md §4.4
pub fn tick_death_screen(
    mut commands: Commands,
    mut query: Query<(Entity, &mut DeathScreen, &mut RespawnLog, &TicksConnected)>,
) {
    for (entity, mut screen, mut log, ticks) in &mut query {
        screen.ticks = screen.ticks.saturating_add(1);
        if !screen.shown {
            // `Minecraft#tick`: dead with no screen -> `setScreen(null)` -> `respawn()`, every tick
            // until the respawn packet replaces the player.
            send_respawn(&mut commands, entity, &mut screen, &mut log, ticks.0);
        } else if screen.held_request && !screen.respawn_sent && screen.ticks >= RESPAWN_BUTTON_TICKS {
            send_respawn(&mut commands, entity, &mut screen, &mut log, ticks.0);
        }
    }
}

/// Refuses what a vanilla client cannot do with the death screen open: `handleKeybinds` does not
/// run and the keys and mouse are released, so no movement, look, slot select, swap, attack, use,
/// break or place. Queued actions are dropped, input written by the caller is undone, and each
/// refusal is noted in [`RespawnLog::refused_while_dead`]. Block commands are refused in
/// `block_actions.rs` (they carry a result); the pathfinder stops itself (`azalea` crate).
// Spec: specs/F1-swarm-runtime.md §4.4 (a dead bot sends only what the death screen sends)
#[allow(clippy::type_complexity)]
pub fn refuse_input_while_dead(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &DeathScreen,
        &mut RespawnLog,
        (&mut ClientMovementState, &mut Jumping, &mut LookDirection, &mut Inventory),
        (
            Has<AttackQueued>,
            Has<UseItemQueued>,
            Has<ReleaseUseItemQueued>,
            Has<SwapOffhandQueued>,
            Has<StartUseItemQueued>,
            Has<MiningQueued>,
            Has<StopMiningQueued>,
            Has<LookAtBlockQueued>,
        ),
        (Has<Mining>, Has<LeftClickMine>, Has<AutoCrouchHold>),
    )>,
) {
    for (
        entity,
        screen,
        mut log,
        (mut mv, mut jumping, mut look, mut inventory),
        (attack, use_item, release, swap, start_use, mine, stop_mine, look_at),
        (mining, left_click, auto_crouch),
    ) in &mut query
    {
        for (is_set, action) in [
            (attack, DeadAction::Attack),
            (use_item || start_use, DeadAction::UseItem),
            (release, DeadAction::ReleaseUse),
            (swap, DeadAction::SwapOffhand),
            (mine, DeadAction::BreakBlock),
            (stop_mine, DeadAction::StopBreak),
            (look_at, DeadAction::LookAtBlock),
            (mv.move_direction != WalkDirection::None, DeadAction::Walk),
            (mv.trying_to_sprint, DeadAction::Sprint),
            (mv.trying_to_crouch, DeadAction::Sneak),
            (**jumping, DeadAction::Jump),
            (*look != screen.locked_look, DeadAction::Look),
            (inventory.selected_hotbar_slot != screen.locked_slot, DeadAction::SelectSlot),
        ] {
            if is_set {
                log.refuse(action);
            }
        }
        mv.move_direction = WalkDirection::None;
        mv.trying_to_sprint = false;
        mv.trying_to_crouch = false;
        **jumping = false;
        *look = screen.locked_look;
        inventory.selected_hotbar_slot = screen.locked_slot;
        // A mining in progress ends without a packet (no `handleKeybinds` to continue or stop it).
        if attack || use_item || release || swap || start_use || mine || stop_mine || look_at || mining
            || left_click || auto_crouch
        {
            commands.entity(entity).remove::<(
                AttackQueued,
                UseItemQueued,
                ReleaseUseItemQueued,
                SwapOffhandQueued,
                StartUseItemQueued,
                MiningQueued,
                StopMiningQueued,
                LookAtBlockQueued,
                Mining,
                LeftClickMine,
                AutoCrouchHold,
            )>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(ticks: u32) -> DeathScreen {
        let mut s = DeathScreen::open(true, String::new(), 0);
        s.ticks = ticks;
        s
    }

    #[test]
    fn gate_opens_at_the_twentieth_tick() {
        assert_eq!(check_respawn_request(Some(&screen(19)), false), Ok(false));
        assert_eq!(check_respawn_request(Some(&screen(20)), false), Ok(true));
    }

    #[test]
    fn refusals() {
        assert_eq!(check_respawn_request(None, false), Err(RespawnRefusal::NotDead));
        assert_eq!(check_respawn_request(Some(&screen(30)), true), Err(RespawnRefusal::Hardcore));
        let mut s = screen(30);
        s.respawn_sent = true;
        assert_eq!(check_respawn_request(Some(&s), false), Err(RespawnRefusal::AlreadySent));
    }

    #[test]
    fn held_request_goes_out_on_the_gate_tick() {
        let mut app = App::new();
        app.add_systems(Update, tick_death_screen);
        let e = app
            .world_mut()
            .spawn((
                { let mut s = screen(0); s.held_request = true; s },
                RespawnLog::default(),
                TicksConnected(100),
            ))
            .id();
        for i in 1..=25 {
            app.update();
            let s = app.world().get::<DeathScreen>(e).unwrap();
            assert_eq!(s.respawn_sent, i >= 20, "tick {i}");
        }
        let log = app.world().get::<RespawnLog>(e).unwrap();
        assert_eq!(log.last_respawn.as_ref().unwrap().screen_ticks, 20);
    }
}
