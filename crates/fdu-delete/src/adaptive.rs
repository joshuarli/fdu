//! Adaptive deletion concurrency.
//!
//! Removing entries mostly waits on the filesystem, and how many threads help depends on the
//! host: on an idle one more is faster up to a point, on a busy one fewer is, and too many
//! floods it with calls. Rayon's pool cannot be resized, so the pool is built at its largest and
//! a [`Gate`] decides how many threads may be inside a filesystem call at once. A controller
//! thread watches the gate's throughput and moves that limit with [`Controller`], a hill climber
//! that knows nothing about files or threads, only the windows of measurements it is given.
//!
//! The deletion path only calls [`Gate::enter`] around its syscalls. It never sees the
//! controller, and the controller never sees an entry.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How many threads a deletion uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Concurrency {
    /// This many, for the whole run.
    Fixed(usize),
    /// Start small and let the controller move between [`LADDER`] levels.
    Adaptive,
}

/// What a deletion uses unless told otherwise.
///
/// Removing a directory mostly waits on the filesystem (a journal commit, not the CPU), and that
/// waiting overlaps across threads, so the pool is larger than the number of cores. More threads
/// stop helping early and then hurt, and the best size falls as the host gets busier. Measured
/// on the layout fixture (192 thousand entries), whole-tree deletion on an idle host took 4.3 s
/// with 2 threads, 2.9 s with 4, 2.7 s with 16, 2.3 s with 32, and no less with 64 or 128. On a
/// host with a load average of 20 to 40 it took a median of 11.6 s with 4 threads, 9.6 s with 8,
/// 10.2 s with 16, 11.8 s with 32 and 22.7 s with 64. Sixteen gives up little when idle and
/// keeps a busy host from being flooded with filesystem calls.
pub(crate) const DEFAULT_THREADS: usize = 16;
/// Fewer planned operations than this never adapt: such a run is over before the controller has
/// converged, so it would only pay for the climb.
pub(crate) const ADAPTIVE_MIN_OPERATIONS: usize = 100_000;

impl Concurrency {
    /// The concurrency for a deletion of `operations` entries, given the value of
    /// `FDU_DELETE_THREADS`: a positive number fixes the thread count, and `adaptive` lets the
    /// controller choose, for a deletion large enough to converge. Anything else, or nothing,
    /// is the default.
    ///
    /// Adapting is opt-in because it has not beaten the default where it was measured. On the
    /// layout fixture (192 thousand entries) it was no faster than 16 threads on an idle host and
    /// slower on a host kept busy by CPU-bound processes (a median of 4.7 s against 3.5 s, once
    /// twice as slow), where it has no time to climb from its small start. On 1.5 million entries and an idle host it matched 16 threads
    /// (21.3 s against 21.2 s) while 32 threads took 19.9 s, a 7% gain it ignores as noise.
    pub(crate) fn choose(setting: Option<&str>, operations: usize) -> Self {
        match setting {
            Some("adaptive") if operations >= ADAPTIVE_MIN_OPERATIONS => Self::Adaptive,
            Some(value) => match value.parse::<usize>() {
                Ok(threads) if threads > 0 => Self::Fixed(threads),
                _ => Self::Fixed(DEFAULT_THREADS),
            },
            None => Self::Fixed(DEFAULT_THREADS),
        }
    }

    /// Threads the pool is built with.
    pub(crate) fn pool_size(self) -> usize {
        match self {
            Self::Fixed(threads) => threads.max(1),
            Self::Adaptive => LADDER[LADDER.len() - 1],
        }
    }

    /// Threads allowed into filesystem calls at the start.
    pub(crate) fn initial_limit(self) -> usize {
        match self {
            Self::Fixed(threads) => threads.max(1),
            Self::Adaptive => LADDER[START_LEVEL],
        }
    }
}

/// Thread counts the controller moves between, one step at a time. Neighbors differ by a factor
/// of two because anything closer is lost in the noise of a window: on the layout fixture the
/// rate of one window varies by tens of percent as the mix of cheap file removals and slow
/// directory removals shifts. A tie goes to the lower level, which makes 16, the count measured
/// to give up little when idle, the usual resting place. 32 is the most that ever helped.
pub(crate) const LADDER: [usize; 5] = [2, 4, 8, 16, 32];
/// Index into [`LADDER`] the climb starts from. A busy host is never flooded: it begins small.
const START_LEVEL: usize = 1;

/// Length of one throughput measurement. Long enough to hold thousands of operations and to
/// average over the shifts in their mix, short enough that a deletion of tens of seconds sees
/// many decisions.
pub(crate) const WINDOW: Duration = Duration::from_millis(250);
/// Windows thrown away after the limit changes, while threads park or wake.
const SETTLE_WINDOWS: usize = 1;
/// Windows whose median makes one trial. One window can land on a journal commit.
const TRIAL_WINDOWS: usize = 3;
/// Trials spent at the chosen level between probes of its neighbors.
const HOLD_TRIALS: u32 = 8;
/// Throughput differences smaller than this are noise. Climbing needs a gain beyond it, and
/// stepping down is accepted while the loss stays within it, so a tie goes to fewer threads.
const NOISE: f64 = 0.10;
/// A climb is refused if per-call latency grew by more than this factor, whatever the throughput.
const LATENCY_LIMIT: f64 = 2.0;
/// While holding, throughput that fell by more than this together with latency that grew by more
/// than [`PRESSURE_LATENCY`] means the host got busier: step down without waiting for the probe.
const PRESSURE_DROP: f64 = 0.25;
const PRESSURE_LATENCY: f64 = 1.5;

/// Limits how many threads are inside a filesystem call at once, and counts what they do.
///
/// A permit is held only for the calls themselves and never while waiting on anything else, so
/// waiting for one cannot deadlock, and a limit of at least one always lets someone through.
pub(crate) struct Gate {
    limit: AtomicUsize,
    active: AtomicUsize,
    waiting: AtomicUsize,
    lock: Mutex<()>,
    wake: Condvar,
    operations: AtomicU64,
    busy_nanos: AtomicU64,
}

impl Gate {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit: AtomicUsize::new(limit.max(1)),
            active: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            lock: Mutex::new(()),
            wake: Condvar::new(),
            operations: AtomicU64::new(0),
            busy_nanos: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        let previous = self.limit.swap(limit.max(1), Ordering::SeqCst);
        if limit > previous && self.waiting.load(Ordering::SeqCst) > 0 {
            let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.wake.notify_all();
        }
    }

    /// Waits until fewer than the limit are inside, then counts the caller in until the permit
    /// is dropped. Lowering the limit never evicts anyone: it only holds back the next callers.
    pub(crate) fn enter(&self) -> Permit<'_> {
        if !self.try_enter() {
            self.enter_slow();
        }
        Permit { gate: self, entered: Instant::now() }
    }

    /// Operations finished so far, and the time they spent inside, waiting for a permit excluded.
    pub(crate) fn totals(&self) -> (u64, u64) {
        (self.operations.load(Ordering::Relaxed), self.busy_nanos.load(Ordering::Relaxed))
    }

    fn try_enter(&self) -> bool {
        let mut active = self.active.load(Ordering::SeqCst);
        loop {
            if active >= self.limit.load(Ordering::SeqCst) {
                return false;
            }
            match self.active.compare_exchange_weak(active, active + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(now) => active = now,
            }
        }
    }

    /// A waiter announces itself before its last look at `active`, and a leaver looks for
    /// waiters after it lowers `active`; with both sequentially consistent, one of them sees
    /// the other, and the leaver notifies under the lock, so the wake-up cannot be missed.
    fn enter_slow(&self) {
        let mut held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.waiting.fetch_add(1, Ordering::SeqCst);
        while !self.try_enter() {
            held = self.wake.wait(held).unwrap_or_else(PoisonError::into_inner);
        }
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) struct Permit<'a> {
    gate: &'a Gate,
    entered: Instant,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let gate = self.gate;
        let nanos = u64::try_from(self.entered.elapsed().as_nanos()).unwrap_or(u64::MAX);
        gate.busy_nanos.fetch_add(nanos, Ordering::Relaxed);
        gate.operations.fetch_add(1, Ordering::Relaxed);
        gate.active.fetch_sub(1, Ordering::SeqCst);
        if gate.waiting.load(Ordering::SeqCst) > 0 {
            let _held = gate.lock.lock().unwrap_or_else(PoisonError::into_inner);
            gate.wake.notify_one();
        }
    }
}

/// Samples the gate every [`WINDOW`] and moves its limit, until `stop` is disconnected.
pub(crate) fn steer(gate: &Gate, stop: &Receiver<()>) {
    let mut controller = Controller::new();
    gate.set_limit(controller.threads());
    let (mut operations, mut busy) = gate.totals();
    let mut sampled = Instant::now();
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(WINDOW) {
        let now = Instant::now();
        let (total_operations, total_busy) = gate.totals();
        let window = Window {
            operations: total_operations - operations,
            elapsed: now - sampled,
            busy: Duration::from_nanos(total_busy - busy),
        };
        (operations, busy, sampled) = (total_operations, total_busy, now);
        gate.set_limit(controller.observe(window));
    }
}

/// What the gate did over one stretch of time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Window {
    pub operations: u64,
    pub elapsed: Duration,
    /// Time the operations spent inside the gate, summed over all threads.
    pub busy: Duration,
}

/// The median of a few windows at one level: operations per second and seconds per operation.
#[derive(Clone, Copy, Debug)]
struct Trial {
    level: usize,
    rate: f64,
    latency: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug)]
enum Plan {
    /// Measuring the starting level, which every comparison begins from.
    Baseline,
    /// Measuring a neighbor of `reference` to see whether it is better.
    Probe { reference: Trial },
    /// Staying at the level of `latest`, the newest measurement there.
    Hold { latest: Trial, trials_left: u32, next: Direction },
}

/// Hill-climbs the thread count on throughput.
///
/// It climbs a level at a time while throughput keeps rising, backs off to the lower level when
/// it stops, then holds there, now and then probing one neighbor so it follows the host as it
/// gets busy or quiet. Every decision rests on the median of several windows, a lower count wins
/// a tie, and a sharp fall in throughput with a jump in latency steps down at once.
pub(crate) struct Controller {
    level: usize,
    settle: usize,
    windows: Vec<(f64, f64)>,
    plan: Plan,
}

impl Controller {
    pub(crate) fn new() -> Self {
        Self {
            level: START_LEVEL,
            settle: SETTLE_WINDOWS,
            windows: Vec::with_capacity(TRIAL_WINDOWS),
            plan: Plan::Baseline,
        }
    }

    pub(crate) fn threads(&self) -> usize {
        LADDER[self.level]
    }

    /// Takes the next window and returns the thread count to run the one after with.
    pub(crate) fn observe(&mut self, window: Window) -> usize {
        let seconds = window.elapsed.as_secs_f64();
        // A window with no operations says nothing about the level: the work may simply have
        // been elsewhere, such as a directory being opened. It is not evidence either way.
        if window.operations == 0 || seconds <= 0.0 {
            return self.threads();
        }
        if self.settle > 0 {
            self.settle -= 1;
            return self.threads();
        }
        let operations = window.operations as f64;
        self.windows.push((operations / seconds, window.busy.as_secs_f64() / operations));
        if self.windows.len() == TRIAL_WINDOWS {
            let trial = Trial {
                level: self.level,
                rate: median(self.windows.iter().map(|window| window.0)),
                latency: median(self.windows.iter().map(|window| window.1)),
            };
            self.decide(trial);
        }
        self.threads()
    }

    fn decide(&mut self, trial: Trial) {
        match self.plan {
            Plan::Baseline => self.probe(trial, Direction::Up),
            Plan::Probe { reference } if trial.level > reference.level => {
                let gained = trial.rate > reference.rate * (1.0 + NOISE)
                    && trial.latency <= reference.latency * LATENCY_LIMIT;
                if gained && trial.level + 1 < LADDER.len() {
                    self.probe(trial, Direction::Up);
                } else if gained {
                    self.hold(trial, HOLD_TRIALS, Direction::Down);
                } else {
                    // No gain: the lower level was as good. Look below it soon, as a tie goes
                    // to fewer threads, but not before a trial there has shown it is steady.
                    // With nothing below, it is simply the level to hold.
                    let trials = if reference.level > 0 { 1 } else { HOLD_TRIALS };
                    self.hold(reference, trials, Direction::Down);
                }
            }
            Plan::Probe { reference } => {
                if trial.rate >= reference.rate * (1.0 - NOISE) {
                    // Within noise of the best seen: take the lower level, and judge the next
                    // step down against the same best, or small losses would add up.
                    let kept = Trial { rate: reference.rate.max(trial.rate), ..trial };
                    if trial.level > 0 {
                        self.probe(kept, Direction::Down);
                    } else {
                        self.hold(kept, HOLD_TRIALS, Direction::Up);
                    }
                } else {
                    self.hold(reference, HOLD_TRIALS, Direction::Up);
                }
            }
            Plan::Hold { latest, trials_left, next } => {
                let pressed = trial.rate < latest.rate * (1.0 - PRESSURE_DROP)
                    && trial.latency > latest.latency * PRESSURE_LATENCY;
                if pressed && trial.level > 0 {
                    self.probe(trial, Direction::Down);
                } else if trials_left <= 1 {
                    self.probe(trial, next);
                } else {
                    self.plan = Plan::Hold { latest: trial, trials_left: trials_left - 1, next };
                    self.windows.clear();
                }
            }
        }
    }

    /// Moves a level from `reference` in `preferred`, or the other way at the end of the ladder,
    /// and measures there.
    fn probe(&mut self, reference: Trial, preferred: Direction) {
        let up = (reference.level + 1 < LADDER.len()).then_some(reference.level + 1);
        let down = reference.level.checked_sub(1);
        let target = match preferred {
            Direction::Up => up.or(down),
            Direction::Down => down.or(up),
        }
        .expect("the ladder has several levels");
        self.plan = Plan::Probe { reference };
        self.move_to(target);
    }

    /// Returns to the level of `trial` and stays there for `trials` trials before probing `next`.
    fn hold(&mut self, trial: Trial, trials: u32, next: Direction) {
        self.plan = Plan::Hold { latest: trial, trials_left: trials, next };
        self.move_to(trial.level);
    }

    fn move_to(&mut self, level: usize) {
        if level != self.level {
            self.settle = SETTLE_WINDOWS;
        }
        self.level = level;
        self.windows.clear();
    }
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable_by(f64::total_cmp);
    values[values.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;

    /// A host whose throughput at a thread count is given by `rate`. Each window is the length
    /// of [`WINDOW`], with a few percent of repeatable noise, and with every thread inside the
    /// gate all the time, so latency follows from throughput as Little's law says.
    struct Host<F: Fn(usize) -> f64> {
        rate: F,
        seed: u64,
    }

    impl<F: Fn(usize) -> f64> Host<F> {
        fn new(rate: F) -> Self {
            Self { rate, seed: 0x2545_f491_4f6c_dd1d }
        }

        fn window(&mut self, threads: usize) -> Window {
            self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = 1.0 + ((self.seed >> 33) % 1000) as f64 / 1000.0 * 0.06 - 0.03;
            let rate = (self.rate)(threads) * noise;
            Window {
                operations: (rate * WINDOW.as_secs_f64()) as u64,
                elapsed: WINDOW,
                busy: WINDOW * u32::try_from(threads).unwrap(),
            }
        }
    }

    /// Runs `windows` windows and returns the thread count each was run with.
    fn run<F: Fn(usize) -> f64>(controller: &mut Controller, host: &mut Host<F>, windows: usize) -> Vec<usize> {
        (0..windows)
            .map(|_| {
                let threads = controller.threads();
                let next = controller.observe(host.window(threads));
                assert!(LADDER.contains(&next));
                threads
            })
            .collect()
    }

    fn share_at(history: &[usize], level: usize) -> f64 {
        history.iter().filter(|threads| **threads == level).count() as f64 / history.len() as f64
    }

    #[test]
    fn climbs_while_throughput_rises_and_settles_where_it_stops() {
        let mut host = Host::new(|threads| threads.min(8) as f64 * 1000.0);
        let mut controller = Controller::new();
        let history = run(&mut controller, &mut host, 400);
        let late = &history[200..];
        assert!(share_at(late, 8) > 0.7, "mostly at 8: {late:?}");
        assert!(late.iter().all(|threads| *threads <= 16), "probes go no further than one step: {late:?}");
    }

    #[test]
    fn ramps_up_one_level_at_a_time_from_the_small_start() {
        let mut host = Host::new(|threads| threads as f64 * 1000.0);
        let mut controller = Controller::new();
        let history = run(&mut controller, &mut host, 300);
        assert_eq!(history[0], LADDER[START_LEVEL]);
        for pair in history.windows(2) {
            let (from, to) = (
                LADDER.iter().position(|level| *level == pair[0]).unwrap(),
                LADDER.iter().position(|level| *level == pair[1]).unwrap(),
            );
            assert!(from.abs_diff(to) <= 1, "{pair:?} skips a level");
        }
        assert_eq!(*history.iter().max().unwrap(), 32, "linear scaling is followed to the top");
        assert!(share_at(&history[150..], 32) > 0.6, "and mostly stays there: {:?}", &history[150..]);
    }

    #[test]
    fn settles_at_the_peak_when_more_threads_make_it_slower() {
        let mut host = Host::new(|threads| if threads <= 8 { threads as f64 * 1000.0 } else { 8000.0 - (threads - 8) as f64 * 300.0 });
        let mut controller = Controller::new();
        let history = run(&mut controller, &mut host, 400);
        let late = &history[200..];
        assert!(share_at(late, 8) > 0.7, "mostly at 8: {late:?}");
        assert!(late.iter().all(|threads| *threads <= 16), "never climbs past the peak for long: {late:?}");
    }

    #[test]
    fn a_flat_curve_goes_to_the_fewest_threads() {
        // More threads buy nothing, so whatever limits the work is not the thread count.
        let mut host = Host::new(|_| 10_000.0);
        let mut controller = Controller::new();
        let history = run(&mut controller, &mut host, 400);
        assert!(share_at(&history[200..], LADDER[0]) > 0.7, "{:?}", &history[200..]);
    }

    #[test]
    fn steps_down_when_the_host_gets_busy() {
        let mut quiet = Host::new(|threads| threads.min(16) as f64 * 1000.0);
        let mut controller = Controller::new();
        let before = run(&mut controller, &mut quiet, 300);
        assert!(share_at(&before[150..], 16) > 0.6, "{:?}", &before[150..]);

        // Contention: past six threads every thread added costs throughput.
        let mut busy = Host::new(|threads| if threads <= 6 { threads as f64 * 1000.0 } else { 6000.0 - (threads - 6) as f64 * 400.0 });
        let after = run(&mut controller, &mut busy, 200);
        assert!(after[..60].iter().any(|threads| *threads <= 8), "backs off within a few seconds: {after:?}");
        // It still probes the level above now and then, to notice the host quieting down.
        assert!(share_at(&after[100..], 16) < 0.15, "does not stay at 16: {after:?}");
        assert!(after[100..].iter().filter(|threads| **threads <= 8).count() * 10 > after[100..].len() * 8);
    }

    #[test]
    fn climbs_again_when_the_host_quiets_down() {
        let mut busy = Host::new(|threads| threads.min(4) as f64 * 1000.0);
        let mut controller = Controller::new();
        run(&mut controller, &mut busy, 300);
        let mut quiet = Host::new(|threads| threads.min(16) as f64 * 1000.0);
        let history = run(&mut controller, &mut quiet, 800);
        assert!(share_at(&history[500..], 16) > 0.6, "{:?}", &history[500..]);
    }

    #[test]
    fn one_spiking_window_is_not_a_gain() {
        let at = |operations: u64, threads: u32| Window { operations, elapsed: WINDOW, busy: WINDOW * threads };
        let mut controller = Controller::new();
        let start = controller.threads();
        // The settle window, then the baseline at the start level.
        for _ in 0..1 + TRIAL_WINDOWS {
            controller.observe(at(1000, 4));
        }
        let probed = controller.threads();
        assert!(probed > start, "the baseline is followed by a step up");
        // At the step up, one window in three is five times faster: its mean says "gain", its
        // median says nothing changed, and the median decides.
        for operations in [1000, 1000, 1000, 5000] {
            controller.observe(at(operations, 6));
        }
        assert_eq!(controller.threads(), start, "back to the lower level");
    }

    #[test]
    fn concurrency_is_fixed_unless_adaptive_is_asked_for_and_the_deletion_is_large() {
        let large = ADAPTIVE_MIN_OPERATIONS;
        assert_eq!(Concurrency::choose(None, large * 10), Concurrency::Fixed(DEFAULT_THREADS));
        assert_eq!(Concurrency::choose(Some("adaptive"), large), Concurrency::Adaptive);
        assert_eq!(Concurrency::choose(Some("adaptive"), large - 1), Concurrency::Fixed(DEFAULT_THREADS));
        assert_eq!(Concurrency::choose(Some("6"), 10), Concurrency::Fixed(6));
        assert_eq!(Concurrency::choose(Some("6"), large * 10), Concurrency::Fixed(6));
        for unusable in ["0", "-3", "many", ""] {
            assert_eq!(Concurrency::choose(Some(unusable), large), Concurrency::Fixed(DEFAULT_THREADS), "{unusable:?}");
        }
    }

    #[test]
    fn windows_without_operations_are_not_evidence() {
        let mut controller = Controller::new();
        let start = controller.threads();
        for _ in 0..100 {
            let idle = Window { operations: 0, elapsed: WINDOW, busy: Duration::ZERO };
            assert_eq!(controller.observe(idle), start);
        }
    }

    #[test]
    fn gate_never_lets_more_than_the_limit_in() {
        let gate = Arc::new(Gate::new(3));
        let inside = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let threads = (0..12)
            .map(|_| {
                let (gate, inside, most) = (Arc::clone(&gate), Arc::clone(&inside), Arc::clone(&most));
                thread::spawn(move || {
                    for _ in 0..200 {
                        let _permit = gate.enter();
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(most.load(Ordering::SeqCst) <= 3);
        assert_eq!(gate.totals().0, 12 * 200);
    }

    #[test]
    fn a_waiter_is_woken_when_a_permit_is_returned_or_the_limit_rises() {
        let gate = Arc::new(Gate::new(1));
        let held = gate.enter();
        let (sender, receiver) = mpsc::channel();
        let waiter = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let _permit = gate.enter();
                sender.send(()).unwrap();
            })
        };
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err(), "blocked at the limit");
        drop(held);
        receiver.recv_timeout(Duration::from_secs(5)).expect("woken by the returned permit");
        waiter.join().unwrap();

        let held = gate.enter();
        let (sender, receiver) = mpsc::channel();
        let waiter = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let _permit = gate.enter();
                sender.send(()).unwrap();
            })
        };
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        gate.set_limit(2);
        receiver.recv_timeout(Duration::from_secs(5)).expect("woken by the raised limit");
        waiter.join().unwrap();
        drop(held);
    }

    #[test]
    fn lowering_the_limit_holds_back_new_entries_without_evicting_anyone() {
        let gate = Gate::new(4);
        let permits = (0..4).map(|_| gate.enter()).collect::<Vec<_>>();
        gate.set_limit(1);
        assert_eq!(gate.limit(), 1);
        assert!(!gate.try_enter(), "full");
        drop(permits);
        assert!(gate.try_enter(), "empty again, and the limit of one lets one in");
        assert!(!gate.try_enter());
        gate.active.fetch_sub(1, Ordering::SeqCst);
    }

    #[test]
    fn steering_stops_when_the_sender_is_dropped() {
        let gate = Arc::new(Gate::new(1));
        let (sender, receiver) = mpsc::channel::<()>();
        let done = Arc::new(AtomicBool::new(false));
        let steering = {
            let (gate, done) = (Arc::clone(&gate), Arc::clone(&done));
            thread::spawn(move || {
                steer(&gate, &receiver);
                done.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        while gate.limit() != LADDER[START_LEVEL] {
            assert!(started.elapsed() < Duration::from_secs(5), "starts small");
            thread::yield_now();
        }
        drop(sender);
        steering.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }
}
