//! Benchmark instrumentation (null-bench fork patch). Not part of upstream.
//!
//! A global, lock-light sample log. Everything is off unless `enable()` is
//! called, so it costs one relaxed atomic load per call site otherwise.

use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Kind {
    /// wall time of one `run_schedule(GameTick)`
    GameTick,
    /// wall time of one `run_schedule(Main)` (the Update loop, 60 Hz)
    UpdateLoop,
    // pathfinder systems that run in GameTick
    PfTimeoutMovement,
    PfCheckObstruction,
    PfCheckNodeReached,
    PfTickExecute,
    PfRecalcNearEnd,
    PfRecalcNoPath,
    // pathfinder systems that run in Update
    PfGotoListener,
    PfHandleTasks,
    PfPathFoundListener,
    /// A* wall time inside the AsyncComputeTaskPool task
    PfAstarPool,
    /// A* wall time of the synchronous "instant" attempt (2000 nodes) in goto_listener
    PfAstarInstant,
    /// time from thread_pool.spawn() to the task closure starting
    PfQueueWait,
    /// GotoEvent creation -> PathFoundEvent consumed in path_found_listener
    PfRequestToFound,
    // chunk-section work inside CachedWorld
    PfChunkClone,
    PfBitsets,
    /// PreUpdate `read_packets` (decode + packet handlers), all bots
    ReadPackets,
    /// Update `handle_receive_chunk_event`
    ChunkEvent,
    // observer
    ObsPacketSystem,
    ObsOther,
    Count,
}
const N: usize = Kind::Count as usize;

pub struct Sample {
    pub t_ns: u64,
    pub v_ns: u64,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static START: Mutex<Option<Instant>> = Mutex::new(None);
static LOGS: [Mutex<Vec<Sample>>; N] = [const { Mutex::new(Vec::new()) }; N];
pub static COUNTERS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

/// Counter indices.
pub mod counter {
    pub const PATH_POOL: usize = 0;
    pub const PATH_INSTANT: usize = 1;
    pub const PATH_PARTIAL: usize = 2;
    pub const PATH_EMPTY: usize = 3;
    pub const PATH_OUTDATED: usize = 4;
    pub const PATH_FOUND_OK: usize = 5;
    pub const SECTIONS_BUILT: usize = 6;
    pub const CHUNK_CLONES: usize = 7;
    pub const SECTIONS_UNIQUE: usize = 8;
    pub const PATH_NODES_SUM: usize = 9;
}

pub fn enable() {
    *START.lock() = Some(Instant::now());
    ENABLED.store(true, Ordering::Relaxed);
}
pub fn disable() {
    ENABLED.store(false, Ordering::Relaxed);
}
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn reset() {
    for l in LOGS.iter() {
        l.lock().clear();
    }
    for c in COUNTERS.iter() {
        c.store(0, Ordering::Relaxed);
    }
    *START.lock() = Some(Instant::now());
}
pub fn count(idx: usize, n: u64) {
    if enabled() {
        COUNTERS[idx].fetch_add(n, Ordering::Relaxed);
    }
}
pub fn counter_value(idx: usize) -> u64 {
    COUNTERS[idx].load(Ordering::Relaxed)
}
pub fn record_ns(kind: Kind, ns: u64) {
    if !enabled() {
        return;
    }
    let t_ns = START
        .lock()
        .map(|s| s.elapsed().as_nanos() as u64)
        .unwrap_or(0);
    LOGS[kind as usize].lock().push(Sample { t_ns, v_ns: ns });
}
pub fn drain(kind: Kind) -> Vec<Sample> {
    std::mem::take(&mut *LOGS[kind as usize].lock())
}

/// RAII timer: records its lifetime as a sample of `kind`.
pub struct Timer(Kind, Option<Instant>);
impl Timer {
    #[inline]
    pub fn new(kind: Kind) -> Self {
        Timer(kind, enabled().then(Instant::now))
    }
}
impl Drop for Timer {
    #[inline]
    fn drop(&mut self) {
        if let Some(t) = self.1 {
            record_ns(self.0, t.elapsed().as_nanos() as u64);
        }
    }
}

static LAST_MARK: Mutex<Option<Instant>> = Mutex::new(None);
/// Chain-segment timing: `mark(None)` starts, `mark(Some(k))` records the time since the previous mark as `k`.
pub fn mark(kind: Option<Kind>) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let mut l = LAST_MARK.lock();
    if let (Some(k), Some(prev)) = (kind, *l) {
        record_ns(k, now.duration_since(prev).as_nanos() as u64);
    }
    *l = Some(Instant::now());
}

/// CPU time used by the calling thread, in ns.
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
/// CPU time used by the whole process, in ns.
pub fn process_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
/// Thread CPU ns spent inside ECS schedule runs (Update loop + GameTick) on the ECS thread.
pub static ECS_CPU_NS: AtomicU64 = AtomicU64::new(0);
/// Thread CPU ns spent inside pathfinder A* tasks on the AsyncComputeTaskPool.
pub static POOL_ASTAR_CPU_NS: AtomicU64 = AtomicU64::new(0);
