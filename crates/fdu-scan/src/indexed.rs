//! Platform-independent orchestration of the indexed scan: the worker queue,
//! directory completion tracking, and event batching. Each platform supplies
//! directory enumeration, mount identity, and worker sizing.
use crate::fsutil::{open_directory_at, stat_entry_at};
use crate::{DirectoryToken, EntryBatch, RootAnchor, ScanQueueMetrics};
use fdu_core::{EntryType, ExclusionReason, FileIdentity, NodeState, ScanEntry, ScanEvent};
use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use std::cell::RefCell;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

#[cfg(target_os = "linux")]
use crate::linux::platform::{
    describe_directory, indexed_scan_limits, mount_identity, scan_hints, with_indexed_directory_buffer,
    DirectoryBuffer, IndexedDirectoryStream, MountIdentity, ScanHints, StreamEntry,
};
#[cfg(target_os = "macos")]
use crate::macos::{
    describe_directory, indexed_scan_limits, mount_identity, scan_hints, with_indexed_directory_buffer,
    DirectoryBuffer, IndexedDirectoryStream, MountIdentity, ScanHints, StreamEntry,
};

/// A worker publishes its entries once it has gathered this many, which is also the capacity of
/// a batch.
const PUBLISH_ENTRIES: usize = 1024;
const BATCH_NAME_BYTES: usize = PUBLISH_ENTRIES * 24;
/// Finished directories are released in groups this large at most.
const PUBLISH_RELEASES: usize = 256;
/// Directories a worker may hold open and unpublished.
const HELD_OPEN_DIRECTORIES: usize = 64;
const DIRECTORY_FINISH_BATCH_SIZE: usize = 256;
const SEEN_DIRECTORY_SHARDS: usize = 64;

/// Metadata that a platform's directory enumeration returns together with a
/// name, sparing a separate stat call. Directories report only identity and type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) struct BulkMetadata {
    pub(crate) identity: FileIdentity,
    pub(crate) entry_type: EntryType,
    pub(crate) link_count: u64,
    pub(crate) apparent_bytes: u64,
    pub(crate) allocated_bytes: u64,
}

/// What enumeration alone says about an entry's type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryHint {
    Directory,
    NotDirectory,
    Unknown,
}

/// An opened directory's identity and whether it is on the scan root's mount.
pub(crate) struct DirectoryFacts {
    pub(crate) identity: FileIdentity,
    pub(crate) link_count: u64,
    pub(crate) same_mount: bool,
}

/// Worker-pool sizing chosen by the platform for its filesystem and descriptor limits.
pub(crate) struct IndexedScanLimits {
    /// Threads that exist for the scan.
    pub(crate) workers: usize,
    /// How many of them work from the start. The rest wait until the scan is seen to spend its
    /// time blocked on storage, where extra threads add throughput; on a warm cache they would
    /// only compete with the others.
    pub(crate) base_workers: usize,
    /// Directories that may wait with an open descriptor for a worker.
    pub(crate) queued_directories: usize,
}

struct ScanEventSender<'a> {
    sender: &'a SyncSender<ScanEvent>,
    metrics: &'a ScanQueueMetrics,
}

impl ScanEventSender<'_> {
    fn send(&self, event: ScanEvent) -> Result<(), ScanEvent> {
        self.metrics.send(self.sender, event)
    }
}

enum IndexedDirectorySource {
    Opened(RootAnchor),
    ReopenFromParent(Arc<OwnedFd>),
}

struct IndexedDirectoryTask {
    source: IndexedDirectorySource,
    name: Vec<u8>,
    identity: FileIdentity,
    token: DirectoryToken,
}

const NO_PARENT: u32 = u32::MAX;
const PROGRESS_CHUNK_BITS: u32 = 16;
const PROGRESS_CHUNK: usize = 1 << PROGRESS_CHUNK_BITS;
const PROGRESS_CHUNKS: usize = 1 << (32 - PROGRESS_CHUNK_BITS);

/// Completion state of one directory. `pending` counts the holds that keep it unfinished: one
/// for its own enumeration and one per child that has not finished.
struct DirectoryProgress {
    parent: AtomicU32,
    pending: AtomicU32,
    incomplete: AtomicBool,
}

impl DirectoryProgress {
    const fn new() -> Self {
        Self {
            parent: AtomicU32::new(NO_PARENT),
            pending: AtomicU32::new(0),
            incomplete: AtomicBool::new(false),
        }
    }
}

/// Tokens are dense integers, so progress lives in a chunked table that grows without locking.
struct ProgressTable {
    chunks: Box<[OnceLock<Box<[DirectoryProgress]>>]>,
}

impl ProgressTable {
    fn new() -> Self {
        Self {
            chunks: (0..PROGRESS_CHUNKS).map(|_| OnceLock::new()).collect(),
        }
    }

    fn slot(&self, token: DirectoryToken) -> &DirectoryProgress {
        let index = token.0 as usize;
        let chunk = self.chunks[index >> PROGRESS_CHUNK_BITS]
            .get_or_init(|| (0..PROGRESS_CHUNK).map(|_| DirectoryProgress::new()).collect());
        &chunk[index & (PROGRESS_CHUNK - 1)]
    }
}

struct ScanProgress<'a> {
    table: ProgressTable,
    /// Finished directories not yet sent. The list is appended to before a directory releases
    /// its parent, so a parent is never listed ahead of a child, and it is sent under its lock
    /// so batches reach the channel in list order.
    pending_finishes: Mutex<Vec<(DirectoryToken, bool)>>,
    sender: &'a ScanEventSender<'a>,
}

impl ScanProgress<'_> {
    fn register_root(&self) {
        self.table.slot(DirectoryToken(0)).pending.store(1, Ordering::Relaxed);
    }

    fn register_child(&self, parent: DirectoryToken, child: DirectoryToken) {
        let child_slot = self.table.slot(child);
        child_slot.parent.store(parent.0, Ordering::Relaxed);
        child_slot.incomplete.store(false, Ordering::Relaxed);
        child_slot.pending.store(1, Ordering::Relaxed);
        let previous = self.table.slot(parent).pending.fetch_add(1, Ordering::Relaxed);
        assert!(previous != 0, "directory parent was registered before its children");
    }

    /// Releases the enumeration hold of `start`, then releases each parent whose last hold this
    /// was. Returns false when an event could not be delivered.
    fn finish_directory(&self, start: DirectoryToken, complete: bool) -> bool {
        let mut token = start;
        let mut complete = complete;
        loop {
            let slot = self.table.slot(token);
            if !complete {
                slot.incomplete.store(true, Ordering::Relaxed);
            }
            // A directory that already finished has no holds left; ignore repeated releases.
            let released = slot
                .pending
                .try_update(Ordering::AcqRel, Ordering::Acquire, |holds| holds.checked_sub(1));
            if released != Ok(1) {
                return true;
            }
            let directory_complete = !slot.incomplete.load(Ordering::Relaxed);
            if !self.push_finished(token, directory_complete) {
                return false;
            }
            let parent = slot.parent.load(Ordering::Relaxed);
            if parent == NO_PARENT {
                return true;
            }
            token = DirectoryToken(parent);
            complete = directory_complete;
        }
    }

    fn push_finished(&self, token: DirectoryToken, complete: bool) -> bool {
        let mut pending = self
            .pending_finishes
            .lock()
            .expect("pending directory finishes are not poisoned");
        pending.push((token, complete));
        if pending.len() < DIRECTORY_FINISH_BATCH_SIZE {
            return true;
        }
        let batch = std::mem::replace(&mut *pending, Vec::with_capacity(DIRECTORY_FINISH_BATCH_SIZE));
        self.sender.send(ScanEvent::DirectoriesFinished(batch)).is_ok()
    }

    fn flush_finishes(&self) -> bool {
        let mut pending = self
            .pending_finishes
            .lock()
            .expect("pending directory finishes are not poisoned");
        if pending.is_empty() {
            return true;
        }
        let batch = std::mem::take(&mut *pending);
        self.sender.send(ScanEvent::DirectoriesFinished(batch)).is_ok()
    }
}

/// Directories already entered, to skip a directory reachable by two paths. Sharded so
/// workers rarely meet on a lock.
struct SeenDirectories {
    shards: Box<[Mutex<foldhash::HashSet<FileIdentity>>]>,
}

impl SeenDirectories {
    fn new() -> Self {
        Self {
            shards: (0..SEEN_DIRECTORY_SHARDS).map(|_| Mutex::new(foldhash::HashSet::default())).collect(),
        }
    }

    /// Returns true when the directory was not seen before.
    fn insert(&self, identity: FileIdentity) -> bool {
        let mixed = (identity.inode ^ identity.device.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let shard = (mixed >> 32) as usize % SEEN_DIRECTORY_SHARDS;
        self.shards[shard]
            .lock()
            .expect("directory identity set is not poisoned")
            .insert(identity)
    }
}

thread_local! {
    /// The deque of the scan worker running on this thread; the root thread has none.
    static LOCAL_TASKS: RefCell<Option<Worker<IndexedDirectoryTask>>> = const { RefCell::new(None) };
}

/// Directory tasks shared by the workers. Each worker keeps the tasks it creates in its own
/// deque, newest first, so it finishes one subtree before starting another and holds few
/// descriptors; idle workers steal the oldest tasks, which are the largest subtrees. The root
/// thread's tasks go through a shared injector.
///
/// `queued` counts tasks sitting in any deque and bounds how many descriptors wait for a
/// worker. `pending` counts queued plus running tasks and the root's own hold, so the queue is
/// finished exactly when it reaches zero.
struct IndexedWorkQueue {
    injector: Injector<IndexedDirectoryTask>,
    stealers: Box<[OnceLock<Stealer<IndexedDirectoryTask>>]>,
    queued: AtomicUsize,
    pending: AtomicUsize,
    closed: AtomicBool,
    sleepers: AtomicUsize,
    base_workers: usize,
    extra_workers_open: AtomicBool,
    samples: AtomicUsize,
    blocked_samples: AtomicUsize,
    park: Mutex<()>,
    /// Wakes workers that ran out of work. Separate from `gate`, so that a wake-up meant for one
    /// of them is never spent on a worker that is only waiting to be needed.
    wake: Condvar,
    /// Wakes the extra workers when they are needed.
    gate: Condvar,
    capacity: usize,
}

impl IndexedWorkQueue {
    fn new(capacity: usize, workers: usize, base_workers: usize) -> Self {
        Self {
            injector: Injector::new(),
            stealers: (0..workers).map(|_| OnceLock::new()).collect(),
            queued: AtomicUsize::new(0),
            // The root's hold, released by `finish_root`.
            pending: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
            sleepers: AtomicUsize::new(0),
            base_workers: base_workers.min(workers),
            extra_workers_open: AtomicBool::new(base_workers >= workers),
            samples: AtomicUsize::new(0),
            blocked_samples: AtomicUsize::new(0),
            park: Mutex::new(()),
            wake: Condvar::new(),
            gate: Condvar::new(),
            capacity,
        }
    }

    /// Gives the calling worker thread its deque. Must precede its first `next_task`.
    fn register_worker(&self, index: usize) {
        let local = Worker::new_lifo();
        let _ = self.stealers[index].set(local.stealer());
        LOCAL_TASKS.with(|slot| *slot.borrow_mut() = Some(local));
    }

    fn try_push(&self, task: IndexedDirectoryTask) -> Result<(), IndexedDirectoryTask> {
        if self.closed.load(Ordering::Relaxed) || self.queued.load(Ordering::Relaxed) >= self.capacity {
            return Err(task);
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        self.queued.fetch_add(1, Ordering::SeqCst);
        LOCAL_TASKS.with(|slot| match slot.borrow().as_ref() {
            Some(local) => local.push(task),
            None => self.injector.push(task),
        });
        // A sleeper registers before it looks for work, and a pusher publishes before it looks
        // for sleepers, so one of the two always sees the other.
        if self.sleepers.load(Ordering::SeqCst) > 0 {
            drop(self.park.lock().expect("indexed work queue is not poisoned"));
            self.wake.notify_one();
        }
        Ok(())
    }

    fn steal(&self, local: &Worker<IndexedDirectoryTask>, thief: usize) -> Option<IndexedDirectoryTask> {
        loop {
            let mut retry = false;
            match self.injector.steal_batch_and_pop(local) {
                Steal::Success(task) => return Some(task),
                Steal::Retry => retry = true,
                Steal::Empty => {}
            }
            for offset in 1..=self.stealers.len() {
                let Some(stealer) = self.stealers[(thief + offset) % self.stealers.len()].get() else {
                    continue;
                };
                match stealer.steal_batch_and_pop(local) {
                    Steal::Success(task) => return Some(task),
                    Steal::Retry => retry = true,
                    Steal::Empty => {}
                }
            }
            if !retry {
                return None;
            }
        }
    }

    /// The next task for the worker `index`, or `None` once the scan is finished or cancelled.
    fn next_task(&self, index: usize, cancelled: &AtomicBool) -> Option<IndexedDirectoryTask> {
        LOCAL_TASKS.with(|slot| {
            let slot = slot.borrow();
            let local = slot.as_ref().expect("the worker registered its deque");
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    self.close();
                }
                if self.closed.load(Ordering::Acquire) {
                    return None;
                }
                if index >= self.base_workers && !self.extra_workers_open.load(Ordering::Acquire) {
                    // Not yet needed. Unlike a worker that has run out of work, this one does not
                    // count as starving.
                    let guard = self.park.lock().expect("indexed work queue is not poisoned");
                    if !self.extra_workers_open.load(Ordering::Acquire)
                        && !self.closed.load(Ordering::Acquire)
                        && !cancelled.load(Ordering::Relaxed)
                    {
                        drop(self.gate.wait(guard).expect("indexed work queue is not poisoned"));
                    }
                    continue;
                }
                if let Some(task) = local.pop().or_else(|| self.steal(local, index)) {
                    self.queued.fetch_sub(1, Ordering::SeqCst);
                    return Some(task);
                }
                let guard = self.park.lock().expect("indexed work queue is not poisoned");
                self.sleepers.fetch_add(1, Ordering::SeqCst);
                let unfinished_work = self.queued.load(Ordering::SeqCst) > 0
                    || self.closed.load(Ordering::Acquire)
                    || cancelled.load(Ordering::Relaxed);
                let guard = if unfinished_work {
                    guard
                } else {
                    self.wake.wait(guard).expect("indexed work queue is not poisoned")
                };
                self.sleepers.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        })
    }

    fn release_hold(&self) {
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.close();
        }
    }

    fn finish_task(&self) {
        self.release_hold();
    }

    fn finish_root(&self) {
        self.release_hold();
    }

    fn cancel(&self) {
        self.close();
    }

    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            drop(self.park.lock().expect("indexed work queue is not poisoned"));
            self.wake.notify_all();
            self.gate.notify_all();
        }
    }

    /// Whether the scan is still sampling how much of its time goes to waiting.
    fn wants_samples(&self) -> bool {
        !self.extra_workers_open.load(Ordering::Relaxed)
    }

    /// Records whether a sampled directory spent most of its time blocked rather than running.
    /// Once a good share of a fair number of samples were blocked, the extra workers start.
    fn note_sample(&self, blocked: bool) {
        const MINIMUM_SAMPLES: usize = 32;
        let samples = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        let blocked_samples = if blocked {
            self.blocked_samples.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.blocked_samples.load(Ordering::Relaxed)
        };
        if samples >= MINIMUM_SAMPLES
            && blocked_samples * 4 >= samples
            && !self.extra_workers_open.swap(true, Ordering::AcqRel)
        {
            drop(self.park.lock().expect("indexed work queue is not poisoned"));
            self.gate.notify_all();
        }
    }

    /// Whether a worker is idle with nothing published for it to take.
    fn is_starving(&self) -> bool {
        self.sleepers.load(Ordering::Relaxed) > 0 && self.queued.load(Ordering::Relaxed) == 0
    }
}

struct ScanContext<'a> {
    sender: &'a ScanEventSender<'a>,
    returned_batches: &'a Mutex<Receiver<EntryBatch>>,
    cancelled: &'a AtomicBool,
    cancelled_event_sent: &'a AtomicBool,
    seen_directories: &'a SeenDirectories,
    next_token: &'a AtomicUsize,
    root_mount: &'a MountIdentity,
    hints: ScanHints,
    progress: &'a ScanProgress<'a>,
    queue: &'a IndexedWorkQueue,
}

/// A directory the worker found but has not published yet.
struct HeldTask {
    task: IndexedDirectoryTask,
    /// The directory it was found in, to reopen it from if its own descriptor is released.
    parent: Option<Arc<OwnedFd>>,
}

/// What a worker has found but not yet sent. Entries of consecutive directories share one batch,
/// so the consumer sees a few large messages instead of one per directory. Everything is
/// published in an order the consumer relies on:
///
/// 1. entries, so a directory's own entry is known before anything found inside it;
/// 2. notices about those directories;
/// 3. the directories found, made available to other workers, which can only then send entries
///    of their own;
/// 4. releases of finished directories, which can complete their parents. A directory is
///    released only after its entries are sent, so a directory's completion is always sent after
///    the entries of everything below it.
struct Outbox {
    batch: EntryBatch,
    notices: Vec<ScanEvent>,
    releases: Vec<(DirectoryToken, bool)>,
    /// Directories to scan next, newest last. This worker takes them first, which keeps it inside
    /// one subtree and its descriptors few.
    held: Vec<HeldTask>,
    held_open: usize,
    directories_scanned: u32,
}

impl Outbox {
    fn new() -> Self {
        Self {
            batch: EntryBatch::with_capacity(PUBLISH_ENTRIES + PUBLISH_ENTRIES / 4, BATCH_NAME_BYTES),
            notices: Vec::new(),
            releases: Vec::new(),
            held: Vec::new(),
            held_open: 0,
            directories_scanned: 0,
        }
    }

    fn should_flush(&self, context: &ScanContext<'_>) -> bool {
        self.batch.entries.len() >= PUBLISH_ENTRIES
            || self.releases.len() >= PUBLISH_RELEASES
            || self.held_open >= HELD_OPEN_DIRECTORIES
            // An idle worker needs the directories this one is holding.
            || (!self.held.is_empty() && context.queue.is_starving())
    }

    fn pop_held(&mut self) -> Option<IndexedDirectoryTask> {
        let held = self.held.pop()?;
        if matches!(held.task.source, IndexedDirectorySource::Opened(_)) {
            self.held_open -= 1;
        }
        Some(held.task)
    }

    /// Sends everything gathered, in the order described above. False means the receiver is gone.
    fn flush(&mut self, context: &ScanContext<'_>) -> bool {
        if !self.batch.entries.is_empty() {
            let next = take_batch(context.returned_batches);
            let entries = std::mem::replace(&mut self.batch, next);
            if context.sender.send(ScanEvent::Entries(entries)).is_err() {
                return false;
            }
        }
        for notice in self.notices.drain(..) {
            if context.sender.send(notice).is_err() {
                return false;
            }
        }
        // Directories the queue has no room for lose their descriptor and are reopened later by
        // this worker, so the descriptors held never exceed the queue's bound.
        let mut kept = Vec::new();
        for mut held in self.held.drain(..) {
            match context.queue.try_push(held.task) {
                Ok(()) => {}
                Err(mut task) => {
                    if let Some(parent) = held.parent.take() {
                        task.source = IndexedDirectorySource::ReopenFromParent(parent);
                    }
                    kept.push(HeldTask { task, parent: None });
                }
            }
        }
        self.held = kept;
        self.held_open = 0;
        for (token, complete) in self.releases.drain(..) {
            if !context.progress.finish_directory(token, complete) {
                return false;
            }
        }
        true
    }
}

/// A recycled batch when one is free without waiting, otherwise a fresh one.
fn take_batch(returned_batches: &Mutex<Receiver<EntryBatch>>) -> EntryBatch {
    let recycled = returned_batches
        .try_lock()
        .ok()
        .and_then(|returned| returned.try_recv().ok());
    match recycled {
        Some(mut batch) => {
            batch.clear();
            batch
        }
        None => EntryBatch::with_capacity(PUBLISH_ENTRIES + PUBLISH_ENTRIES / 4, BATCH_NAME_BYTES),
    }
}

pub(crate) fn scan_indexed(
    anchor: &RootAnchor,
    sender: SyncSender<ScanEvent>,
    returned_batches: Receiver<EntryBatch>,
    cancelled: Arc<AtomicBool>,
    metrics: ScanQueueMetrics,
) {
    let event_sender = ScanEventSender {
        sender: &sender,
        metrics: &metrics,
    };
    let fail = |message: String| {
        let _ = event_sender.send(ScanEvent::Failed { message });
        let _ = event_sender.send(ScanEvent::Finished);
    };
    let root_identity = match crate::fsutil::identity_of(anchor.as_fd()) {
        Ok(identity) if identity == anchor.identity => identity,
        Ok(_) => return fail("scan root identity changed after it was opened".to_owned()),
        Err(error) => return fail(error.to_string()),
    };
    let root_mount = match mount_identity(anchor.as_fd()) {
        Ok(identity) => identity,
        Err(error) => return fail(error.to_string()),
    };
    if event_sender
        .send(ScanEvent::Started {
            root_name: anchor.name.clone(),
            identity: root_identity,
            link_count: anchor.link_count,
        })
        .is_err()
    {
        let _ = event_sender.send(ScanEvent::Finished);
        return;
    }

    let returned_batches = Mutex::new(returned_batches);
    let seen_directories = SeenDirectories::new();
    seen_directories.insert(root_identity);
    let next_token = AtomicUsize::new(1);
    let cancelled_event_sent = AtomicBool::new(false);
    let progress = ScanProgress {
        table: ProgressTable::new(),
        pending_finishes: Mutex::new(Vec::with_capacity(DIRECTORY_FINISH_BATCH_SIZE)),
        sender: &event_sender,
    };
    progress.register_root();
    let limits = indexed_scan_limits();
    let worker_count = limits.workers.max(1);
    let queue = IndexedWorkQueue::new(limits.queued_directories.max(1), worker_count, limits.base_workers.max(1));
    let context = ScanContext {
        sender: &event_sender,
        returned_batches: &returned_batches,
        cancelled: &cancelled,
        cancelled_event_sent: &cancelled_event_sent,
        seen_directories: &seen_directories,
        next_token: &next_token,
        root_mount: &root_mount,
        hints: scan_hints(anchor.as_fd()),
        progress: &progress,
        queue: &queue,
    };

    thread::scope(|scope| {
        let context = &context;
        let workers = (0..worker_count)
            .map(|index| scope.spawn(move || worker_loop(index, context)))
            .collect::<Vec<_>>();
        // The root is scanned like any other directory, on this thread, which then shares the
        // work its own subtree holds with the workers.
        let root_task = IndexedDirectoryTask {
            source: IndexedDirectorySource::Opened(anchor.clone()),
            name: Vec::new(),
            identity: root_identity,
            token: DirectoryToken(0),
        };
        if !run_task_tree(root_task, context, &mut Outbox::new()) {
            queue.cancel();
        }
        queue.finish_root();
        for worker in workers {
            if worker.join().is_err() {
                let _ = event_sender.send(ScanEvent::Failed {
                    message: "indexed scan worker panicked".to_owned(),
                });
                let _ = progress.finish_directory(DirectoryToken(0), false);
                queue.cancel();
            }
        }
    });
    let _ = progress.flush_finishes();
    let _ = event_sender.send(ScanEvent::Finished);
}

fn worker_loop(index: usize, context: &ScanContext<'_>) {
    context.queue.register_worker(index);
    let mut outbox = Outbox::new();
    while let Some(task) = context.queue.next_task(index, context.cancelled) {
        if !run_task_tree(task, context, &mut outbox) {
            context.queue.cancel();
        }
        context.queue.finish_task();
    }
}

/// Scans `task` and every directory found beneath it that this worker keeps for itself, then
/// publishes everything. The caller still holds the queue's claim on `task` until this returns,
/// so the scan cannot be seen as finished while directories are held here. False means the scan
/// should stop.
fn run_task_tree(
    task: IndexedDirectoryTask,
    context: &ScanContext<'_>,
    outbox: &mut Outbox,
) -> bool {
    let mut next = Some(task);
    while let Some(task) = next.take() {
        if !scan_indexed_task(task, context, outbox) {
            let _ = outbox.flush(context);
            return false;
        }
        next = outbox.pop_held();
    }
    outbox.flush(context)
}

/// Opens the task's directory and scans it. False means the scan should stop.
fn scan_indexed_task(
    task: IndexedDirectoryTask,
    context: &ScanContext<'_>,
    outbox: &mut Outbox,
) -> bool {
    let directory = match task.source {
        IndexedDirectorySource::Opened(anchor) => anchor.fd,
        IndexedDirectorySource::ReopenFromParent(parent) => {
            let child = match open_directory_at(parent.as_fd(), task.name.as_slice()) {
                Ok(child) => child,
                Err(error) => return failed_task(task.token, error.to_string(), context, outbox),
            };
            match describe_directory(child.as_fd(), context.root_mount) {
                Ok(facts) if facts.identity != task.identity => {
                    return failed_task(
                        task.token,
                        "directory identity changed before scanning".to_owned(),
                        context,
                        outbox,
                    );
                }
                Ok(facts) if !facts.same_mount => {
                    outbox.notices.push(ScanEvent::DirectoryExcluded {
                        directory: task.token,
                        reason: ExclusionReason::MountBoundary,
                    });
                    outbox.releases.push((task.token, false));
                    return true;
                }
                Ok(_) => {}
                Err(error) => return failed_task(task.token, error.to_string(), context, outbox),
            }
            Arc::new(child)
        }
    };
    match scan_indexed_directory(&directory, task.token, context, outbox) {
        Ok(keep_running) => keep_running,
        Err(error) => failed_task(task.token, error.to_string(), context, outbox),
    }
}

fn failed_task(
    token: DirectoryToken,
    message: String,
    context: &ScanContext<'_>,
    outbox: &mut Outbox,
) -> bool {
    outbox.notices.push(ScanEvent::DirectoryFailed {
        directory: token,
        message,
    });
    outbox.releases.push((token, false));
    !outbox.should_flush(context) || outbox.flush(context)
}

fn scan_indexed_directory(
    directory: &Arc<OwnedFd>,
    token: DirectoryToken,
    context: &ScanContext<'_>,
    outbox: &mut Outbox,
) -> io::Result<bool> {
    with_indexed_directory_buffer(|buffer| {
        scan_indexed_directory_with_buffer(directory, token, context, buffer, outbox)
    })
}

/// Everything the scanner learned about one directory entry.
struct ClassifiedEntry {
    entry_type: EntryType,
    identity: FileIdentity,
    link_count: u64,
    apparent_bytes: u64,
    allocated_bytes: u64,
    state: NodeState,
    child_token: Option<DirectoryToken>,
    /// At most one problem is reported per entry.
    failure: Option<String>,
}

impl ClassifiedEntry {
    fn incomplete(&mut self, message: impl ToString) {
        self.state = NodeState::Incomplete;
        self.failure = Some(message.to_string());
    }
}

fn classify_entry(
    entry: &StreamEntry<'_>,
    directory: &Arc<OwnedFd>,
    token: DirectoryToken,
    context: &ScanContext<'_>,
    outbox: &mut Outbox,
) -> ClassifiedEntry {
    let directory_fd = directory.as_fd();
    let mut found = ClassifiedEntry {
        entry_type: EntryType::Other,
        identity: FileIdentity { device: 0, inode: 0 },
        link_count: 0,
        apparent_bytes: 0,
        allocated_bytes: 0,
        state: NodeState::Complete,
        child_token: None,
        failure: None,
    };
    // The identity seen before a directory is opened, to detect it being replaced in between.
    let mut expected_identity = None;
    match (entry.bulk(), entry.hint()) {
        (Some(bulk), _) => {
            found.identity = bulk.identity;
            found.link_count = bulk.link_count;
            found.entry_type = bulk.entry_type;
            expected_identity = Some(bulk.identity);
            if bulk.entry_type != EntryType::Directory {
                found.apparent_bytes = bulk.apparent_bytes;
                found.allocated_bytes = bulk.allocated_bytes;
            }
        }
        // Enumeration says directory, so opening it yields everything a stat would.
        (None, EntryHint::Directory) => found.entry_type = EntryType::Directory,
        (None, _) => match stat_entry_at(directory_fd, entry.name()) {
            Ok(stat) => {
                found.identity = stat.identity;
                found.link_count = stat.link_count;
                found.entry_type = stat.entry_type;
                expected_identity = Some(stat.identity);
                if stat.entry_type != EntryType::Directory {
                    match (stat.apparent_bytes, stat.allocated_bytes) {
                        (Ok(apparent), Ok(allocated)) => {
                            found.apparent_bytes = apparent;
                            found.allocated_bytes = allocated;
                        }
                        (Err(error), _) | (_, Err(error)) => found.incomplete(error),
                    }
                }
            }
            Err(error) => {
                found.incomplete(error);
                return found;
            }
        },
    }
    if found.entry_type != EntryType::Directory {
        return found;
    }

    let child = match open_directory_at(directory_fd, entry.name()) {
        Ok(child) => child,
        Err(error) => {
            found.incomplete(error);
            return found;
        }
    };
    let facts = match describe_directory(child.as_fd(), context.root_mount) {
        Ok(facts) => facts,
        Err(error) => {
            found.incomplete(error);
            return found;
        }
    };
    found.link_count = facts.link_count;
    if expected_identity.is_some_and(|expected| expected != facts.identity) {
        found.incomplete("directory identity changed during scan");
        return found;
    }
    found.identity = facts.identity;
    if !facts.same_mount {
        found.state = NodeState::Excluded(ExclusionReason::MountBoundary);
    } else if !context.seen_directories.insert(facts.identity) {
        found.state = NodeState::Excluded(ExclusionReason::UnsupportedAlias);
    } else {
        let child_token = match allocate_directory_token(context.next_token) {
            Ok(child_token) => child_token,
            Err(error) => {
                found.incomplete(error);
                return found;
            }
        };
        context.progress.register_child(token, child_token);
        found.child_token = Some(child_token);
        outbox.held.push(HeldTask {
            task: IndexedDirectoryTask {
                source: IndexedDirectorySource::Opened(RootAnchor {
                    fd: Arc::new(child),
                    name: Vec::new(),
                    identity: facts.identity,
                    link_count: facts.link_count,
                }),
                name: entry.name().to_bytes().to_vec(),
                identity: facts.identity,
                token: child_token,
            },
            parent: Some(Arc::clone(directory)),
        });
        outbox.held_open += 1;
    }
    found
}

/// Scans one directory into the outbox. Returns false when the scan should stop.
fn scan_indexed_directory_with_buffer(
    directory: &Arc<OwnedFd>,
    token: DirectoryToken,
    context: &ScanContext<'_>,
    buffer: &mut DirectoryBuffer,
    outbox: &mut Outbox,
) -> io::Result<bool> {
    if context.cancelled.load(Ordering::Relaxed) {
        send_cancelled_once(context);
        return Ok(false);
    }
    let sample = DirectorySample::start(context, outbox);
    let mut stream = IndexedDirectoryStream::new(directory.as_fd(), buffer, context.hints);
    let mut complete = true;

    loop {
        if context.cancelled.load(Ordering::Relaxed) {
            outbox.releases.push((token, false));
            let _ = outbox.flush(context);
            send_cancelled_once(context);
            return Ok(false);
        }
        let entry = match stream.next_entry() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                complete = false;
                outbox.notices.push(ScanEvent::DirectoryFailed {
                    directory: token,
                    message: error.to_string(),
                });
                break;
            }
        };
        let name_bytes = entry.name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let mut found = classify_entry(&entry, directory, token, context, outbox);
        if found.state != NodeState::Complete {
            complete = false;
        }
        if let Some(message) = found.failure.take() {
            complete = false;
            outbox.notices.push(ScanEvent::DirectoryFailed {
                directory: token,
                message,
            });
        }

        let batch = &mut outbox.batch;
        let Ok(name_start) = u32::try_from(batch.names.len()) else {
            complete = false;
            outbox.notices.push(ScanEvent::DirectoryFailed {
                directory: token,
                message: "directory batch exceeded supported name storage".to_owned(),
            });
            break;
        };
        let name_length = u32::try_from(name_bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "filename is too long"))?;
        let Some(name_end) = name_start.checked_add(name_length) else {
            complete = false;
            break;
        };
        batch.names.extend_from_slice(name_bytes);
        batch.entries.push(ScanEntry {
            parent: token,
            name: name_start..name_end,
            directory_token: found.child_token,
            entry_type: found.entry_type,
            identity: found.identity,
            link_count: found.link_count,
            apparent_bytes: found.apparent_bytes,
            allocated_bytes: found.allocated_bytes,
            state: found.state,
        });
        // A very wide directory is published as it is read.
        if outbox.batch.entries.len() >= PUBLISH_ENTRIES || outbox.held_open >= HELD_OPEN_DIRECTORIES {
            if !outbox.flush(context) {
                return Ok(false);
            }
        }
    }

    if let Some(sample) = sample {
        context.queue.note_sample(sample.was_blocked());
    }
    outbox.releases.push((token, complete));
    // The root's entries are what the interface shows first, so they are never held back.
    if (token == DirectoryToken(0) || outbox.should_flush(context)) && !outbox.flush(context) {
        return Ok(false);
    }
    Ok(true)
}

/// One measurement of a directory scan's wall time against the CPU time it used. A directory that
/// mostly waited for storage shows far more of the first than the second.
struct DirectorySample {
    wall: std::time::Instant,
    cpu_nanos: u64,
}

impl DirectorySample {
    /// Every this many directories of a worker is measured, while the question is still open.
    const INTERVAL: u32 = 16;

    fn start(context: &ScanContext<'_>, outbox: &mut Outbox) -> Option<Self> {
        outbox.directories_scanned = outbox.directories_scanned.wrapping_add(1);
        if outbox.directories_scanned % Self::INTERVAL != 0 || !context.queue.wants_samples() {
            return None;
        }
        Some(Self { wall: std::time::Instant::now(), cpu_nanos: thread_cpu_nanos() })
    }

    fn was_blocked(&self) -> bool {
        const FIXED_ALLOWANCE_NANOS: u64 = 40_000;
        let wall = u64::try_from(self.wall.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let cpu = thread_cpu_nanos().saturating_sub(self.cpu_nanos);
        // Being preempted adds wall time too, so only a large excess counts as waiting.
        wall > cpu.saturating_mul(2).saturating_add(FIXED_ALLOWANCE_NANOS)
    }
}

fn thread_cpu_nanos() -> u64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    u64::try_from(time.tv_sec).unwrap_or(0).saturating_mul(1_000_000_000).saturating_add(u64::try_from(time.tv_nsec).unwrap_or(0))
}

fn send_cancelled_once(context: &ScanContext<'_>) {
    if !context.cancelled_event_sent.swap(true, Ordering::Relaxed) {
        let _ = context.sender.send(ScanEvent::Cancelled);
    }
}

fn allocate_directory_token(next_token: &AtomicUsize) -> io::Result<DirectoryToken> {
    let token = next_token.fetch_add(1, Ordering::Relaxed);
    u32::try_from(token)
        .ok()
        .filter(|token| *token != NO_PARENT)
        .map(DirectoryToken)
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "too many directories in index"))
}
