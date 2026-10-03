use super::{
    EntryType, ExclusionReason, FileIdentity, IndexedDirectoryStream, MountIdentity, NodeState,
    RootAnchor, ScanQueueMetrics, ScanEvent, entry_type_for_mode, file_identity, file_size,
    identity_and_link_count_for_fd, identity_for_fd, mount_identity, open_child_directory,
    same_mount_on_fd, with_stat_at,
};
use crate::{DirectoryToken, EntryBatch};
use fdu_core::ScanEntry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const INDEXED_BATCH_SIZE: usize = 1024;
const INDEXED_SCAN_WORKER_LIMIT: usize = 16;
const INDEXED_SCAN_WORKER_HEADROOM: usize = 6;
const INDEXED_TASKS_PER_WORKER: usize = 8;

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
    ReopenFromParent(Arc<File>),
}

struct IndexedDirectoryTask {
    source: IndexedDirectorySource,
    name: Vec<u8>,
    identity: FileIdentity,
    link_count: u64,
    token: DirectoryToken,
}

struct ScanDirectoryResult {
    deferred: VecDeque<IndexedDirectoryTask>,
    keep_running: bool,
}

struct DirectoryProgress {
    parent: Option<DirectoryToken>,
    children_remaining: usize,
    enumerated: bool,
    complete: bool,
    finished: bool,
}

struct ScanProgress<'a> {
    directories: Mutex<HashMap<DirectoryToken, DirectoryProgress>>,
    finish_order: Mutex<()>,
    sender: &'a ScanEventSender<'a>,
}

impl ScanProgress<'_> {
    fn register_child(&self, parent: DirectoryToken, child: DirectoryToken) {
        let mut directories = self
            .directories
            .lock()
            .expect("directory progress is not poisoned");
        let parent_progress = directories
            .get_mut(&parent)
            .expect("directory parent was registered before its children");
        parent_progress.children_remaining += 1;
        let previous = directories.insert(
            child,
            DirectoryProgress {
                parent: Some(parent),
                children_remaining: 0,
                enumerated: false,
                complete: true,
                finished: false,
            },
        );
        assert!(previous.is_none(), "directory token was allocated twice");
    }

    fn finish_directory(&self, start: DirectoryToken, complete: bool) -> bool {
        let _send_guard = self
            .finish_order
            .lock()
            .expect("directory finish order is not poisoned");
        let events = {
            let mut directories = self
                .directories
                .lock()
                .expect("directory progress is not poisoned");
            let mut events = Vec::new();
            let mut current = Some(start);
            let mut current_complete = complete;
            let mut finishing_own_enumeration = true;
            while let Some(token) = current {
                let Some(progress) = directories.get_mut(&token) else {
                    return false;
                };
                if finishing_own_enumeration {
                    progress.enumerated = true;
                }
                progress.complete &= current_complete;
                if progress.finished || !progress.enumerated || progress.children_remaining != 0 {
                    break;
                }
                progress.finished = true;
                let directory_complete = progress.complete;
                let parent = progress.parent;
                events.push(ScanEvent::DirectoryFinished {
                    directory: token,
                    complete: directory_complete,
                });
                current = parent;
                current_complete = directory_complete;
                finishing_own_enumeration = false;
                if let Some(parent) = parent {
                    let Some(parent_progress) = directories.get_mut(&parent) else {
                        return false;
                    };
                    parent_progress.children_remaining = parent_progress
                        .children_remaining
                        .checked_sub(1)
                        .expect("finished directory has a pending parent count");
                    if !directory_complete {
                        parent_progress.complete = false;
                    }
                }
            }
            events
        };
        events
            .into_iter()
            .all(|event| self.sender.send(event).is_ok())
    }
}

struct WorkQueueState {
    tasks: VecDeque<IndexedDirectoryTask>,
    active_workers: usize,
    root_finished: bool,
    closed: bool,
}

struct IndexedWorkQueue {
    state: Mutex<WorkQueueState>,
    ready: Condvar,
    capacity: usize,
}

impl IndexedWorkQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(WorkQueueState {
                tasks: VecDeque::new(),
                active_workers: 0,
                root_finished: false,
                closed: false,
            }),
            ready: Condvar::new(),
            capacity,
        }
    }

    fn try_push(&self, task: IndexedDirectoryTask) -> Result<(), IndexedDirectoryTask> {
        let mut state = self.state.lock().expect("indexed work queue is not poisoned");
        if state.closed || state.tasks.len() >= self.capacity {
            return Err(task);
        }
        state.tasks.push_back(task);
        self.ready.notify_one();
        Ok(())
    }

    fn next_task(&self, cancelled: &AtomicBool) -> Option<IndexedDirectoryTask> {
        let mut state = self.state.lock().expect("indexed work queue is not poisoned");
        loop {
            if cancelled.load(Ordering::Relaxed) {
                state.tasks.clear();
                state.closed = true;
                self.ready.notify_all();
                return None;
            }
            if let Some(task) = state.tasks.pop_front() {
                state.active_workers += 1;
                self.ready.notify_all();
                return Some(task);
            }
            if state.closed || (state.root_finished && state.active_workers == 0) {
                state.closed = true;
                self.ready.notify_all();
                return None;
            }
            state = self.ready.wait(state).expect("indexed work queue is not poisoned");
        }
    }

    fn finish_task(&self) {
        let mut state = self.state.lock().expect("indexed work queue is not poisoned");
        state.active_workers = state.active_workers.saturating_sub(1);
        if state.root_finished && state.active_workers == 0 && state.tasks.is_empty() {
            state.closed = true;
        }
        self.ready.notify_all();
    }

    fn finish_root(&self) {
        let mut state = self.state.lock().expect("indexed work queue is not poisoned");
        state.root_finished = true;
        if state.active_workers == 0 && state.tasks.is_empty() {
            state.closed = true;
        }
        self.ready.notify_all();
    }

    fn cancel(&self) {
        let mut state = self.state.lock().expect("indexed work queue is not poisoned");
        state.tasks.clear();
        state.closed = true;
        self.ready.notify_all();
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

struct ScanContext<'a> {
    sender: &'a ScanEventSender<'a>,
    returned_batches: &'a Mutex<Receiver<EntryBatch>>,
    cancelled: &'a AtomicBool,
    cancelled_event_sent: &'a AtomicBool,
    seen_directories: &'a Mutex<HashSet<FileIdentity>>,
    next_token: &'a AtomicUsize,
    root_mount: &'a MountIdentity,
    progress: &'a ScanProgress<'a>,
    queue: &'a IndexedWorkQueue,
}

pub(super) fn scan_indexed(
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
    let root_identity = match identity_for_fd(anchor.as_raw_fd()) {
        Ok(identity) if identity == anchor.identity => identity,
        Ok(_) => {
            let _ = event_sender.send(ScanEvent::Failed {
                message: "scan root identity changed after it was opened".to_owned(),
            });
            let _ = event_sender.send(ScanEvent::Finished);
            return;
        }
        Err(error) => {
            let _ = event_sender.send(ScanEvent::Failed {
                message: error.to_string(),
            });
            let _ = event_sender.send(ScanEvent::Finished);
            return;
        }
    };
    let root_mount = match mount_identity(anchor.as_raw_fd()) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = event_sender.send(ScanEvent::Failed {
                message: error.to_string(),
            });
            let _ = event_sender.send(ScanEvent::Finished);
            return;
        }
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
    let mut seen_directories = HashSet::new();
    seen_directories.insert(root_identity);
    let seen_directories = Mutex::new(seen_directories);
    let next_token = AtomicUsize::new(1);
    let cancelled_event_sent = AtomicBool::new(false);
    let progress = ScanProgress {
        directories: Mutex::new(HashMap::from([(
            DirectoryToken(0),
            DirectoryProgress {
                parent: None,
                children_remaining: 0,
                enumerated: false,
                complete: true,
                finished: false,
            },
        )])),
        finish_order: Mutex::new(()),
        sender: &event_sender,
    };
    // Directory reads can block in the filesystem, so bounded worker headroom keeps other
    // independent subtrees moving while a worker waits for a bulk read.
    let worker_count = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .saturating_add(INDEXED_SCAN_WORKER_HEADROOM)
        .min(INDEXED_SCAN_WORKER_LIMIT);
    let queue = IndexedWorkQueue::new(worker_count.saturating_mul(INDEXED_TASKS_PER_WORKER).max(1));
    let context = ScanContext {
        sender: &event_sender,
        returned_batches: &returned_batches,
        cancelled: &cancelled,
        cancelled_event_sent: &cancelled_event_sent,
        seen_directories: &seen_directories,
        next_token: &next_token,
        root_mount: &root_mount,
        progress: &progress,
        queue: &queue,
    };

    thread::scope(|scope| {
        let workers = (0..worker_count)
            .map(|_| scope.spawn(|| worker_loop(&context)))
            .collect::<Vec<_>>();
        match scan_indexed_directory(anchor, DirectoryToken(0), &context) {
            Ok(result) => {
                if result.keep_running
                    && !process_deferred_tasks(result.deferred, &context)
                {
                    queue.cancel();
                }
                if !result.keep_running {
                    queue.cancel();
                }
            }
            Err(error) => {
                let _ = event_sender.send(ScanEvent::Failed {
                    message: error.to_string(),
                });
                let _ = progress.finish_directory(DirectoryToken(0), false);
                queue.cancel();
            }
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
    let _ = event_sender.send(ScanEvent::Finished);
}

fn worker_loop(context: &ScanContext<'_>) {
    while let Some(task) = context.queue.next_task(context.cancelled) {
        let result = scan_indexed_task(task, context);
        if result.keep_running {
            if !process_deferred_tasks(result.deferred, context) {
                context.queue.cancel();
            }
        } else {
            context.queue.cancel();
        }
        context.queue.finish_task();
    }
}

fn process_deferred_tasks(
    mut tasks: VecDeque<IndexedDirectoryTask>,
    context: &ScanContext<'_>,
) -> bool {
    while let Some(task) = tasks.pop_front() {
        if context.cancelled.load(Ordering::Relaxed) {
            send_cancelled_once(context);
            context.queue.cancel();
            return false;
        }
        match context.queue.try_push(task) {
            Ok(()) => continue,
            Err(task) => {
                let result = scan_indexed_task(task, context);
                if !result.keep_running {
                    context.queue.cancel();
                    return false;
                }
                tasks.extend(result.deferred);
            }
        }
    }
    true
}

fn scan_indexed_task(
    task: IndexedDirectoryTask,
    context: &ScanContext<'_>,
) -> ScanDirectoryResult {
    let anchor = match task.source {
        IndexedDirectorySource::Opened(anchor) => anchor,
        IndexedDirectorySource::ReopenFromParent(parent) => {
            let name = match CString::new(task.name.as_slice()) {
                Ok(name) => name,
                Err(error) => return failed_task(task.token, error.to_string(), context),
            };
            let child = match open_child_directory(parent.as_raw_fd(), &name) {
                Ok(child) => child,
                Err(error) => return failed_task(task.token, error.to_string(), context),
            };
            let (identity, link_count) = match identity_and_link_count_for_fd(child.as_raw_fd()) {
                Ok(metadata) => metadata,
                Err(error) => return failed_task(task.token, error.to_string(), context),
            };
            if identity != task.identity {
                return failed_task(
                    task.token,
                    "directory identity changed before scanning".to_owned(),
                    context,
                );
            }
            match same_mount_on_fd(child.as_raw_fd(), context.root_mount, identity.device) {
                Ok(true) => {}
                Ok(false) => {
                    let sent = context
                        .sender
                        .send(ScanEvent::DirectoryExcluded {
                            directory: task.token,
                            reason: ExclusionReason::MountBoundary,
                        })
                        .is_ok();
                    let finished = context.progress.finish_directory(task.token, false);
                    return ScanDirectoryResult {
                        deferred: VecDeque::new(),
                        keep_running: sent && finished,
                    };
                }
                Err(error) => return failed_task(task.token, error.to_string(), context),
            }
            RootAnchor {
                file: Arc::new(child),
                name: task.name.clone(),
                identity,
                link_count: if link_count == 0 { task.link_count } else { link_count },
            }
        }
    };
    match scan_indexed_directory(&anchor, task.token, context) {
        Ok(result) => result,
        Err(error) => failed_task(task.token, error.to_string(), context),
    }
}

fn failed_task(
    token: DirectoryToken,
    message: String,
    context: &ScanContext<'_>,
) -> ScanDirectoryResult {
    let sent = context
        .sender
        .send(ScanEvent::DirectoryFailed {
            directory: token,
            message,
        })
        .is_ok();
    let finished = context.progress.finish_directory(token, false);
    ScanDirectoryResult {
        deferred: VecDeque::new(),
        keep_running: sent && finished,
    }
}

fn scan_indexed_directory(
    anchor: &RootAnchor,
    token: DirectoryToken,
    context: &ScanContext<'_>,
) -> io::Result<ScanDirectoryResult> {
    super::with_indexed_directory_buffer(|buffer| {
        scan_indexed_directory_with_buffer(anchor, token, context, buffer)
    })
}

fn scan_indexed_directory_with_buffer(
    anchor: &RootAnchor,
    token: DirectoryToken,
    context: &ScanContext<'_>,
    buffer: &mut super::attributes::AlignedBuffer<{ super::BULK_RECORD_BUFFER_BYTES }>,
) -> io::Result<ScanDirectoryResult> {
    if context.cancelled.load(Ordering::Relaxed) {
        send_cancelled_once(context);
        context.queue.cancel();
        return Ok(ScanDirectoryResult {
            deferred: VecDeque::new(),
            keep_running: false,
        });
    }
    let directory = File::from(anchor.try_clone_fd()?);
    let mut stream = IndexedDirectoryStream::from_file(directory, buffer);
    let mut batch = take_indexed_batch(context.returned_batches, token);
    let mut complete = true;
    let mut pending = VecDeque::new();
    let mut deferred = VecDeque::new();

    loop {
        if context.cancelled.load(Ordering::Relaxed) {
            if !publish_indexed_batch(context.sender, context.returned_batches, &mut batch) {
                context.queue.cancel();
                return Ok(ScanDirectoryResult {
                    deferred,
                    keep_running: false,
                });
            }
            dispatch_tasks(std::mem::take(&mut pending), anchor, context, &mut deferred);
            send_cancelled_once(context);
            context.queue.cancel();
            return Ok(ScanDirectoryResult {
                deferred,
                keep_running: false,
            });
        }
        let directory_fd = stream.fd();
        let (name, bulk_metadata) = match stream.next_entry_name() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                complete = false;
                if !publish_indexed_batch(context.sender, context.returned_batches, &mut batch)
                    || context
                        .sender
                        .send(ScanEvent::DirectoryFailed {
                            directory: token,
                            message: error.to_string(),
                        })
                        .is_err()
                {
                    context.queue.cancel();
                    return Ok(ScanDirectoryResult {
                        deferred,
                        keep_running: false,
                    });
                }
                break;
            }
        };
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let mut entry_type = EntryType::Other;
        let mut identity = FileIdentity { device: 0, inode: 0 };
        let mut link_count = 0;
        let mut apparent_bytes = 0;
        let mut allocated_bytes = 0;
        let mut state = NodeState::Complete;
        let mut child_token = None;
        let metadata = match bulk_metadata {
            Some(metadata) => Ok((
                metadata.identity,
                metadata.link_count,
                metadata.entry_type,
                Ok(metadata.apparent_bytes),
                Ok(metadata.allocated_bytes),
            )),
            None => with_stat_at(directory_fd, name, |stat| {
                let identity = file_identity(stat)?;
                let link_count = u64::try_from(stat.st_nlink).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "negative link count")
                })?;
                Ok((
                    identity,
                    link_count,
                    entry_type_for_mode(stat.st_mode),
                    file_size(stat, true),
                    file_size(stat, false),
                ))
            }),
        };
        match metadata {
            Ok((found_identity, found_link_count, found_type, apparent, allocated)) => {
                identity = found_identity;
                link_count = found_link_count;
                entry_type = found_type;
                match (apparent, allocated) {
                    (Ok(apparent), Ok(allocated)) if entry_type != EntryType::Directory => {
                        apparent_bytes = apparent;
                        allocated_bytes = allocated;
                    }
                    (Err(error), _) | (_, Err(error)) if entry_type != EntryType::Directory => {
                        state = NodeState::Incomplete;
                        complete = false;
                        if context
                            .sender
                            .send(ScanEvent::DirectoryFailed {
                                directory: token,
                                message: error.to_string(),
                            })
                            .is_err()
                        {
                            context.queue.cancel();
                            return Ok(ScanDirectoryResult {
                                deferred,
                                keep_running: false,
                            });
                        }
                    }
                    _ => {}
                }
                if entry_type == EntryType::Directory {
                    match open_child_directory(directory_fd, name) {
                        Ok(child) => {
                            let opened_metadata = match identity_and_link_count_for_fd(child.as_raw_fd()) {
                                Ok(metadata) => Some(metadata),
                                Err(error) => {
                                    state = NodeState::Incomplete;
                                    complete = false;
                                    if context
                                        .sender
                                        .send(ScanEvent::DirectoryFailed {
                                            directory: token,
                                            message: error.to_string(),
                                        })
                                        .is_err()
                                    {
                                        context.queue.cancel();
                                        return Ok(ScanDirectoryResult {
                                            deferred,
                                            keep_running: false,
                                        });
                                    }
                                    None
                                }
                            };
                            if let Some((opened_identity, opened_link_count)) = opened_metadata {
                                link_count = opened_link_count;
                                if opened_identity != identity {
                                    state = NodeState::Incomplete;
                                    complete = false;
                                    if context
                                        .sender
                                        .send(ScanEvent::DirectoryFailed {
                                            directory: token,
                                            message: "directory identity changed during scan".to_owned(),
                                        })
                                        .is_err()
                                    {
                                        context.queue.cancel();
                                        return Ok(ScanDirectoryResult {
                                            deferred,
                                            keep_running: false,
                                        });
                                    }
                                } else {
                                    match same_mount_on_fd(
                                        child.as_raw_fd(),
                                        context.root_mount,
                                        opened_identity.device,
                                    )? {
                                        false => {
                                            state = NodeState::Excluded(ExclusionReason::MountBoundary);
                                            complete = false;
                                        }
                                        true => {
                                            let is_new = context
                                                .seen_directories
                                                .lock()
                                                .expect("directory identity set is not poisoned")
                                                .insert(identity);
                                            if !is_new {
                                                state = NodeState::Excluded(
                                                    ExclusionReason::UnsupportedAlias,
                                                );
                                                complete = false;
                                            } else {
                                                let child_token_value =
                                                    allocate_directory_token(context.next_token)?;
                                                context
                                                    .progress
                                                    .register_child(token, child_token_value);
                                                child_token = Some(child_token_value);
                                                pending.push_back(IndexedDirectoryTask {
                                                    source: IndexedDirectorySource::Opened(RootAnchor {
                                                        file: Arc::new(child),
                                                        name: Vec::new(),
                                                        identity,
                                                        link_count: opened_link_count,
                                                    }),
                                                    name: name_bytes.to_vec(),
                                                    identity,
                                                    link_count: opened_link_count,
                                                    token: child_token_value,
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            state = NodeState::Incomplete;
                            complete = false;
                            if context
                                .sender
                                .send(ScanEvent::DirectoryFailed {
                                    directory: token,
                                    message: error.to_string(),
                                })
                                .is_err()
                            {
                                context.queue.cancel();
                                return Ok(ScanDirectoryResult {
                                    deferred,
                                    keep_running: false,
                                });
                            }
                        }
                    }
                }
            }
            Err(error) => {
                state = NodeState::Incomplete;
                complete = false;
                if context
                    .sender
                    .send(ScanEvent::DirectoryFailed {
                        directory: token,
                        message: error.to_string(),
                    })
                    .is_err()
                {
                    context.queue.cancel();
                    return Ok(ScanDirectoryResult {
                        deferred,
                        keep_running: false,
                    });
                }
            }
        }

        let name_start = match u32::try_from(batch.names.len()) {
            Ok(start) => start,
            Err(_) => {
                complete = false;
                if context
                    .sender
                    .send(ScanEvent::DirectoryFailed {
                        directory: token,
                        message: "directory batch exceeded supported name storage".to_owned(),
                    })
                    .is_err()
                {
                    context.queue.cancel();
                    return Ok(ScanDirectoryResult {
                        deferred,
                        keep_running: false,
                    });
                }
                break;
            }
        };
        let name_end = match name_start.checked_add(u32::try_from(name_bytes.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "filename is too long")
        })?) {
            Some(end) => end,
            None => {
                complete = false;
                break;
            }
        };
        batch.names.extend_from_slice(name_bytes);
        batch.entries.push(ScanEntry {
            name: name_start..name_end,
            directory_token: child_token,
            entry_type,
            identity,
            link_count,
            apparent_bytes,
            allocated_bytes,
            state,
        });
        if batch.entries.len() >= INDEXED_BATCH_SIZE
            || pending.len() >= context.queue.capacity()
        {
            if !publish_indexed_batch(context.sender, context.returned_batches, &mut batch) {
                context.queue.cancel();
                return Ok(ScanDirectoryResult {
                    deferred,
                    keep_running: false,
                });
            }
            dispatch_tasks(std::mem::take(&mut pending), anchor, context, &mut deferred);
        }
    }

    if !publish_indexed_batch(context.sender, context.returned_batches, &mut batch) {
        context.queue.cancel();
        return Ok(ScanDirectoryResult {
            deferred,
            keep_running: false,
        });
    }
    dispatch_tasks(std::mem::take(&mut pending), anchor, context, &mut deferred);
    let keep_running = context.progress.finish_directory(token, complete);
    if !keep_running {
        context.queue.cancel();
    }
    Ok(ScanDirectoryResult {
        deferred,
        keep_running,
    })
}

fn dispatch_tasks(
    mut tasks: VecDeque<IndexedDirectoryTask>,
    parent: &RootAnchor,
    context: &ScanContext<'_>,
    deferred: &mut VecDeque<IndexedDirectoryTask>,
) {
    while let Some(task) = tasks.pop_front() {
        match context.queue.try_push(task) {
            Ok(()) => {}
            Err(mut task) => {
                task.source = IndexedDirectorySource::ReopenFromParent(Arc::clone(&parent.file));
                deferred.push_back(task);
            }
        }
    }
}

fn send_cancelled_once(context: &ScanContext<'_>) {
    if !context.cancelled_event_sent.swap(true, Ordering::Relaxed) {
        let _ = context.sender.send(ScanEvent::Cancelled);
    }
}

fn allocate_directory_token(next_token: &AtomicUsize) -> io::Result<DirectoryToken> {
    let token = next_token.fetch_add(1, Ordering::Relaxed);
    u32::try_from(token)
        .map(DirectoryToken)
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "too many directories in index"))
}

fn take_indexed_batch(
    returned_batches: &Mutex<Receiver<EntryBatch>>,
    directory: DirectoryToken,
) -> EntryBatch {
    let mut batch = returned_batches
        .lock()
        .expect("returned batch pool is not poisoned")
        .try_recv()
        .unwrap_or_else(|_| {
            EntryBatch::with_capacity(directory, INDEXED_BATCH_SIZE, INDEXED_BATCH_SIZE * 24)
        });
    batch.clear_for_directory(directory);
    batch
}

fn publish_indexed_batch(
    sender: &ScanEventSender<'_>,
    returned_batches: &Mutex<Receiver<EntryBatch>>,
    batch: &mut EntryBatch,
) -> bool {
    if batch.entries.is_empty() {
        return true;
    }
    let next = take_indexed_batch(returned_batches, batch.directory);
    let current = std::mem::replace(batch, next);
    sender.send(ScanEvent::Entries(current)).is_ok()
}
