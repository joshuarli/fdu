use fdu_core::{EntryType, ExclusionReason, FileIdentity, NodeId, NodeRecord, NodeState, Tree};
use hashbrown::HashSet;
use std::fmt;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

const OUTCOME_BATCH_SIZE: usize = 2048;
/// Progress is advisory, so it is sent at most this often instead of once per
/// operation. A send per operation made the worker wait on the interface.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

pub struct DeletionPlan {
    tree: Arc<Tree>,
    pub roots: Vec<NodeId>,
    targets: Vec<NodeId>,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
}

impl DeletionPlan {
    pub fn operation_count(&self) -> usize {
        self.targets.len()
    }

    pub fn tree(&self) -> &Arc<Tree> {
        &self.tree
    }
}

/// One selected root that cannot be deleted. `node` is `None` for problems with
/// the selection as a whole, such as an empty selection or a byte total that
/// overflows.
#[derive(Debug)]
pub struct Rejection {
    pub node: Option<NodeId>,
    pub reason: String,
}

#[derive(Debug)]
pub struct PlanError {
    pub rejected: Vec<Rejection>,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} selected entr{} cannot be deleted", self.rejected.len(), if self.rejected.len() == 1 { "y" } else { "ies" })
    }
}

impl std::error::Error for PlanError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutcomeKind {
    Deleted,
    AlreadyAbsent,
    Changed,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub struct DeleteOutcome {
    pub node: NodeId,
    pub kind: OutcomeKind,
    pub message: Option<String>,
}

#[derive(Debug)]
pub enum DeleteEvent {
    Progress { completed: usize, total: usize, current: NodeId },
    Outcomes(Vec<DeleteOutcome>),
    Failed { message: String },
    Finished,
}

/// Plans the permanent removal of `selected` roots, which may live in different
/// directories. The whole selection is accepted or rejected; an ineligible root
/// is never dropped to let the others through. Roots must not overlap, because a
/// directory root already covers every descendant.
pub fn create_plan(tree: Arc<Tree>, selected: &[NodeId]) -> Result<DeletionPlan, PlanError> {
    let mut rejected = Vec::new();
    if selected.is_empty() {
        rejected.push(Rejection { node: None, reason: "no entries are selected".to_owned() });
    }
    let mut unique = Vec::with_capacity(selected.len());
    let mut seen = HashSet::with_capacity(selected.len());
    for id in selected {
        if seen.insert(*id) {
            unique.push(*id);
        }
    }
    for id in &unique {
        let reject = |reason: &str| Rejection { node: Some(*id), reason: reason.to_owned() };
        match tree.record(*id) {
            None => rejected.push(reject("absent from the index")),
            Some(record) if *id == tree.root() || record.entry_type == EntryType::Root => {
                rejected.push(reject("the scan root is protected"));
            }
            Some(_) if has_selected_ancestor(&tree, &seen, *id) => {
                rejected.push(reject("already covered by a selected parent directory"));
            }
            Some(record) => {
                if let Some(reason) = ineligible_reason(&tree, *id, record.entry_type) {
                    rejected.push(reject(reason));
                }
            }
        }
    }
    if !rejected.is_empty() {
        return Err(PlanError { rejected });
    }

    let overflow = |what: &str| PlanError {
        rejected: vec![Rejection { node: None, reason: format!("selected {what} byte total exceeds u64") }],
    };
    let mut targets = Vec::new();
    let mut apparent_bytes = 0u64;
    let mut allocated_bytes = 0u64;
    for root in &unique {
        let record = tree.record(*root).expect("validated deletion root");
        apparent_bytes = apparent_bytes.checked_add(record.apparent_bytes).ok_or_else(|| overflow("apparent"))?;
        allocated_bytes = allocated_bytes.checked_add(record.allocated_bytes).ok_or_else(|| overflow("allocated"))?;
        append_postorder(&tree, *root, &mut targets);
    }
    Ok(DeletionPlan {
        tree,
        roots: unique,
        targets,
        apparent_bytes,
        allocated_bytes,
    })
}

fn has_selected_ancestor(tree: &Tree, selected: &HashSet<NodeId>, id: NodeId) -> bool {
    let mut current = tree.record(id).and_then(|record| record.parent());
    while let Some(ancestor) = current {
        if selected.contains(&ancestor) {
            return true;
        }
        current = tree.record(ancestor).and_then(|record| record.parent());
    }
    false
}

fn ineligible_reason(tree: &Tree, id: NodeId, entry_type: EntryType) -> Option<&'static str> {
    let record = tree.record(id)?;
    match record.state {
        NodeState::Excluded(ExclusionReason::MountBoundary) => return Some("mount boundary is protected"),
        NodeState::Excluded(ExclusionReason::UnsupportedAlias) => return Some("unsupported directory alias is protected"),
        NodeState::Complete => {}
        NodeState::Scanning => return Some("scan is still in progress"),
        NodeState::Incomplete => return Some("scan is incomplete"),
        NodeState::Stale => return Some("entry totals are stale; rescan first"),
        NodeState::Tombstone => return Some("entry has already been removed from the index"),
    }
    if entry_type != EntryType::Directory {
        return None;
    }
    let mut pending = vec![WalkFrame::Children {
        next: record.first_child(),
        directories: false,
    }];
    while let Some(WalkFrame::Children { next, directories }) = pending.pop() {
        let Some(child) = next else {
            continue;
        };
        let Some(child_record) = tree.record(child) else {
            return Some("directory index is invalid");
        };
        pending.push(WalkFrame::Children {
            next: child_record.next_sibling(),
            directories,
        });
        if child_record.entry_type == EntryType::Directory {
            pending.push(WalkFrame::Children {
                next: child_record.first_child(),
                directories: false,
            });
        }
        match child_record.state {
            NodeState::Complete => {}
            NodeState::Excluded(ExclusionReason::MountBoundary) => return Some("subtree contains an excluded mount boundary"),
            NodeState::Excluded(ExclusionReason::UnsupportedAlias) => return Some("subtree contains an unsupported directory alias"),
            NodeState::Scanning => return Some("subtree scan is incomplete"),
            NodeState::Incomplete => return Some("subtree contains scan errors"),
            NodeState::Stale => return Some("subtree totals are stale; rescan first"),
            NodeState::Tombstone => return Some("subtree changed during a deletion operation"),
        }
    }
    None
}

enum WalkFrame {
    Enter(NodeId),
    Children { next: Option<NodeId>, directories: bool },
    Exit(NodeId),
}

fn append_postorder(tree: &Tree, root: NodeId, output: &mut Vec<NodeId>) {
    let mut stack = vec![WalkFrame::Enter(root)];
    while let Some(frame) = stack.pop() {
        match frame {
            WalkFrame::Enter(id) => {
                let record = tree.record(id).expect("validated deletion subtree");
                if record.entry_type == EntryType::Directory {
                    stack.push(WalkFrame::Exit(id));
                    stack.push(WalkFrame::Children {
                        next: record.first_child(),
                        directories: true,
                    });
                    stack.push(WalkFrame::Children {
                        next: record.first_child(),
                        directories: false,
                    });
                } else {
                    output.push(id);
                }
            }
            WalkFrame::Children { next: Some(child), directories } => {
                let Some(record) = tree.record(child) else {
                    continue;
                };
                stack.push(WalkFrame::Children {
                    next: record.next_sibling(),
                    directories,
                });
                if (record.entry_type == EntryType::Directory) == directories {
                    stack.push(WalkFrame::Enter(child));
                }
            }
            WalkFrame::Children { next: None, .. } => {}
            WalkFrame::Exit(id) => output.push(id),
        }
    }
}

pub fn execute(
    root: OwnedFd,
    plan: DeletionPlan,
    cancelled: Arc<AtomicBool>,
    sender: SyncSender<DeleteEvent>,
) {
    posix::run(root, plan, cancelled, sender);
}

mod posix {
    use super::*;
    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, Stat};
    use rustix::io::Errno;
    use std::collections::BTreeMap;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};
    use std::sync::Mutex;
    use std::time::Instant;

    const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    /// Removing a directory mostly waits on the filesystem (a journal commit, not the CPU), and
    /// that waiting overlaps across threads, so the pool is much larger than the number of cores.
    /// Measured on the layout fixture, whole-tree deletion took 4.3 s with 2 threads, 2.9 s with
    /// 4, 2.7 s with 16, 2.3 s with 32, and no less with 64 or 128.
    const DELETE_THREADS: usize = 32;

    /// Identifies the mount a directory lives on, so a directory that became a mount point since
    /// scanning is never entered.
    #[cfg(target_os = "macos")]
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct MountIdentity {
        device: u64,
        fsid: [i32; 2],
        mount_point: Vec<u8>,
    }

    /// The kernel mount ID tells bind mounts of one filesystem apart; the device separates
    /// filesystem boundaries that share a mount, such as btrfs subvolumes.
    #[cfg(target_os = "linux")]
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct MountIdentity {
        device: u64,
        mount_id: u64,
    }

    /// An opened directory under the plan, or the directory a selected entry lives in. A child
    /// holds its parent's context, so a directory's descriptor lives until everything below it has
    /// finished, and the directory can be removed from the parent's descriptor afterwards.
    struct DirContext {
        fd: OwnedFd,
        node: NodeId,
        /// The context of the directory above, if this directory is itself being removed. The
        /// context of a selected entry's parent has none: the parent stays.
        parent: Option<Arc<DirContext>>,
        /// Whether this directory is being removed, and so has holds counted for it.
        removing: bool,
    }

    /// State shared by every thread of one deletion.
    struct Deletion<'a> {
        tree: &'a Tree,
        root_mount: MountIdentity,
        cancelled: &'a AtomicBool,
        sender: &'a SyncSender<DeleteEvent>,
        after_operation: &'a (dyn Fn(usize) + Sync),
        /// Per node, whether its outcome is decided. Anything left undecided at the end was not
        /// attempted.
        recorded: Vec<AtomicBool>,
        /// Per directory being removed: its own hold plus one for each subdirectory not yet removed.
        holds: Vec<AtomicU32>,
        outcomes: Mutex<Vec<DeleteOutcome>>,
        completed: AtomicUsize,
        total: usize,
        started: Instant,
        last_progress_nanos: AtomicU64,
        receiver_gone: AtomicBool,
    }

    pub(super) fn run(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
    ) {
        run_with(root, plan, cancelled, sender, DELETE_THREADS, |_| {});
    }

    pub(super) fn run_with(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
        threads: usize,
        after_operation: impl Fn(usize) + Sync,
    ) {
        let tree: &Tree = &plan.tree;
        let root_record = tree.record(tree.root()).expect("tree has root");
        let expected_root = root_record.identity;
        let root_mount = match directory_state(root.as_fd()) {
            Ok((identity, mount)) if identity == expected_root => mount,
            Ok(_) => {
                send_failure(&sender, "scan root identity changed before deletion");
                return;
            }
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };
        let deletion = Deletion {
            tree,
            root_mount,
            cancelled: &cancelled,
            sender: &sender,
            after_operation: &after_operation,
            recorded: (0..tree.len()).map(|_| AtomicBool::new(false)).collect(),
            holds: (0..tree.len()).map(|_| AtomicU32::new(0)).collect(),
            outcomes: Mutex::new(Vec::with_capacity(OUTCOME_BATCH_SIZE)),
            completed: AtomicUsize::new(0),
            total: plan.targets.len(),
            started: Instant::now(),
            last_progress_nanos: AtomicU64::new(0),
            receiver_gone: AtomicBool::new(false),
        };

        // Selected entries are grouped by the directory they live in, so each such directory is
        // opened once.
        let mut groups: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for selected in &plan.roots {
            let parent = tree
                .record(*selected)
                .and_then(NodeRecord::parent)
                .expect("deletion targets have parents");
            groups.entry(parent.index()).or_default().push(*selected);
        }
        let root_context = match root.try_clone() {
            Ok(fd) => Arc::new(DirContext { fd, node: tree.root(), parent: None, removing: false }),
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };
        let pool = match rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|index| format!("fdu-delete-{index}"))
            .build()
        {
            Ok(pool) => pool,
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };
        let deletion = &deletion;
        pool.scope(|scope| {
            for (parent, selected) in groups {
                let root_context = Arc::clone(&root_context);
                scope.spawn(move |scope| {
                    let parent = NodeId::from_index(parent).expect("indexed parent");
                    delete_group(deletion, &root_context, parent, selected, scope);
                });
            }
        });
        drop(pool);

        if deletion.receiver_gone.load(Ordering::Relaxed) {
            return;
        }
        // Whatever was not attempted, in plan order, because the deletion was stopped.
        let mut outcomes = std::mem::take(&mut *deletion.outcomes.lock().expect("outcomes are not poisoned"));
        for node in plan.targets.iter().copied() {
            if !deletion.recorded[node.index()].swap(true, Ordering::Relaxed) {
                outcomes.push(DeleteOutcome { node, kind: OutcomeKind::Cancelled, message: None });
                if outcomes.len() >= OUTCOME_BATCH_SIZE && !flush(&sender, &mut outcomes) {
                    return;
                }
            }
        }
        if !flush(&sender, &mut outcomes) {
            return;
        }
        let _ = sender.send(DeleteEvent::Finished);
    }

    impl Deletion<'_> {
        fn stopped(&self) -> bool {
            self.cancelled.load(Ordering::Relaxed) || self.receiver_gone.load(Ordering::Relaxed)
        }

        /// Decides the outcome of `node`.
        fn record(&self, node: NodeId, kind: OutcomeKind, message: Option<String>) {
            self.recorded[node.index()].store(true, Ordering::Relaxed);
            let completed = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
            (self.after_operation)(completed);
            let batch = {
                let mut outcomes = self.outcomes.lock().expect("outcomes are not poisoned");
                outcomes.push(DeleteOutcome { node, kind, message });
                (outcomes.len() >= OUTCOME_BATCH_SIZE)
                    .then(|| std::mem::replace(&mut *outcomes, Vec::with_capacity(OUTCOME_BATCH_SIZE)))
            };
            if let Some(batch) = batch {
                if self.sender.send(DeleteEvent::Outcomes(batch)).is_err() {
                    self.receiver_gone.store(true, Ordering::Relaxed);
                }
            }
            self.report_progress(completed, node);
        }

        /// Progress is advisory, so it goes out at most every interval, from whichever thread
        /// notices the interval has passed, and never waits on the interface.
        fn report_progress(&self, completed: usize, current: NodeId) {
            let now = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let interval = u64::try_from(PROGRESS_INTERVAL.as_nanos()).unwrap_or(u64::MAX);
            let last = self.last_progress_nanos.load(Ordering::Relaxed);
            if now.saturating_sub(last) < interval
                || self
                    .last_progress_nanos
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_err()
            {
                return;
            }
            match self.sender.try_send(DeleteEvent::Progress { completed, total: self.total, current }) {
                // A full queue means the interface is behind; the next report supersedes this one.
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    self.receiver_gone.store(true, Ordering::Relaxed);
                }
            }
        }

        /// Gives every entry below `node` the same outcome, for a directory that could not be
        /// entered.
        fn record_below(&self, node: NodeId, kind: OutcomeKind, message: &str) {
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                for child in self.tree.children(current) {
                    if self.tree.record(child).is_some_and(|record| record.entry_type == EntryType::Directory) {
                        stack.push(child);
                    }
                    self.record(child, kind, Some(message.to_owned()));
                }
            }
        }
    }

    /// Removes the selected entries that live in the directory `parent`, opened below the scan
    /// root through directories checked against the index.
    fn delete_group<'scope>(
        deletion: &'scope Deletion<'_>,
        root_context: &Arc<DirContext>,
        parent: NodeId,
        selected: Vec<NodeId>,
        scope: &rayon::Scope<'scope>,
    ) {
        let context = match open_context(deletion, root_context, parent) {
            Ok(context) => context,
            Err(message) => {
                for node in selected {
                    deletion.record(node, OutcomeKind::Failed, Some(message.clone()));
                    deletion.record_below(node, OutcomeKind::Failed, &message);
                }
                return;
            }
        };
        for node in selected {
            if deletion.stopped() {
                return;
            }
            let is_directory = deletion
                .tree
                .record(node)
                .is_some_and(|record| record.entry_type == EntryType::Directory);
            if is_directory {
                deletion.holds[node.index()].store(1, Ordering::Relaxed);
                let context = Arc::clone(&context);
                scope.spawn(move |scope| delete_directory(deletion, context, node, scope));
            } else {
                delete_file(deletion, context.fd.as_fd(), node);
            }
        }
    }

    /// The context of `node`, a directory above selected entries: each directory from just below
    /// the root down is opened relative to its parent and checked against its indexed identity, so
    /// an ancestor replaced after scanning is never followed.
    fn open_context(
        deletion: &Deletion<'_>,
        root_context: &Arc<DirContext>,
        node: NodeId,
    ) -> Result<Arc<DirContext>, String> {
        let tree = deletion.tree;
        let mut path = Vec::new();
        let mut current = Some(node);
        while let Some(next) = current {
            if next == tree.root() {
                break;
            }
            path.push(next);
            current = tree.record(next).and_then(NodeRecord::parent);
            if current.is_none() {
                return Err("parent missing from index".to_owned());
            }
        }
        let mut context = Arc::clone(root_context);
        for next in path.into_iter().rev() {
            let (fd, _) = open_checked(deletion, context.fd.as_fd(), next).map_err(|(_, message)| message)?;
            context = Arc::new(DirContext { fd, node: next, parent: None, removing: false });
        }
        Ok(context)
    }

    /// Opens the directory `node` inside `parent` and checks it is the directory that was indexed.
    /// On failure returns what to report for the directory, and why.
    fn open_checked(
        deletion: &Deletion<'_>,
        parent: BorrowedFd<'_>,
        node: NodeId,
    ) -> Result<(OwnedFd, MountIdentity), (OutcomeKind, String)> {
        let record = deletion.tree.record(node).expect("path node exists");
        let name = deletion.tree.name(node).ok_or((OutcomeKind::Failed, "directory name missing".to_owned()))?;
        let fd = match retry_errno(|| fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Err((OutcomeKind::AlreadyAbsent, "directory is already gone".to_owned())),
            Err(error) => {
                // A directory replaced by something else is a change, not a failure to remove it.
                if let Ok(latest) = stat_at(parent, name) {
                    if identity(&latest).ok() != Some(record.identity) || entry_type(&latest) != record.entry_type {
                        return Err((OutcomeKind::Changed, "directory was replaced during validation".to_owned()));
                    }
                }
                return Err((OutcomeKind::Failed, io::Error::from(error).to_string()));
            }
        };
        let (found, mount) = directory_state(fd.as_fd()).map_err(|error| (OutcomeKind::Failed, error.to_string()))?;
        if found != record.identity {
            return Err((OutcomeKind::Changed, "directory was replaced".to_owned()));
        }
        if mount != deletion.root_mount {
            return Err((OutcomeKind::Changed, "directory became a mount boundary".to_owned()));
        }
        Ok((fd, mount))
    }

    /// Removes everything below the directory `node`, then the directory itself once the last of
    /// its subdirectories has been removed, by whichever thread finishes last.
    fn delete_directory<'scope>(
        deletion: &'scope Deletion<'_>,
        parent: Arc<DirContext>,
        node: NodeId,
        scope: &rayon::Scope<'scope>,
    ) {
        if deletion.stopped() {
            return;
        }
        let fd = match open_checked(deletion, parent.fd.as_fd(), node) {
            Ok((fd, _)) => fd,
            Err((kind, message)) => {
                deletion.record(node, kind, Some(message.clone()));
                let below = if kind == OutcomeKind::AlreadyAbsent { OutcomeKind::AlreadyAbsent } else { OutcomeKind::Failed };
                deletion.record_below(node, below, &message);
                // Nothing under it will be removed, so the directory above cannot be emptied either;
                // its own removal reports that.
                release(deletion, &parent);
                return;
            }
        };
        let context = Arc::new(DirContext { fd, node, parent: Some(parent), removing: true });

        // Entries go in inode order, which the filesystem frees faster than the order they were
        // listed in.
        let tree = deletion.tree;
        let mut files = Vec::new();
        let mut directories = Vec::new();
        for child in tree.children(node) {
            let record = tree.record(child).expect("indexed child");
            if record.entry_type == EntryType::Directory {
                directories.push(child);
            } else {
                files.push(child);
            }
        }
        files.sort_unstable_by_key(|child| tree.record(*child).map_or(0, |record| record.identity.inode));
        directories.sort_unstable_by_key(|child| tree.record(*child).map_or(0, |record| record.identity.inode));

        for child in directories {
            deletion.holds[child.index()].store(1, Ordering::Relaxed);
            deletion.holds[node.index()].fetch_add(1, Ordering::Relaxed);
            let context = Arc::clone(&context);
            scope.spawn(move |scope| delete_directory(deletion, context, child, scope));
        }
        for child in files {
            if deletion.stopped() {
                break;
            }
            delete_file(deletion, context.fd.as_fd(), child);
        }
        release(deletion, &context);
    }

    /// Gives up one hold on a directory being removed and, if it was the last, removes the
    /// directory and gives up the hold its parent has on it, and so on upward.
    fn release(deletion: &Deletion<'_>, context: &Arc<DirContext>) {
        let mut context = Arc::clone(context);
        loop {
            if !context.removing {
                return;
            }
            if deletion.holds[context.node.index()].fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            if !deletion.stopped() {
                remove_directory(deletion, &context);
            }
            match context.parent.clone() {
                Some(parent) => context = parent,
                None => return,
            }
        }
    }

    /// Removes the now empty directory of `context` from its parent.
    fn remove_directory(deletion: &Deletion<'_>, context: &DirContext) {
        let node = context.node;
        let parent = context.parent.as_ref().expect("a directory being removed has a parent");
        let record = deletion.tree.record(node).expect("deletion target exists");
        let name = deletion.tree.name(node).expect("indexed entry has name");
        let parent_fd = parent.fd.as_fd();
        // Entries were checked when the directory was opened; check once more what is there now.
        let latest = match stat_at(parent_fd, name) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return deletion.record(node, OutcomeKind::AlreadyAbsent, None),
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(io::Error::from(error).to_string())),
        };
        match identity(&latest) {
            Ok(found) if found == record.identity && entry_type(&latest) == record.entry_type => {}
            Ok(_) => {
                return deletion.record(
                    node,
                    OutcomeKind::Changed,
                    Some("entry changed immediately before removal".to_owned()),
                );
            }
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(error.to_string())),
        }
        unlink(deletion, parent_fd, name, node, AtFlags::REMOVEDIR);
    }

    /// Removes one entry that is not a directory, after checking it is the entry that was indexed.
    fn delete_file(deletion: &Deletion<'_>, parent_fd: BorrowedFd<'_>, node: NodeId) {
        let record = deletion.tree.record(node).expect("deletion target exists");
        let name = deletion.tree.name(node).expect("indexed entry has name");
        let current = match stat_at(parent_fd, name) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return deletion.record(node, OutcomeKind::AlreadyAbsent, None),
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(io::Error::from(error).to_string())),
        };
        match identity(&current) {
            Ok(found) if found == record.identity && entry_type(&current) == record.entry_type => {}
            Ok(_) => {
                return deletion.record(
                    node,
                    OutcomeKind::Changed,
                    Some("entry was replaced since scanning".to_owned()),
                );
            }
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(error.to_string())),
        }
        unlink(deletion, parent_fd, name, node, AtFlags::empty());
    }

    fn unlink(deletion: &Deletion<'_>, parent_fd: BorrowedFd<'_>, name: &[u8], node: NodeId, flags: AtFlags) {
        match fs::unlinkat(parent_fd, name, flags) {
            Ok(()) => deletion.record(node, OutcomeKind::Deleted, None),
            Err(Errno::NOENT) => deletion.record(node, OutcomeKind::AlreadyAbsent, None),
            Err(Errno::INTR) => deletion.record(
                node,
                OutcomeKind::Failed,
                Some("unlink was interrupted; removal outcome is uncertain, refresh required".to_owned()),
            ),
            Err(Errno::NOTEMPTY) => deletion.record(
                node,
                OutcomeKind::Failed,
                Some("directory gained unindexed children".to_owned()),
            ),
            Err(error) => deletion.record(node, OutcomeKind::Failed, Some(io::Error::from(error).to_string())),
        }
    }

    fn stat_at(parent: BorrowedFd<'_>, name: &[u8]) -> Result<Stat, Errno> {
        retry_errno(|| fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))
    }

    // `st_dev` is 32 bits on macOS but 64 bits on Linux: the checked conversion is
    // required on one platform and a no-op where the field is already `u64`.
    #[allow(clippy::useless_conversion)]
    fn identity(metadata: &Stat) -> io::Result<FileIdentity> {
        Ok(FileIdentity {
            device: u64::try_from(metadata.st_dev).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid device number"))?,
            inode: metadata.st_ino,
        })
    }

    fn entry_type(metadata: &Stat) -> EntryType {
        match FileType::from_raw_mode(metadata.st_mode) {
            FileType::Directory => EntryType::Directory,
            FileType::RegularFile => EntryType::RegularFile,
            FileType::Symlink => EntryType::Symlink,
            _ => EntryType::Other,
        }
    }

    /// The identity and mount of an open directory.
    #[cfg(target_os = "macos")]
    fn directory_state(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, MountIdentity)> {
        let metadata = retry(|| fs::fstat(fd))?;
        let filesystem = retry(|| fs::fstatfs(fd))?;
        // Apple's fsid_t is exactly two i32 words.
        const _: () = assert!(std::mem::size_of::<[i32; 2]>() == 8);
        let fsid = unsafe { std::mem::transmute_copy::<_, [i32; 2]>(&filesystem.f_fsid) };
        let mount_point = filesystem
            .f_mntonname
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        let identity = identity(&metadata)?;
        Ok((identity, MountIdentity { device: identity.device, fsid, mount_point }))
    }

    #[cfg(target_os = "linux")]
    fn directory_state(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, MountIdentity)> {
        let wanted = fs::StatxFlags::INO | fs::StatxFlags::MNT_ID;
        let metadata = retry(|| fs::statx(fd, c"", AtFlags::EMPTY_PATH, wanted))?;
        if !fs::StatxFlags::from_bits_retain(metadata.stx_mask).contains(wanted) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel did not report the directory identity and mount ID",
            ));
        }
        let device = fs::makedev(metadata.stx_dev_major, metadata.stx_dev_minor);
        Ok((
            FileIdentity { device, inode: metadata.stx_ino },
            MountIdentity { device, mount_id: metadata.stx_mnt_id },
        ))
    }

    fn flush(sender: &SyncSender<DeleteEvent>, outcomes: &mut Vec<DeleteOutcome>) -> bool {
        if outcomes.is_empty() {
            return true;
        }
        let values = std::mem::replace(outcomes, Vec::with_capacity(OUTCOME_BATCH_SIZE));
        sender.send(DeleteEvent::Outcomes(values)).is_ok()
    }

    fn retry_errno<T>(mut operation: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
        loop {
            match operation() {
                Err(Errno::INTR) => continue,
                result => return result,
            }
        }
    }

    fn retry<T>(operation: impl FnMut() -> rustix::io::Result<T>) -> io::Result<T> {
        retry_errno(operation).map_err(io::Error::from)
    }

    fn send_failure(sender: &SyncSender<DeleteEvent>, message: &str) {
        let _ = sender.send(DeleteEvent::Failed {
            message: message.to_owned(),
        });
        let _ = sender.send(DeleteEvent::Finished);
    }
}

#[cfg(test)]
mod tests {
    use super::{append_postorder, create_plan, ineligible_reason};
    use fdu_core::{EntryType, FileIdentity, NodeId, NodeState, Tree};
    use std::sync::Arc;

    fn test_tree() -> (Arc<Tree>, NodeId, NodeId, NodeId) {
        let identity = FileIdentity { device: 1, inode: 1 };
        let mut tree = Tree::new(b"root", identity, 1).unwrap();
        let directory = tree
            .append(tree.root(), b"dir", EntryType::Directory, FileIdentity { device: 1, inode: 2 }, 2, 10, 512, NodeState::Complete)
            .unwrap();
        let file = tree
            .append(directory, b"file", EntryType::RegularFile, FileIdentity { device: 1, inode: 3 }, 1, 10, 512, NodeState::Complete)
            .unwrap();
        let second = tree
            .append(directory, b"second", EntryType::RegularFile, FileIdentity { device: 1, inode: 4 }, 1, 0, 0, NodeState::Complete)
            .unwrap();
        (Arc::new(tree), directory, file, second)
    }

    #[test]
    fn plan_emits_directory_descendants_before_their_parent() {
        let (tree, directory, file, second) = test_tree();
        let mut targets = Vec::new();
        append_postorder(&tree, directory, &mut targets);
        assert_eq!(targets, vec![file, second, directory]);
    }

    #[test]
    fn ineligible_directory_is_rejected_as_a_whole() {
        let (tree, directory, file, _) = test_tree();
        let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
        mutable.set_state(file, NodeState::Incomplete);
        let tree = Arc::new(mutable);
        assert_eq!(ineligible_reason(&tree, directory, EntryType::Directory), Some("subtree contains scan errors"));
        assert!(create_plan(tree, &[directory]).is_err());
    }

    #[test]
    fn excluded_directories_and_subtrees_cannot_be_planned_for_deletion() {
        for (reason, direct, nested) in [
            (
                fdu_core::ExclusionReason::MountBoundary,
                "mount boundary is protected",
                "subtree contains an excluded mount boundary",
            ),
            (
                fdu_core::ExclusionReason::UnsupportedAlias,
                "unsupported directory alias is protected",
                "subtree contains an unsupported directory alias",
            ),
        ] {
            let (tree, directory, _, _) = test_tree();
            let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
            mutable.set_state(directory, NodeState::Excluded(reason));
            let tree = Arc::new(mutable);
            assert_eq!(ineligible_reason(&tree, directory, EntryType::Directory), Some(direct));
            assert!(create_plan(Arc::clone(&tree), &[directory]).is_err());

            let (tree, directory, file, _) = test_tree();
            let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
            mutable.set_state(file, NodeState::Excluded(reason));
            let tree = Arc::new(mutable);
            assert_eq!(ineligible_reason(&tree, directory, EntryType::Directory), Some(nested));
            assert!(create_plan(tree, &[directory]).is_err());
        }
    }

    #[test]
    fn plan_accepts_roots_from_different_directories() {
        let (tree, _, file, _) = test_tree();
        let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
        let top_level = mutable
            .append(mutable.root(), b"top", EntryType::RegularFile, FileIdentity { device: 1, inode: 9 }, 1, 5, 512, NodeState::Complete)
            .unwrap();
        let tree = Arc::new(mutable);
        let plan = create_plan(tree, &[file, top_level]).unwrap();
        assert_eq!(plan.roots, vec![file, top_level]);
        assert_eq!(plan.operation_count(), 2);
        assert_eq!(plan.apparent_bytes, 15);
    }

    #[test]
    fn plan_rejects_a_root_covered_by_another_selected_directory() {
        let (tree, directory, file, _) = test_tree();
        let rejection = create_plan(tree, &[directory, file]).err().expect("selection is rejected");
        assert_eq!(rejection.rejected.len(), 1);
        assert_eq!(rejection.rejected[0].node, Some(file));
        assert!(rejection.rejected[0].reason.contains("covered"));
    }

    #[test]
    fn plan_names_each_ineligible_root_and_deletes_none_of_the_selection() {
        let (tree, _, file, _) = test_tree();
        let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
        let blocked = mutable
            .append(mutable.root(), b"blocked", EntryType::RegularFile, FileIdentity { device: 1, inode: 9 }, 1, 5, 512, NodeState::Incomplete)
            .unwrap();
        let tree = Arc::new(mutable);
        let rejection = create_plan(tree, &[file, blocked]).err().expect("selection is rejected");
        assert_eq!(rejection.rejected.len(), 1);
        assert_eq!(rejection.rejected[0].node, Some(blocked));
    }

    mod filesystem {
        use crate::posix::run_with;
        use crate::{create_plan, DeleteEvent, DeleteOutcome, DeletionPlan, OutcomeKind};
        use fdu_core::{EntryType, FileIdentity, NodeId, NodeState, Tree};
        use rustix::fs::{Mode, OFlags};
        use std::fs;
        use std::io;
        use std::os::fd::OwnedFd;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::mpsc;
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

        struct TempRoot {
            parent: PathBuf,
            root: PathBuf,
        }

        impl TempRoot {
            fn new() -> io::Result<Self> {
                loop {
                    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
                    let parent = std::env::temp_dir().join(format!("fdu-delete-{}-{id}", std::process::id()));
                    match fs::create_dir(&parent) {
                        Ok(()) => {
                            let root = parent.join("root");
                            fs::create_dir(&root)?;
                            return Ok(Self { parent, root });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                        Err(error) => return Err(error),
                    }
                }
            }
        }

        impl Drop for TempRoot {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.parent);
            }
        }

        fn append_path(tree: &mut Tree, parent: NodeId, path: &Path, name: &[u8]) -> io::Result<NodeId> {
            let metadata = fs::symlink_metadata(path)?;
            let file_type = metadata.file_type();
            let entry_type = if file_type.is_dir() {
                EntryType::Directory
            } else if file_type.is_symlink() {
                EntryType::Symlink
            } else if file_type.is_file() {
                EntryType::RegularFile
            } else {
                EntryType::Other
            };
            let identity = FileIdentity { device: metadata.dev(), inode: metadata.ino() };
            let apparent = if entry_type == EntryType::Directory { 0 } else { metadata.len() };
            let allocated = if entry_type == EntryType::Directory {
                0
            } else {
                metadata.blocks().checked_mul(512).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "allocated size overflow"))?
            };
            let id = tree
                .append(parent, name, entry_type, identity, metadata.nlink(), apparent, allocated, NodeState::Complete)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "index exhausted"))?;
            if entry_type == EntryType::Directory {
                let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
                entries.sort_by(|left, right| left.file_name().as_bytes().cmp(right.file_name().as_bytes()));
                for entry in entries {
                    append_path(tree, id, &entry.path(), entry.file_name().as_bytes())?;
                }
            } else {
                assert!(tree.add_to_ancestors(parent, apparent, allocated));
            }
            Ok(id)
        }

        fn index_root(root: &Path) -> io::Result<Arc<Tree>> {
            let metadata = fs::symlink_metadata(root)?;
            let mut tree = Tree::new(
                root.as_os_str().as_bytes(),
                FileIdentity { device: metadata.dev(), inode: metadata.ino() },
                metadata.nlink(),
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root name exceeds index limits"))?;
            let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by(|left, right| left.file_name().as_bytes().cmp(right.file_name().as_bytes()));
            let root_id = tree.root();
            for entry in entries {
                append_path(&mut tree, root_id, &entry.path(), entry.file_name().as_bytes())?;
            }
            tree.set_state(tree.root(), NodeState::Complete);
            Ok(Arc::new(tree))
        }

        fn child(tree: &Tree, parent: NodeId, name: &[u8]) -> NodeId {
            tree.children(parent).find(|id| tree.name(*id) == Some(name)).expect("fixture entry is indexed")
        }

        fn root_fd(root: &Path) -> io::Result<OwnedFd> {
            Ok(rustix::fs::open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        }

        fn execute_plan(
            root: &Path,
            plan: DeletionPlan,
            cancel_after: Option<usize>,
        ) -> Vec<DeleteOutcome> {
            let cancelled = Arc::new(AtomicBool::new(false));
            let hook_cancelled = Arc::clone(&cancelled);
            let (sender, receiver) = mpsc::sync_channel(2);
            let root = root_fd(root).unwrap();
            let worker = thread::spawn(move || {
                // One thread makes "stop after exactly this many operations" deterministic.
                let threads = if cancel_after.is_some() { 1 } else { 4 };
                run_with(root, plan, cancelled, sender, threads, move |completed| {
                    if cancel_after == Some(completed) {
                        hook_cancelled.store(true, Ordering::Relaxed);
                    }
                });
            });
            let mut outcomes = Vec::new();
            loop {
                match receiver.recv_timeout(Duration::from_secs(5)).expect("deletion worker reports completion") {
                    DeleteEvent::Outcomes(batch) => outcomes.extend(batch),
                    DeleteEvent::Failed { message } => panic!("unexpected worker failure: {message}"),
                    DeleteEvent::Progress { .. } => {}
                    DeleteEvent::Finished => break,
                }
            }
            worker.join().expect("deletion worker exits");
            outcomes
        }

        fn outcome(outcomes: &[DeleteOutcome], node: NodeId) -> OutcomeKind {
            outcomes.iter().find(|item| item.node == node).expect("target has an outcome").kind
        }

        #[test]
        fn indexed_group_deletion_removes_symlinks_themselves_and_keeps_external_hardlinks() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let group = temp.root.join("group");
            fs::create_dir(&group)?;
            fs::create_dir(group.join("sub"))?;
            fs::write(group.join("sub/payload"), b"group data")?;
            let external_hardlink = temp.parent.join("outside-hardlink");
            fs::hard_link(group.join("sub/payload"), &external_hardlink)?;
            let sentinel = temp.parent.join("sentinel");
            fs::write(&sentinel, b"independent target")?;
            std::os::unix::fs::symlink("../../sentinel", group.join("sentinel-link"))?;

            let tree = index_root(&temp.root)?;
            let selected = child(&tree, tree.root(), b"group");
            let expected_link = child(&tree, selected, b"sentinel-link");
            let plan = create_plan(Arc::clone(&tree), &[selected]).unwrap();
            assert_eq!(plan.operation_count(), 4);
            let outcomes = execute_plan(&temp.root, plan, None);
            assert!(outcomes.iter().all(|item| item.kind == OutcomeKind::Deleted));
            assert!(!group.exists());
            assert_eq!(fs::read(&sentinel)?, b"independent target");
            assert!(external_hardlink.exists());
            assert_eq!(outcome(&outcomes, expected_link), OutcomeKind::Deleted);
            Ok(())
        }

        #[test]
        fn individual_hardlink_selection_removes_only_the_selected_name() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let first = temp.root.join("first");
            let second = temp.root.join("second");
            fs::write(&first, b"shared inode")?;
            fs::hard_link(&first, &second)?;
            let tree = index_root(&temp.root)?;
            let first_id = child(&tree, tree.root(), b"first");
            let second_id = child(&tree, tree.root(), b"second");
            assert_ne!(first_id, second_id);
            assert_eq!(tree.record(first_id).unwrap().identity, tree.record(second_id).unwrap().identity);
            let plan = create_plan(Arc::clone(&tree), &[first_id]).unwrap();
            let outcomes = execute_plan(&temp.root, plan, None);
            assert_eq!(outcome(&outcomes, first_id), OutcomeKind::Deleted);
            assert!(!first.exists());
            assert_eq!(fs::read(second)?, b"shared inode");
            Ok(())
        }

        #[test]
        fn absent_and_replaced_entries_are_distinguished_without_deleting_replacements() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let absent = temp.root.join("absent");
            let replacement_path = temp.parent.join("replacement-source");
            fs::write(&absent, b"old")?;
            fs::write(&replacement_path, b"new")?;
            let tree = index_root(&temp.root)?;
            let absent_id = child(&tree, tree.root(), b"absent");
            let plan = create_plan(Arc::clone(&tree), &[absent_id]).unwrap();
            fs::remove_file(&absent)?;
            let outcomes = execute_plan(&temp.root, plan, None);
            assert_eq!(outcome(&outcomes, absent_id), OutcomeKind::AlreadyAbsent);

            let target = temp.root.join("target");
            fs::write(&target, b"indexed original")?;
            let tree = index_root(&temp.root)?;
            let target_id = child(&tree, tree.root(), b"target");
            let plan = create_plan(Arc::clone(&tree), &[target_id]).unwrap();
            let replacement = temp.parent.join("replacement-new");
            fs::write(&replacement, b"replacement stays")?;
            fs::rename(replacement, &target)?;
            let outcomes = execute_plan(&temp.root, plan, None);
            assert_eq!(outcome(&outcomes, target_id), OutcomeKind::Changed);
            assert_eq!(fs::read(&target)?, b"replacement stays");
            Ok(())
        }

        #[test]
        fn selections_from_several_directories_are_removed_together() -> io::Result<()> {
            let temp = TempRoot::new()?;
            fs::create_dir(temp.root.join("left"))?;
            fs::create_dir(temp.root.join("right"))?;
            fs::write(temp.root.join("left/a"), b"a")?;
            fs::write(temp.root.join("left/keep"), b"keep")?;
            fs::write(temp.root.join("right/b"), b"b")?;
            fs::write(temp.root.join("top"), b"top")?;
            let tree = index_root(&temp.root)?;
            let left = child(&tree, tree.root(), b"left");
            let right = child(&tree, tree.root(), b"right");
            let a = child(&tree, left, b"a");
            let b = child(&tree, right, b"b");
            let top = child(&tree, tree.root(), b"top");
            let plan = create_plan(Arc::clone(&tree), &[a, b, top]).unwrap();
            let outcomes = execute_plan(&temp.root, plan, None);
            assert!(outcomes.iter().all(|item| item.kind == OutcomeKind::Deleted));
            assert!(!temp.root.join("left/a").exists());
            assert!(!temp.root.join("right/b").exists());
            assert!(!temp.root.join("top").exists());
            assert!(temp.root.join("left/keep").exists());
            Ok(())
        }

        #[test]
        fn deep_trees_are_removed_while_reusing_the_open_ancestor_chain() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let deep = temp.root.join("a/b/c/d");
            fs::create_dir_all(&deep)?;
            fs::create_dir_all(temp.root.join("a/b/side"))?;
            for directory in ["a", "a/b", "a/b/c", "a/b/c/d", "a/b/side"] {
                fs::write(temp.root.join(directory).join("one"), b"1")?;
                fs::write(temp.root.join(directory).join("two"), b"22")?;
            }
            let tree = index_root(&temp.root)?;
            let top = child(&tree, tree.root(), b"a");
            let plan = create_plan(Arc::clone(&tree), &[top]).unwrap();
            assert_eq!(plan.operation_count(), 15);
            let outcomes = execute_plan(&temp.root, plan, None);
            assert_eq!(outcomes.len(), 15);
            assert!(outcomes.iter().all(|item| item.kind == OutcomeKind::Deleted), "{outcomes:?}");
            assert!(!temp.root.join("a").exists());
            Ok(())
        }

        #[test]
        fn new_children_after_confirmation_leave_the_directory_visible() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let directory = temp.root.join("selected");
            fs::create_dir(&directory)?;
            fs::write(directory.join("indexed"), b"known")?;
            let tree = index_root(&temp.root)?;
            let selected = child(&tree, tree.root(), b"selected");
            let plan = create_plan(Arc::clone(&tree), &[selected]).unwrap();
            fs::write(directory.join("new-child"), b"not in the frozen plan")?;

            let outcomes = execute_plan(&temp.root, plan, None);
            assert_eq!(outcome(&outcomes, selected), OutcomeKind::Failed);
            assert!(directory.join("new-child").exists());
            assert!(directory.exists());
            assert!(!directory.join("indexed").exists());
            Ok(())
        }

        #[test]
        fn replacement_ancestors_are_not_followed_or_removed() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let directory = temp.root.join("selected");
            fs::create_dir(&directory)?;
            fs::write(directory.join("indexed"), b"inside")?;
            let outside = temp.parent.join("moved-outside");
            let tree = index_root(&temp.root)?;
            let selected = child(&tree, tree.root(), b"selected");
            let child_id = child(&tree, selected, b"indexed");
            let plan = create_plan(Arc::clone(&tree), &[selected]).unwrap();
            fs::rename(&directory, &outside)?;
            fs::create_dir(&directory)?;
            fs::write(directory.join("replacement"), b"leave me")?;

            let outcomes = execute_plan(&temp.root, plan, None);
            assert_eq!(outcome(&outcomes, child_id), OutcomeKind::Failed);
            assert_eq!(outcome(&outcomes, selected), OutcomeKind::Changed);
            assert_eq!(fs::read(outside.join("indexed"))?, b"inside");
            assert_eq!(fs::read(directory.join("replacement"))?, b"leave me");
            Ok(())
        }

        #[test]
        fn cancellation_stops_after_one_successful_removal() -> io::Result<()> {
            let temp = TempRoot::new()?;
            fs::write(temp.root.join("first"), b"first")?;
            fs::write(temp.root.join("second"), b"second")?;
            let tree = index_root(&temp.root)?;
            let first = child(&tree, tree.root(), b"first");
            let second = child(&tree, tree.root(), b"second");
            let plan = create_plan(Arc::clone(&tree), &[first, second]).unwrap();
            let outcomes = execute_plan(&temp.root, plan, Some(1));
            assert_eq!(outcome(&outcomes, first), OutcomeKind::Deleted);
            assert_eq!(outcome(&outcomes, second), OutcomeKind::Cancelled);
            assert!(!temp.root.join("first").exists());
            assert_eq!(fs::read(temp.root.join("second"))?, b"second");
            Ok(())
        }

        /// A tree with deep chains, fan-outs of small directories, and wide directories of files,
        /// so deletion splits into many tasks of very different sizes.
        fn build_mixed_tree(root: &Path) -> io::Result<usize> {
            let mut entries = 0;
            let mut make = |path: &Path, directory: bool| -> io::Result<()> {
                if directory {
                    fs::create_dir(path)?;
                } else {
                    fs::write(path, b"x")?;
                }
                entries += 1;
                Ok(())
            };
            for top in 0..10 {
                let top_dir = root.join(format!("top-{top:02}"));
                make(&top_dir, true)?;
                let mut chain = top_dir.join("chain");
                make(&chain, true)?;
                for level in 0..7 {
                    chain = chain.join(format!("level-{level}"));
                    make(&chain, true)?;
                    make(&chain.join("file"), false)?;
                }
                let fan = top_dir.join("fan");
                make(&fan, true)?;
                for branch in 0..25 {
                    let branch_dir = fan.join(format!("branch-{branch:02}"));
                    make(&branch_dir, true)?;
                    for leaf in 0..2 {
                        make(&branch_dir.join(format!("leaf-{leaf}")), false)?;
                    }
                }
                let wide = top_dir.join("wide");
                make(&wide, true)?;
                for file in 0..120 {
                    make(&wide.join(format!("file-{file:03}")), false)?;
                }
            }
            Ok(entries)
        }

        /// Many deletions at once, each with its own pool, over trees that split into tasks of very
        /// different sizes: every entry is removed once, nothing hangs, and nothing is left. Raise
        /// FDU_STRESS_DELETES for a soak.
        #[test]
        fn concurrent_deletions_remove_every_entry_exactly_once() -> io::Result<()> {
            let rounds = std::env::var("FDU_STRESS_DELETES")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(6);
            thread::scope(|scope| {
                let threads = (0..4)
                    .map(|_| {
                        scope.spawn(|| -> io::Result<()> {
                            for _ in 0..rounds {
                                let temp = TempRoot::new()?;
                                let expected = build_mixed_tree(&temp.root)?;
                                let tree = index_root(&temp.root)?;
                                let selected = tree.children(tree.root()).collect::<Vec<_>>();
                                let plan = create_plan(Arc::clone(&tree), &selected).unwrap();
                                assert_eq!(plan.operation_count(), expected);
                                let outcomes = execute_plan(&temp.root, plan, None);
                                assert_eq!(outcomes.len(), expected, "one outcome per entry");
                                assert!(
                                    outcomes.iter().all(|item| item.kind == OutcomeKind::Deleted),
                                    "{:?}",
                                    outcomes.iter().find(|item| item.kind != OutcomeKind::Deleted)
                                );
                                assert_eq!(fs::read_dir(&temp.root)?.count(), 0, "nothing is left");
                            }
                            Ok(())
                        })
                    })
                    .collect::<Vec<_>>();
                for thread in threads {
                    thread.join().expect("stress thread does not panic")?;
                }
                Ok(())
            })
        }

        #[test]
        fn incomplete_directories_are_refused_as_a_whole() -> io::Result<()> {
            let temp = TempRoot::new()?;
            let directory = temp.root.join("selected");
            fs::create_dir(&directory)?;
            fs::write(directory.join("important"), b"keep")?;
            let tree = Arc::try_unwrap(index_root(&temp.root)?).ok().unwrap();
            let selected = child(&tree, tree.root(), b"selected");
            let important = child(&tree, selected, b"important");
            let mut tree = tree;
            tree.set_state(important, NodeState::Incomplete);
            assert!(create_plan(Arc::new(tree), &[selected]).is_err());
            assert_eq!(fs::read(directory.join("important"))?, b"keep");
            Ok(())
        }
    }
}
