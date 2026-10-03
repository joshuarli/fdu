use fdu_core::{EntryType, ExclusionReason, FileIdentity, NodeId, NodeState, Tree};
use std::collections::HashSet;
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
    #[cfg(target_os = "macos")]
    execute_macos(root, plan, cancelled, sender);
    #[cfg(target_os = "linux")]
    {
        let _ = root;
        let _ = plan;
        let _ = cancelled;
        let _ = sender.send(DeleteEvent::Finished);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::CString;
    use std::fs::File;
    use std::mem;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct MountIdentity {
        device: u64,
        fsid: [i32; 2],
        mount_point: Vec<u8>,
    }

    pub(super) fn run(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
    ) {
        run_with(root, plan, cancelled, sender, |_| {});
    }

    pub(super) fn run_with(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
        mut after_operation: impl FnMut(usize),
    ) {
        let root_file = File::from(root);
        let root_fd = root_file.as_raw_fd();
        let root_record = plan.tree.record(plan.tree.root()).expect("tree has root");
        let expected_root = root_record.identity;
        let root_mount = match mount_identity(root_fd) {
            Ok(identity) if identity_of_fd(root_fd).ok() == Some(expected_root) => identity,
            Ok(_) => {
                send_failure(&sender, "scan root identity changed before deletion");
                return;
            }
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };

        let total = plan.targets.len();
        let mut outcomes = Vec::with_capacity(OUTCOME_BATCH_SIZE);
        let mut completed = 0usize;
        let mut chain = AncestorChain::default();
        let mut last_progress = std::time::Instant::now() - PROGRESS_INTERVAL;
        for (index, node) in plan.targets.iter().copied().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                for remaining in plan.targets[index..].iter().copied() {
                    outcomes.push(DeleteOutcome {
                        node: remaining,
                        kind: OutcomeKind::Cancelled,
                        message: None,
                    });
                    if outcomes.len() == OUTCOME_BATCH_SIZE && !flush(&sender, &mut outcomes) {
                        return;
                    }
                }
                break;
            }
            let Some(record) = plan.tree.record(node) else {
                outcomes.push(DeleteOutcome {
                    node,
                    kind: OutcomeKind::Failed,
                    message: Some("indexed entry is missing".to_owned()),
                });
                continue;
            };
            let parent = record.parent().expect("deletion targets have parents");
            let parent_fd = chain.open(root_fd, &root_mount, &plan.tree, parent);
            let result = match parent_fd {
                Ok(parent_fd) => delete_one(parent_fd, &root_mount, &plan.tree, node),
                Err(error) => Err((OutcomeKind::Failed, error.to_string())),
            };
            let outcome = match result {
                Ok(kind) => DeleteOutcome { node, kind, message: None },
                Err((kind, message)) => DeleteOutcome { node, kind, message: Some(message) },
            };
            completed = completed.saturating_add(1);
            after_operation(completed);
            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                last_progress = std::time::Instant::now();
                match sender.try_send(DeleteEvent::Progress {
                    completed,
                    total,
                    current: node,
                }) {
                    // A full queue means the interface is behind; the next
                    // report supersedes this one.
                    Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
                }
            }
            outcomes.push(outcome);
            if outcomes.len() == OUTCOME_BATCH_SIZE && !flush(&sender, &mut outcomes) {
                return;
            }
        }
        if !flush(&sender, &mut outcomes) {
            return;
        }
        let _ = sender.send(DeleteEvent::Finished);
        drop(root_file);
    }

    fn delete_one(
        parent_fd: RawFd,
        root_mount: &MountIdentity,
        tree: &Tree,
        node: NodeId,
    ) -> Result<OutcomeKind, (OutcomeKind, String)> {
        let record = tree.record(node).expect("deletion target exists");
        let name_bytes = tree.name(node).expect("indexed entry has name");
        let name = CString::new(name_bytes).map_err(|error| (OutcomeKind::Failed, error.to_string()))?;
        let current = match stat_at(parent_fd, &name) {
            Ok(stat) => stat,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(OutcomeKind::AlreadyAbsent),
            Err(error) => return Err((OutcomeKind::Failed, error.to_string())),
        };
        let current_identity = identity(&current).map_err(|error| (OutcomeKind::Failed, error.to_string()))?;
        let current_type = entry_type(current.st_mode);
        if current_identity != record.identity || current_type != record.entry_type {
            return Err((OutcomeKind::Changed, "entry was replaced since scanning".to_owned()));
        }
        let flags = if record.entry_type == EntryType::Directory {
            let directory = retry_interrupted(|| {
                let directory_fd = unsafe {
                    libc::openat(
                        parent_fd,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if directory_fd < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(unsafe { File::from_raw_fd(directory_fd) })
                }
            });
            let directory = match directory {
                Ok(directory) => directory,
                Err(error) => {
                    if let Ok(latest) = stat_at(parent_fd, &name) {
                        if identity(&latest).ok() != Some(record.identity)
                            || entry_type(latest.st_mode) != record.entry_type
                        {
                            return Err((OutcomeKind::Changed, "directory was replaced during validation".to_owned()));
                        }
                    }
                    return Err((OutcomeKind::Failed, error.to_string()));
                }
            };
            if identity_of_fd(directory.as_raw_fd()).ok() != Some(record.identity) {
                return Err((OutcomeKind::Changed, "directory changed during validation".to_owned()));
            }
            if mount_identity(directory.as_raw_fd()).ok().as_ref() != Some(root_mount) {
                return Err((OutcomeKind::Changed, "directory became a mount boundary".to_owned()));
            }
            libc::AT_REMOVEDIR
        } else {
            0
        };
        // Opening a directory to validate it takes time, so re-check what is
        // there just before removal. A non-directory was checked an instant
        // ago with nothing in between, so a second look would only repeat it.
        if flags == libc::AT_REMOVEDIR {
            let latest = match stat_at(parent_fd, &name) {
                Ok(stat) => stat,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(OutcomeKind::AlreadyAbsent),
                Err(error) => return Err((OutcomeKind::Failed, error.to_string())),
            };
            if identity(&latest).map_err(|error| (OutcomeKind::Failed, error.to_string()))? != record.identity
                || entry_type(latest.st_mode) != record.entry_type
            {
                return Err((OutcomeKind::Changed, "entry changed immediately before removal".to_owned()));
            }
        }
        if unsafe { libc::unlinkat(parent_fd, name.as_ptr(), flags) } == 0 {
            Ok(OutcomeKind::Deleted)
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(OutcomeKind::AlreadyAbsent)
            } else if error.kind() == io::ErrorKind::Interrupted {
                Err((
                    OutcomeKind::Failed,
                    "unlink was interrupted; removal outcome is uncertain, refresh required".to_owned(),
                ))
            } else if error.raw_os_error() == Some(libc::ENOTEMPTY) {
                Err((OutcomeKind::Failed, "directory gained unindexed children".to_owned()))
            } else {
                Err((OutcomeKind::Failed, error.to_string()))
            }
        }
    }

    /// The open directories from just below the scan root down to the most
    /// recent parent. Deletion visits an entry's children before the entry, so
    /// consecutive parents share most of their path; keeping the shared
    /// directories open avoids re-walking and re-validating it from the root
    /// for every directory. Each directory is opened once, relative to its
    /// parent and checked against its indexed identity, so an ancestor replaced
    /// after it was opened is never followed.
    #[derive(Default)]
    struct AncestorChain {
        directories: Vec<(NodeId, File)>,
    }

    impl AncestorChain {
        /// Returns the descriptor of `parent`, opening only the directories not
        /// already in the chain.
        fn open(
            &mut self,
            root_fd: RawFd,
            root_mount: &MountIdentity,
            tree: &Tree,
            parent: NodeId,
        ) -> io::Result<RawFd> {
            let mut path = Vec::new();
            let mut current = Some(parent);
            while let Some(node) = current {
                if node == tree.root() {
                    break;
                }
                let record = tree
                    .record(node)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "parent missing from index"))?;
                path.push(node);
                current = record.parent();
            }
            path.reverse();
            let shared = self
                .directories
                .iter()
                .zip(&path)
                .take_while(|((cached, _), wanted)| cached == *wanted)
                .count();
            self.directories.truncate(shared);
            for node in &path[shared..] {
                let directory_fd = self.directories.last().map_or(root_fd, |(_, file)| file.as_raw_fd());
                let child = open_validated_child(directory_fd, root_mount, tree, *node)?;
                self.directories.push((*node, child));
            }
            Ok(self.directories.last().map_or(root_fd, |(_, file)| file.as_raw_fd()))
        }
    }

    fn open_validated_child(
        directory_fd: RawFd,
        root_mount: &MountIdentity,
        tree: &Tree,
        node: NodeId,
    ) -> io::Result<File> {
        let name = CString::new(tree.name(node).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "directory name missing"))?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let child = retry_interrupted(|| {
            let fd = unsafe {
                libc::openat(
                    directory_fd,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(unsafe { File::from_raw_fd(fd) })
            }
        })?;
        let expected = tree.record(node).expect("path node exists");
        if identity_of_fd(child.as_raw_fd())? != expected.identity {
            return Err(io::Error::new(io::ErrorKind::Other, "ancestor directory was replaced"));
        }
        if mount_identity(child.as_raw_fd())? != *root_mount {
            return Err(io::Error::new(io::ErrorKind::Other, "ancestor became a mount boundary"));
        }
        Ok(child)
    }

    fn stat_at(parent: RawFd, name: &CString) -> io::Result<libc::stat> {
        let mut metadata = mem::MaybeUninit::<libc::stat>::uninit();
        retry_interrupted(|| {
            if unsafe { libc::fstatat(parent, name.as_ptr(), metadata.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })?;
        Ok(unsafe { metadata.assume_init() })
    }

    fn identity(metadata: &libc::stat) -> io::Result<FileIdentity> {
        Ok(FileIdentity {
            device: u64::try_from(metadata.st_dev).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid device number"))?,
            inode: u64::try_from(metadata.st_ino).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid inode number"))?,
        })
    }

    fn identity_of_fd(fd: RawFd) -> io::Result<FileIdentity> {
        let mut metadata = mem::MaybeUninit::<libc::stat>::uninit();
        retry_interrupted(|| {
            if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })?;
        identity(unsafe { metadata.assume_init_ref() })
    }

    fn entry_type(mode: libc::mode_t) -> EntryType {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => EntryType::Directory,
            libc::S_IFREG => EntryType::RegularFile,
            libc::S_IFLNK => EntryType::Symlink,
            _ => EntryType::Other,
        }
    }

    fn mount_identity(fd: RawFd) -> io::Result<MountIdentity> {
        let mut metadata = mem::MaybeUninit::<libc::stat>::uninit();
        retry_interrupted(|| {
            if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })?;
        let mut filesystem = mem::MaybeUninit::<libc::statfs>::zeroed();
        retry_interrupted(|| {
            if unsafe { libc::fstatfs(fd, filesystem.as_mut_ptr()) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })?;
        let metadata = unsafe { metadata.assume_init_ref() };
        let filesystem = unsafe { filesystem.assume_init_ref() };
        let fsid = unsafe { mem::transmute::<libc::fsid_t, [i32; 2]>(filesystem.f_fsid) };
        let mount_point = filesystem
            .f_mntonname
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        Ok(MountIdentity {
            device: u64::try_from(metadata.st_dev).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid device number"))?,
            fsid,
            mount_point,
        })
    }

    fn flush(sender: &SyncSender<DeleteEvent>, outcomes: &mut Vec<DeleteOutcome>) -> bool {
        if outcomes.is_empty() {
            return true;
        }
        let values = std::mem::replace(outcomes, Vec::with_capacity(OUTCOME_BATCH_SIZE));
        sender.send(DeleteEvent::Outcomes(values)).is_ok()
    }

    fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        loop {
            match operation() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    fn send_failure(sender: &SyncSender<DeleteEvent>, message: &str) {
        let _ = sender.send(DeleteEvent::Failed {
            message: message.to_owned(),
        });
        let _ = sender.send(DeleteEvent::Finished);
    }

    pub(super) fn execute_macos(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
    ) {
        run(root, plan, cancelled, sender)
    }
}

#[cfg(target_os = "macos")]
use macos::execute_macos;

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

    #[cfg(target_os = "macos")]
    mod macos_filesystem {
        use crate::macos::run_with;
        use crate::{create_plan, DeleteEvent, DeleteOutcome, DeletionPlan, OutcomeKind};
        use fdu_core::{EntryType, FileIdentity, NodeId, NodeState, Tree};
        use std::fs::{self, OpenOptions};
        use std::io;
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(root)?;
            let raw = file.into_raw_fd();
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
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
                run_with(root, plan, cancelled, sender, move |completed| {
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
