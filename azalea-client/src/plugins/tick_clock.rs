//! The game-tick clock of the run loop, like the vanilla client's (Null fork, patch 23).
//!
//! Vanilla keeps a fractional "ticks due" residual. Each frame it adds the elapsed time divided by
//! the tick length (`DeltaTracker.Timer#advanceGameTime`), takes the whole part as the ticks due
//! and keeps the fraction. It then runs at most 10 client ticks back to back after the queued
//! packets (`Minecraft#runTick`); the ticks due beyond 10 are lost. The tick length follows the
//! server's tick rate, but never goes below 50 ms (`Minecraft#getTickTargetMillis`).
//!
//! Not done: the frozen-and-stepping state. `TickRateManager#runsNormally` is also true for the
//! ticks a `/tick step` runs; here a frozen server always counts as 50 ms.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use bevy_ecs::prelude::*;

/// The length of one tick at the default 20 ticks per second.
pub const DEFAULT_TICK_MS: f64 = 50.0;
/// Most client ticks one frame runs (`Minecraft#runTick`).
pub const MAX_TICKS_PER_FRAME: u32 = 10;

/// The server's tick rate and frozen flag, from `ClientboundTickingState`.
#[derive(Clone, Copy, Debug, PartialEq, Resource)]
pub struct ServerTickRate {
    pub tick_rate: f32,
    pub frozen: bool,
}

impl Default for ServerTickRate {
    fn default() -> Self {
        Self { tick_rate: 20.0, frozen: false }
    }
}

impl ServerTickRate {
    // Spec: specs/F5-version-layer.md §4.6 (tick catch-up like vanilla; `Minecraft#getTickTargetMillis`)
    pub fn tick_length_ms(&self) -> f64 {
        if self.frozen {
            return DEFAULT_TICK_MS;
        }
        // `TickRateManager#setTickRate` keeps the rate at 1 or more.
        let server_ms = 1000.0 / f64::from(self.tick_rate.max(1.0));
        server_ms.max(DEFAULT_TICK_MS)
    }
}

/// Adds `elapsed_ms` to the residual and returns the whole ticks due and the new residual
/// (`DeltaTracker.Timer#advanceGameTime`).
// Spec: specs/F5-version-layer.md §4.6 (tick catch-up like vanilla)
pub fn advance_game_time(residual: f64, elapsed_ms: f64, tick_ms: f64) -> (u32, f64) {
    let total = residual + elapsed_ms.max(0.0) / tick_ms;
    let due = total.floor();
    (due as u32, total - due)
}

/// How many of the ticks due one frame runs (`Minecraft#runTick`).
pub fn ticks_to_run(due: u32) -> u32 {
    due.min(MAX_TICKS_PER_FRAME)
}

/// The residual and the time of the last frame.
#[derive(Debug, Default)]
pub struct TickClock {
    residual: f64,
    last: Option<Instant>,
}

impl TickClock {
    /// The ticks due at `now`; the first frame has none.
    // Spec: specs/F5-version-layer.md §4.6 (tick catch-up like vanilla)
    pub fn advance(&mut self, now: Instant, tick_ms: f64) -> u32 {
        let Some(last) = self.last.replace(now) else { return 0 };
        let (due, residual) =
            advance_game_time(self.residual, now.duration_since(last).as_secs_f64() * 1000.0, tick_ms);
        self.residual = residual;
        due
    }
}

/// Client ticks this client dropped in all (`due` beyond 10 in one frame). The server's ticks go
/// on while the client drops them, so a count of server ticks from client ticks adds them back
/// (`ItemInUse::server_ticks`, patch 39).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Resource)]
pub struct DroppedTicks(pub u64);

/// Counters for the loop, read by the ability check: the update that had the most ticks due,
/// what it ran and sent, and the ticks dropped in all. Reset with [`reset_loop_stats`].
pub static MAX_DUE: AtomicU64 = AtomicU64::new(0);
pub static RAN_AT_MAX_DUE: AtomicU64 = AtomicU64::new(0);
pub static TICK_ENDS_AT_MAX_DUE: AtomicU64 = AtomicU64::new(0);
pub static TICKS_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Every `ServerboundClientTickEnd` queued, all clients.
pub static CLIENT_TICK_ENDS: AtomicU64 = AtomicU64::new(0);

pub fn reset_loop_stats() {
    for a in [&MAX_DUE, &RAN_AT_MAX_DUE, &TICK_ENDS_AT_MAX_DUE, &TICKS_DROPPED] {
        a.store(0, Ordering::Relaxed);
    }
}

/// Notes one update: `due` ticks were due, `ran` ran and sent `ends` tick-end packets.
pub fn note_update(due: u32, ran: u32, ends: u64) {
    TICKS_DROPPED.fetch_add(u64::from(due - ran), Ordering::Relaxed);
    if u64::from(due) > MAX_DUE.load(Ordering::Relaxed) {
        MAX_DUE.store(u64::from(due), Ordering::Relaxed);
        RAN_AT_MAX_DUE.store(u64::from(ran), Ordering::Relaxed);
        TICK_ENDS_AT_MAX_DUE.store(ends, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn normal_frames_run_one_tick_in_three() {
        // 60 frames of 16.667 ms are 1000 ms: 20 ticks, never more than one per frame.
        let mut res = 0.0;
        let mut total = 0;
        for _ in 0..60 {
            let (due, r) = advance_game_time(res, 1000.0 / 60.0, 50.0);
            res = r;
            assert!(due <= 1);
            total += due;
        }
        assert_eq!(total, 20);
    }

    #[test]
    fn a_347_ms_stall_runs_six_or_seven_and_keeps_the_fraction() {
        let (due, residual) = advance_game_time(0.0, 347.0, 50.0);
        assert_eq!(due, 6);
        assert!((residual - 0.94).abs() < 1e-9);
        // with 0.2 of a tick left over from the frame before, the seventh tick comes due too
        let (due, residual) = advance_game_time(0.2, 347.0, 50.0);
        assert_eq!((due, ticks_to_run(due)), (7, 7));
        assert!((residual - 0.14).abs() < 1e-9);
    }

    #[test]
    fn a_2_s_stall_runs_ten_and_drops_the_rest() {
        let (due, residual) = advance_game_time(0.0, 2000.0, 50.0);
        assert_eq!((due, ticks_to_run(due)), (40, 10));
        assert!(residual.abs() < 1e-9);
        // the fraction survives the drop
        let (due, residual) = advance_game_time(0.5, 2010.0, 50.0);
        assert_eq!((due, ticks_to_run(due)), (40, 10));
        assert!((residual - 0.7).abs() < 1e-9);
    }

    #[test]
    fn the_cap_is_ten() {
        assert_eq!(ticks_to_run(0), 0);
        assert_eq!(ticks_to_run(10), 10);
        assert_eq!(ticks_to_run(11), 10);
    }

    #[test]
    fn a_slower_server_makes_longer_ticks_and_a_faster_one_does_not_shorten_them() {
        let slow = ServerTickRate { tick_rate: 10.0, frozen: false };
        assert_eq!(slow.tick_length_ms(), 100.0);
        assert_eq!(advance_game_time(0.0, 347.0, slow.tick_length_ms()).0, 3);
        assert_eq!(ServerTickRate { tick_rate: 40.0, frozen: false }.tick_length_ms(), 50.0);
        assert_eq!(ServerTickRate { tick_rate: 5.0, frozen: true }.tick_length_ms(), 50.0);
        // a rate under 1 reads as 1, like `TickRateManager#setTickRate`
        assert_eq!(ServerTickRate { tick_rate: 0.0, frozen: false }.tick_length_ms(), 1000.0);
    }

    #[test]
    fn the_clock_starts_at_zero_ticks_and_follows_time() {
        let mut c = TickClock::default();
        let t0 = Instant::now();
        assert_eq!(c.advance(t0, 50.0), 0);
        assert_eq!(c.advance(t0 + Duration::from_millis(347), 50.0), 6);
        // 0.94 left over + 53 ms = 2.0 ticks in f64 rounding terms: one or two, never lost
        let due = c.advance(t0 + Duration::from_millis(400), 50.0);
        assert!((1..=2).contains(&due));
    }
}
