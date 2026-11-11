//! A second thread for TopK scans: one is started for the process and
//! sleeps (parked) between queries, and only where the process may keep two
//! CPUs busy. `Helper::join` runs one closure here and another on that
//! thread, and returns once both are done.
//!
//! `MVDB_SCAN_THREADS=1` turns the helper off and `=2` turns it on whatever
//! the CPU quota (for comparisons); `MVDB_SPLIT_MIN_ROWS` changes the table
//! size from which a scan is split (4,096); `MVDB_SCAN_STATS=1` prints how
//! long the helper took to start and how long the caller waited for it, at
//! shutdown, to standard error.

use std::cell::UnsafeCell;
use std::hint;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::thread::{self, Thread};
use std::time::Instant;

use crate::cgroup;

const IDLE: u8 = 0;
const POSTED: u8 = 1;
const DONE: u8 = 2;
const PANICKED: u8 = 3;

/// Turns of the waiting loop spent spinning (about 100 us) before the
/// caller yields.
const SPINS: u32 = 1 << 11;

pub struct Helper {
    thread: Thread,
    /// Taken by a caller for the length of a `join`; a second caller at the
    /// same time (tests run in parallel) runs both halves itself.
    busy: AtomicBool,
    state: AtomicU8,
    job: UnsafeCell<Job>,
    stats: Option<Stats>,
}

// SAFETY: `job` is written by the caller holding `busy` before it publishes
// POSTED and read by the helper only after it sees POSTED; the closure it
// points to is `Send`.
unsafe impl Sync for Helper {}
unsafe impl Send for Helper {}

/// A closure on the caller's stack, its type erased. The caller does not
/// return (or unwind) before the helper has run it.
#[derive(Clone, Copy)]
struct Job {
    data: *mut (),
    call: unsafe fn(*mut ()),
}

static HELPER: OnceLock<Option<Helper>> = OnceLock::new();

/// The helper, started at the first call, or `None` where it is off or the
/// thread could not be started.
pub fn get() -> Option<&'static Helper> {
    HELPER.get_or_init(start).as_ref()
}

/// Smallest table whose unfiltered scans are split.
pub fn min_rows() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("MVDB_SPLIT_MIN_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(4096))
}

/// On under a CPU quota of at least 1.8 CPUs, or without a quota on a
/// machine with two CPUs or more.
fn wanted() -> bool {
    match std::env::var("MVDB_SCAN_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
        Some(n) => n >= 2,
        None => match cgroup::cpu_quota() {
            Some(q) => q >= 1.8,
            None => thread::available_parallelism().is_ok_and(|n| n.get() >= 2),
        },
    }
}

fn start() -> Option<Helper> {
    if !wanted() {
        return None;
    }
    let spawned = thread::Builder::new().name("topk-helper".into()).spawn(|| {
        if let Some(h) = HELPER.wait() {
            h.serve();
        }
    });
    let thread = spawned.ok()?.thread().clone();
    let stats = std::env::var("MVDB_SCAN_STATS").is_ok_and(|v| v == "1").then(Stats::default);
    Some(Helper { thread, busy: AtomicBool::new(false), state: AtomicU8::new(IDLE), job: UnsafeCell::new(Job::NONE), stats })
}

impl Job {
    const NONE: Job = Job { data: std::ptr::null_mut(), call: noop };

    fn erase<F: FnMut() + Send>(f: &mut F) -> Job {
        unsafe fn call<F: FnMut()>(data: *mut ()) {
            (*data.cast::<F>())()
        }
        Job { data: (f as *mut F).cast(), call: call::<F> }
    }
}

unsafe fn noop(_: *mut ()) {}

impl Helper {
    /// Runs `mine` here and `theirs` on the helper thread.
    pub fn join<A, B: Send>(&self, mine: impl FnOnce() -> A, theirs: impl FnOnce() -> B + Send) -> (A, B) {
        if self.busy.swap(true, Ordering::Acquire) {
            return (mine(), theirs());
        }
        let mut theirs = Some(theirs);
        let mut out = None;
        let mut task = || out = theirs.take().map(|f| f());
        let posted = Instant::now();
        // SAFETY: `busy` is held and the helper is idle, so nothing reads
        // `job` until POSTED is published below.
        unsafe { *self.job.get() = Job::erase(&mut task) };
        self.state.store(POSTED, Ordering::Release);
        self.thread.unpark();
        // Waits for the helper even if `mine` panics, as `task` borrows
        // this frame.
        let mut wait = Wait { helper: self, posted, mine_done: None };
        let a = mine();
        wait.mine_done = Some(Instant::now());
        let panicked = wait.finish();
        std::mem::forget(wait);
        if panicked {
            panic!("the TopK helper thread panicked");
        }
        (a, out.expect("the helper ran the task"))
    }

    fn serve(&self) {
        loop {
            // A wake-up may be spurious or come before POSTED is visible.
            while self.state.load(Ordering::Acquire) != POSTED {
                thread::park();
            }
            if let Some(s) = &self.stats {
                s.started.store(Instant::now().duration_since(s.epoch).as_nanos() as u64, Ordering::Relaxed);
            }
            // SAFETY: the caller wrote `job` before publishing POSTED and
            // keeps the closure alive until it sees DONE or PANICKED.
            let job = unsafe { *self.job.get() };
            let ok = panic::catch_unwind(AssertUnwindSafe(|| unsafe { (job.call)(job.data) })).is_ok();
            self.state.store(if ok { DONE } else { PANICKED }, Ordering::Release);
        }
    }
}

struct Wait<'h> {
    helper: &'h Helper,
    posted: Instant,
    mine_done: Option<Instant>,
}

impl Wait<'_> {
    /// Waits for the helper to finish the job and frees it for the next
    /// caller; true if the job panicked.
    fn finish(&self) -> bool {
        let h = self.helper;
        let mut spins = 0u32;
        let end = loop {
            match h.state.load(Ordering::Acquire) {
                s @ (DONE | PANICKED) => break s,
                _ if spins < SPINS => {
                    spins += 1;
                    hint::spin_loop();
                }
                _ => thread::yield_now(),
            }
        };
        if let Some(s) = &h.stats {
            let done = Instant::now();
            let started = s.epoch + std::time::Duration::from_nanos(s.started.load(Ordering::Relaxed));
            let mine_done = self.mine_done.unwrap_or(done);
            s.record(started.saturating_duration_since(self.posted), done.saturating_duration_since(mine_done), mine_done.saturating_duration_since(self.posted));
        }
        h.state.store(IDLE, Ordering::Relaxed);
        h.busy.store(false, Ordering::Release);
        end == PANICKED
    }
}

impl Drop for Wait<'_> {
    /// Reached only when `mine` panicked.
    fn drop(&mut self) {
        self.finish();
    }
}

/// Totals over every split scan, in nanoseconds.
struct Stats {
    epoch: Instant,
    started: AtomicU64,
    splits: AtomicU64,
    start_delay: AtomicU64,
    max_start_delay: AtomicU64,
    late_over_10us: AtomicU64,
    wait: AtomicU64,
    mine: AtomicU64,
}

impl Default for Stats {
    fn default() -> Stats {
        let z = || AtomicU64::new(0);
        Stats { epoch: Instant::now(), started: z(), splits: z(), start_delay: z(), max_start_delay: z(), late_over_10us: z(), wait: z(), mine: z() }
    }
}

impl Stats {
    fn record(&self, start_delay: std::time::Duration, wait: std::time::Duration, mine: std::time::Duration) {
        let ns = |d: std::time::Duration| d.as_nanos() as u64;
        self.splits.fetch_add(1, Ordering::Relaxed);
        self.start_delay.fetch_add(ns(start_delay), Ordering::Relaxed);
        self.max_start_delay.fetch_max(ns(start_delay), Ordering::Relaxed);
        self.late_over_10us.fetch_add(u64::from(ns(start_delay) > 10_000), Ordering::Relaxed);
        self.wait.fetch_add(ns(wait), Ordering::Relaxed);
        self.mine.fetch_add(ns(mine), Ordering::Relaxed);
    }
}

/// With `MVDB_SCAN_STATS=1`, one line of totals on standard error.
pub fn report() {
    let Some(Some(Helper { stats: Some(s), .. })) = HELPER.get() else {
        return;
    };
    let n = s.splits.load(Ordering::Relaxed).max(1) as f64;
    let us = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64 / n / 1000.0;
    eprintln!(
        "topk-helper: {} splits; helper start delay mean {:.1} us, max {:.1} us, {} over 10 us; caller's half {:.1} us, then waited {:.1} us",
        s.splits.load(Ordering::Relaxed),
        us(&s.start_delay),
        s.max_start_delay.load(Ordering::Relaxed) as f64 / 1000.0,
        s.late_over_10us.load(Ordering::Relaxed),
        us(&s.mine),
        us(&s.wait),
    );
}
