use crate::{RootAnchor, ScanQueueMetrics, ScanReport, TopLevelDirectory};
use fdu_core::{
    DirectoryToken, EntryBatch, EntryType, ExclusionReason, FileIdentity, NodeState, ScanEntry,
    ScanEvent,
};
use std::ffi::{CStr, CString, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr::NonNull;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

const DIRECTORY_OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const DIRECTORY_ENTRY: libc::c_uchar = libc::DT_DIR;
const UNKNOWN_ENTRY: libc::c_uchar = libc::DT_UNKNOWN;
const FILE_TYPE_MASK: libc::mode_t = libc::S_IFMT;
const DIRECTORY_TYPE: libc::mode_t = libc::S_IFDIR;

struct DirectoryEntry {
    name: CString,
    kind: EntryKind,
}

#[derive(Clone, Copy)]
enum EntryKind {
    Directory,
    Other,
    Unknown,
}

struct DirectoryPath {
    parent: Option<usize>,
    name: Option<CString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MountIdentity {
    device: libc::dev_t,
    filesystem_id: [i32; 2],
    mount_point: Vec<u8>,
}

enum OpenedDirectory {
    InsideRoot(File),
    MountBoundary,
}

struct DirectoryStream {
    stream: NonNull<libc::DIR>,
    fd: RawFd,
}

impl DirectoryStream {
    fn from_file(directory: File) -> io::Result<Self> {
        let fd = directory.into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        match NonNull::new(stream) {
            Some(stream) => Ok(Self { stream, fd }),
            None => {
                let error = io::Error::last_os_error();
                drop(unsafe { File::from_raw_fd(fd) });
                Err(error)
            }
        }
    }

    fn fd(&self) -> RawFd {
        self.fd
    }

    fn read_all(mut self) -> io::Result<(Self, Vec<DirectoryEntry>)> {
        let mut entries = Vec::new();
        while let Some(entry) = self.next_entry()? {
            if entry.name.as_bytes() != b"." && entry.name.as_bytes() != b".." {
                entries.push(entry);
            }
        }
        Ok((self, entries))
    }

    fn next_entry(&mut self) -> io::Result<Option<DirectoryEntry>> {
        let Some((name, kind)) = self.next_entry_name()? else {
            return Ok(None);
        };
        Ok(Some(DirectoryEntry {
            name: name.to_owned(),
            kind: match kind {
                DIRECTORY_ENTRY => EntryKind::Directory,
                UNKNOWN_ENTRY => EntryKind::Unknown,
                _ => EntryKind::Other,
            },
        }))
    }

    fn next_entry_name(&mut self) -> io::Result<Option<(&CStr, libc::c_uchar)>> {
        loop {
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(self.stream.as_ptr()) };
            if entry.is_null() {
                let error = unsafe { *libc::__error() };
                if error == libc::EINTR {
                    continue;
                }
                return if readdir_null_result(error)? {
                    Ok(None)
                } else {
                    unreachable!("a null readdir result cannot describe an entry")
                };
            }

            let entry = unsafe { &*entry };
            let name_length = usize::from(entry.d_namlen);
            if name_length >= entry.d_name.len() || entry.d_name[name_length] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid macOS directory entry name",
                ));
            }
            let name_bytes = unsafe {
                std::slice::from_raw_parts(entry.d_name.as_ptr().cast::<u8>(), name_length)
            };
            if name_bytes.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "macOS directory entry name contains a NUL byte",
                ));
            }
            let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
            if name.to_bytes().len() != name_length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid macOS directory entry name length",
                ));
            } else {
                return Ok(Some((name, entry.d_type)));
            }
        }
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.stream.as_ptr());
        }
    }
}

pub(super) fn scan(path: &Path, apparent: bool) -> io::Result<ScanReport> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    let (root, entries) = DirectoryStream::from_file(root)?.read_all()?;
    let root_mount = mount_identity(root.fd())?;
    let mut seen_directories = HashSet::new();
    seen_directories.insert(identity_for_fd(root.fd())?);
    let mut report = ScanReport {
        directories: Vec::new(),
        skipped_entries: 0,
        mount_boundaries: 0,
        unsupported_aliases: 0,
    };

    for entry in entries {
        let is_directory = match entry.kind {
            EntryKind::Directory => true,
            EntryKind::Other => false,
            EntryKind::Unknown => match with_stat_at(root.fd(), &entry.name, |stat| {
                Ok(is_directory(stat.st_mode))
            }) {
                Ok(is_directory) => is_directory,
                Err(_) => {
                    mark_skipped(&mut report.skipped_entries);
                    continue;
                }
            },
        };
        if !is_directory {
            continue;
        }

        let directory = match open_child_directory(root.fd(), &entry.name) {
            Ok(directory) => directory,
            Err(_) => {
                mark_skipped(&mut report.skipped_entries);
                continue;
            }
        };
        let directory = match keep_directory_on_root_mount(directory, &root_mount) {
            Ok(OpenedDirectory::InsideRoot(directory)) => directory,
            Ok(OpenedDirectory::MountBoundary) => {
                mark_mount_boundary(&mut report.mount_boundaries);
                report.directories.push(TopLevelDirectory {
                    name: OsString::from_vec(entry.name.as_bytes().to_vec()),
                    disk_size: 0,
                    exclusion: Some(ExclusionReason::MountBoundary),
                });
                continue;
            }
            Err(_) => {
                mark_skipped(&mut report.skipped_entries);
                continue;
            }
        };
        let identity = match identity_for_fd(directory.as_raw_fd()) {
            Ok(identity) => identity,
            Err(_) => {
                mark_skipped(&mut report.skipped_entries);
                continue;
            }
        };
        if !seen_directories.insert(identity) {
            mark_unsupported_alias(&mut report.unsupported_aliases);
            report.directories.push(TopLevelDirectory {
                name: OsString::from_vec(entry.name.as_bytes().to_vec()),
                disk_size: 0,
                exclusion: Some(ExclusionReason::UnsupportedAlias),
            });
            continue;
        }
        match scan_subtree(
            directory,
            &root_mount,
            apparent,
            &mut seen_directories,
            &mut report.skipped_entries,
            &mut report.mount_boundaries,
            &mut report.unsupported_aliases,
        ) {
            Ok(disk_size) => report.directories.push(TopLevelDirectory {
                name: OsString::from_vec(entry.name.as_bytes().to_vec()),
                disk_size,
                exclusion: None,
            }),
            Err(_) => mark_skipped(&mut report.skipped_entries),
        }
    }

    report
        .directories
        .sort_unstable_by(|left, right| {
            left.exclusion
                .is_some()
                .cmp(&right.exclusion.is_some())
                .then_with(|| right.disk_size.cmp(&left.disk_size))
        });
    Ok(report)
}

struct IndexedDirectoryPath {
    parent: Option<usize>,
    name: Option<Range<u32>>,
    identity: FileIdentity,
    token: DirectoryToken,
    children_remaining: usize,
    enumerated: bool,
    complete: bool,
}

const INDEXED_BATCH_SIZE: usize = 256;

struct ScanEventSender<'a> {
    sender: &'a SyncSender<ScanEvent>,
    metrics: &'a ScanQueueMetrics,
}

impl ScanEventSender<'_> {
    fn send(&self, event: ScanEvent) -> Result<(), ScanEvent> {
        self.metrics.send(self.sender, event)
    }
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
    if let Err(error) = scan_indexed_inner(anchor, &event_sender, &returned_batches, &cancelled) {
        let _ = event_sender.send(ScanEvent::Failed {
            message: error.to_string(),
        });
    }
    let _ = event_sender.send(ScanEvent::Finished);
}

fn scan_indexed_inner(
    anchor: &RootAnchor,
    sender: &ScanEventSender<'_>,
    returned_batches: &Receiver<EntryBatch>,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let root = File::from(anchor.try_clone_fd()?);
    let root_identity = identity_for_fd(root.as_raw_fd())?;
    let root_mount = mount_identity(root.as_raw_fd())?;
    if root_identity != anchor.identity {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "scan root identity changed after it was opened",
        ));
    }
    let root_metadata = anchor.identity;
    sender
        .send(ScanEvent::Started {
            root_name: anchor.name.clone(),
            identity: root_metadata,
            link_count: anchor.link_count,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "scan receiver was closed"))?;

    let mut directory_names = Vec::new();
    let mut paths = vec![IndexedDirectoryPath {
        parent: None,
        name: None,
        identity: root_metadata,
        token: DirectoryToken(0),
        children_remaining: 0,
        enumerated: false,
        complete: true,
    }];
    let mut pending = vec![0usize];
    let mut seen_directories = HashSet::new();
    seen_directories.insert(root_metadata);
    let mut batch = take_indexed_batch(returned_batches, DirectoryToken(0));

    while let Some(path_index) = pending.pop() {
        if cancelled.load(Ordering::Relaxed) {
            let _ = sender.send(ScanEvent::Cancelled);
            return Ok(());
        }
        let token = paths[path_index].token;
        batch.clear_for_directory(token);
        let directory = if path_index == 0 {
            root.try_clone().map(OpenedDirectory::InsideRoot)
        } else {
            open_indexed_directory(
                root.as_raw_fd(),
                &root_mount,
                &paths,
                &directory_names,
                path_index,
            )
        };
        let directory = match directory {
            Ok(OpenedDirectory::InsideRoot(directory)) => directory,
            Ok(OpenedDirectory::MountBoundary) => {
                let _ = sender.send(ScanEvent::DirectoryExcluded {
                    directory: token,
                    reason: ExclusionReason::MountBoundary,
                });
                paths[path_index].complete = false;
                paths[path_index].enumerated = true;
                if !finish_indexed_directory(path_index, &mut paths, sender)? {
                    return Ok(());
                }
                continue;
            }
            Err(error) => {
                paths[path_index].complete = false;
                if !send_scan_event(
                    sender,
                    ScanEvent::DirectoryFailed {
                        directory: token,
                        message: error.to_string(),
                    },
                ) {
                    return Ok(());
                }
                paths[path_index].enumerated = true;
                if !finish_indexed_directory(path_index, &mut paths, sender)? {
                    return Ok(());
                }
                continue;
            }
        };
        let directory = match keep_directory_on_root_mount(directory, &root_mount)? {
            OpenedDirectory::InsideRoot(directory) => directory,
            OpenedDirectory::MountBoundary => {
                if !send_scan_event(
                    sender,
                    ScanEvent::DirectoryExcluded {
                        directory: token,
                        reason: ExclusionReason::MountBoundary,
                    },
                ) {
                    return Ok(());
                }
                paths[path_index].complete = false;
                paths[path_index].enumerated = true;
                if !finish_indexed_directory(path_index, &mut paths, sender)? {
                    return Ok(());
                }
                continue;
            }
        };
        if identity_for_fd(directory.as_raw_fd())? != paths[path_index].identity {
            paths[path_index].complete = false;
            if !send_scan_event(
                sender,
                ScanEvent::DirectoryFailed {
                    directory: token,
                    message: "directory identity changed during scan".to_owned(),
                },
            ) {
                return Ok(());
            }
            paths[path_index].enumerated = true;
            if !finish_indexed_directory(path_index, &mut paths, sender)? {
                return Ok(());
            }
            continue;
        }
        let mut stream = match DirectoryStream::from_file(directory) {
            Ok(stream) => stream,
            Err(error) => {
                paths[path_index].complete = false;
                if !send_scan_event(
                    sender,
                    ScanEvent::DirectoryFailed {
                        directory: token,
                        message: error.to_string(),
                    },
                ) {
                    return Ok(());
                }
                paths[path_index].enumerated = true;
                if !finish_indexed_directory(path_index, &mut paths, sender)? {
                    return Ok(());
                }
                continue;
            }
        };
        loop {
            if cancelled.load(Ordering::Relaxed) {
                if !publish_indexed_batch(sender, returned_batches, &mut batch) {
                    return Ok(());
                }
                let _ = sender.send(ScanEvent::Cancelled);
                return Ok(());
            }
            let directory_fd = stream.fd();
            let (name, _) = match stream.next_entry_name() {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    paths[path_index].complete = false;
                    if !publish_indexed_batch(sender, returned_batches, &mut batch)
                        || !send_scan_event(
                            sender,
                            ScanEvent::DirectoryFailed {
                                directory: token,
                                message: error.to_string(),
                            },
                        )
                    {
                        return Ok(());
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
            let mut child_path = None;
            let metadata = with_stat_at(directory_fd, name, |stat| {
                let identity = file_identity(stat)?;
                let link_count = u64::try_from(stat.st_nlink).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "negative link count")
                })?;
                Ok((identity, link_count, entry_type_for_mode(stat.st_mode), file_size(stat, true), file_size(stat, false)))
            });
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
                            paths[path_index].complete = false;
                            if !send_scan_event(
                                sender,
                                ScanEvent::DirectoryFailed {
                                    directory: token,
                                    message: error.to_string(),
                                },
                            ) {
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                    if entry_type == EntryType::Directory {
                        match open_child_directory(directory_fd, name) {
                            Ok(child) => {
                                let opened_identity = match identity_for_fd(child.as_raw_fd()) {
                                    Ok(identity) => identity,
                                    Err(error) => {
                                        state = NodeState::Incomplete;
                                        paths[path_index].complete = false;
                                        if !send_scan_event(
                                            sender,
                                            ScanEvent::DirectoryFailed {
                                                directory: token,
                                                message: error.to_string(),
                                            },
                                        ) {
                                            return Ok(());
                                        }
                                        identity
                                    }
                                };
                                if opened_identity != identity {
                                    state = NodeState::Incomplete;
                                    paths[path_index].complete = false;
                                    if !send_scan_event(
                                        sender,
                                        ScanEvent::DirectoryFailed {
                                            directory: token,
                                            message: "directory identity changed during scan".to_owned(),
                                        },
                                    ) {
                                        return Ok(());
                                    }
                                } else {
                                    match keep_directory_on_root_mount(child, &root_mount)? {
                                        OpenedDirectory::MountBoundary => {
                                            state = NodeState::Excluded(ExclusionReason::MountBoundary);
                                            paths[path_index].complete = false;
                                        }
                                        OpenedDirectory::InsideRoot(child) => {
                                            if !seen_directories.insert(identity) {
                                                state = NodeState::Excluded(ExclusionReason::UnsupportedAlias);
                                                paths[path_index].complete = false;
                                            } else {
                                                let name_start = u32::try_from(directory_names.len()).map_err(|_| {
                                                    io::Error::new(io::ErrorKind::OutOfMemory, "directory names exceed index limits")
                                                })?;
                                                let name_end = name_start
                                                    .checked_add(u32::try_from(name_bytes.len()).map_err(|_| {
                                                        io::Error::new(io::ErrorKind::InvalidData, "directory name is too long")
                                                    })?)
                                                    .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "directory names exceed index limits"))?;
                                                directory_names.extend_from_slice(name_bytes);
                                                let child_index = paths.len();
                                                let child_token = DirectoryToken(u32::try_from(child_index).map_err(|_| {
                                                    io::Error::new(io::ErrorKind::OutOfMemory, "too many directories in index")
                                                })?);
                                                paths.push(IndexedDirectoryPath {
                                                    parent: Some(path_index),
                                                    name: Some(name_start..name_end),
                                                    identity,
                                                    token: child_token,
                                                    children_remaining: 0,
                                                    enumerated: false,
                                                    complete: true,
                                                });
                                                paths[path_index].children_remaining += 1;
                                                pending.push(child_index);
                                                child_path = Some(child_token);
                                                drop(child);
                                            }
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                state = NodeState::Incomplete;
                                paths[path_index].complete = false;
                                if !send_scan_event(
                                    sender,
                                    ScanEvent::DirectoryFailed {
                                        directory: token,
                                        message: error.to_string(),
                                    },
                                ) {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    state = NodeState::Incomplete;
                    paths[path_index].complete = false;
                    if !send_scan_event(
                        sender,
                        ScanEvent::DirectoryFailed {
                            directory: token,
                            message: error.to_string(),
                        },
                    ) {
                        return Ok(());
                    }
                }
            }

            let name_start = match u32::try_from(batch.names.len()) {
                Ok(start) => start,
                Err(_) => {
                    paths[path_index].complete = false;
                    if !send_scan_event(
                        sender,
                        ScanEvent::DirectoryFailed {
                            directory: token,
                            message: "directory batch exceeded supported name storage".to_owned(),
                        },
                    ) {
                        return Ok(());
                    }
                    break;
                }
            };
            let name_end = match name_start.checked_add(u32::try_from(name_bytes.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "filename is too long")
            })?) {
                Some(end) => end,
                None => {
                    paths[path_index].complete = false;
                    break;
                }
            };
            batch.names.extend_from_slice(name_bytes);
            batch.entries.push(ScanEntry {
                name: name_start..name_end,
                directory_token: child_path,
                entry_type,
                identity,
                link_count,
                apparent_bytes,
                allocated_bytes,
                state,
            });
            if batch.entries.len() >= INDEXED_BATCH_SIZE
                && !publish_indexed_batch(sender, returned_batches, &mut batch)
            {
                return Ok(());
            }
        }
        if !publish_indexed_batch(sender, returned_batches, &mut batch) {
            return Ok(());
        }
        paths[path_index].enumerated = true;
        if !finish_indexed_directory(path_index, &mut paths, sender)? {
            return Ok(());
        }
    }
    Ok(())
}

fn take_indexed_batch(
    returned_batches: &Receiver<EntryBatch>,
    directory: DirectoryToken,
) -> EntryBatch {
    returned_batches
        .try_recv()
        .unwrap_or_else(|_| EntryBatch::with_capacity(directory, INDEXED_BATCH_SIZE, INDEXED_BATCH_SIZE * 24))
}

fn publish_indexed_batch(
    sender: &ScanEventSender<'_>,
    returned_batches: &Receiver<EntryBatch>,
    batch: &mut EntryBatch,
) -> bool {
    if batch.entries.is_empty() {
        return true;
    }
    let mut next = take_indexed_batch(returned_batches, batch.directory);
    next.clear_for_directory(batch.directory);
    let current = std::mem::replace(batch, next);
    sender.send(ScanEvent::Entries(current)).is_ok()
}

fn send_scan_event(sender: &ScanEventSender<'_>, event: ScanEvent) -> bool {
    sender.send(event).is_ok()
}

fn finish_indexed_directory(
    start: usize,
    paths: &mut [IndexedDirectoryPath],
    sender: &ScanEventSender<'_>,
) -> io::Result<bool> {
    let mut current = start;
    loop {
        if !paths[current].enumerated || paths[current].children_remaining != 0 {
            return Ok(true);
        }
        let token = paths[current].token;
        let complete = paths[current].complete;
        if !send_scan_event(sender, ScanEvent::DirectoryFinished { directory: token, complete }) {
            return Ok(false);
        }
        let Some(parent) = paths[current].parent else {
            return Ok(true);
        };
        paths[current].enumerated = false;
        paths[parent].children_remaining = paths[parent].children_remaining.saturating_sub(1);
        if !complete {
            paths[parent].complete = false;
        }
        current = parent;
    }
}

fn open_indexed_directory(
    root_fd: RawFd,
    root_mount: &MountIdentity,
    paths: &[IndexedDirectoryPath],
    directory_names: &[u8],
    path_index: usize,
) -> io::Result<OpenedDirectory> {
    let mut components = Vec::new();
    let mut current_index = Some(path_index);
    while let Some(index) = current_index {
        let path = &paths[index];
        let Some(parent) = path.parent else {
            break;
        };
        components.push(index);
        current_index = Some(parent);
    }
    let mut current = None;
    for component in components.into_iter().rev() {
        let parent_fd = current.as_ref().map_or(root_fd, AsRawFd::as_raw_fd);
        let range = paths[component].name.as_ref().expect("directory path has a name");
        let name = CString::new(&directory_names[range.start as usize..range.end as usize])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let child = open_child_directory(parent_fd, &name)?;
        match keep_directory_on_root_mount(child, root_mount)? {
            OpenedDirectory::MountBoundary => return Ok(OpenedDirectory::MountBoundary),
            OpenedDirectory::InsideRoot(child) => {
                if identity_for_fd(child.as_raw_fd())? != paths[component].identity {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "directory identity changed during scan",
                    ));
                }
                current = Some(child);
            }
        }
    }
    current.map(OpenedDirectory::InsideRoot).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "cannot reopen root as a child directory")
    })
}

fn file_identity(stat: &libc::stat) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        device: u64::try_from(stat.st_dev).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "device number is outside supported range")
        })?,
        inode: u64::try_from(stat.st_ino).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "inode number is outside supported range")
        })?,
    })
}

fn identity_for_fd(fd: RawFd) -> io::Result<FileIdentity> {
    let mut metadata = mem::MaybeUninit::<libc::stat>::uninit();
    retry_interrupted(|| {
        if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })?;
    file_identity(unsafe { metadata.assume_init_ref() })
}

fn entry_type_for_mode(mode: libc::mode_t) -> EntryType {
    match mode & FILE_TYPE_MASK {
        DIRECTORY_TYPE => EntryType::Directory,
        libc::S_IFREG => EntryType::RegularFile,
        libc::S_IFLNK => EntryType::Symlink,
        _ => EntryType::Other,
    }
}

fn scan_subtree(
    root: File,
    root_mount: &MountIdentity,
    apparent: bool,
    seen_directories: &mut HashSet<FileIdentity>,
    skipped_entries: &mut usize,
    mount_boundaries: &mut usize,
    unsupported_aliases: &mut usize,
) -> io::Result<u64> {
    let mut paths = vec![DirectoryPath {
        parent: None,
        name: None,
    }];
    let mut pending = vec![0usize];
    let mut disk_size = 0u64;

    while let Some(path_index) = pending.pop() {
        let directory = if path_index == 0 {
            match root.try_clone() {
                Ok(directory) => OpenedDirectory::InsideRoot(directory),
                Err(error) => {
                    return Err(error);
                }
            }
        } else {
            match open_directory_path(root.as_raw_fd(), root_mount, &paths, path_index) {
                Ok(directory) => directory,
                Err(_) => {
                    mark_skipped(skipped_entries);
                    continue;
                }
            }
        };
        let directory = match directory {
            OpenedDirectory::InsideRoot(directory) => directory,
            OpenedDirectory::MountBoundary => {
                mark_mount_boundary(mount_boundaries);
                continue;
            }
        };
        if path_index != 0 {
            let identity = match identity_for_fd(directory.as_raw_fd()) {
                Ok(identity) => identity,
                Err(_) => {
                    mark_skipped(skipped_entries);
                    continue;
                }
            };
            if !seen_directories.insert(identity) {
                mark_unsupported_alias(unsupported_aliases);
                continue;
            }
        }

        let stream = match DirectoryStream::from_file(directory) {
            Ok(stream) => stream,
            Err(error) if path_index == 0 => return Err(error),
            Err(_) => {
                mark_skipped(skipped_entries);
                continue;
            }
        };
        let (stream, entries) = match stream.read_all() {
            Ok(contents) => contents,
            Err(error) if path_index == 0 => return Err(error),
            Err(_) => {
                mark_skipped(skipped_entries);
                continue;
            }
        };

        for entry in entries {
            let child_directory = match entry.kind {
                EntryKind::Directory => true,
                EntryKind::Other => {
                    match with_stat_at(stream.fd(), &entry.name, |stat| {
                        file_size(stat, apparent)
                    }) {
                        Ok(size) => add_size(&mut disk_size, size, skipped_entries),
                        Err(_) => mark_skipped(skipped_entries),
                    }
                    continue;
                }
                EntryKind::Unknown => {
                    match with_stat_at(stream.fd(), &entry.name, |stat| {
                        unknown_entry(stat, apparent)
                    }) {
                        Ok(UnknownEntry::Directory) => true,
                        Ok(UnknownEntry::File(size)) => {
                            add_size(&mut disk_size, size, skipped_entries);
                            continue;
                        }
                        Err(_) => {
                            mark_skipped(skipped_entries);
                            continue;
                        }
                    }
                }
            };

            if child_directory {
                let child_index = paths.len();
                paths.push(DirectoryPath {
                    parent: Some(path_index),
                    name: Some(entry.name),
                });
                pending.push(child_index);
            }
        }
    }

    Ok(disk_size)
}

enum UnknownEntry {
    Directory,
    File(u64),
}

fn unknown_entry(stat: &libc::stat, apparent: bool) -> io::Result<UnknownEntry> {
    if is_directory(stat.st_mode) {
        Ok(UnknownEntry::Directory)
    } else {
        file_size(stat, apparent).map(UnknownEntry::File)
    }
}

fn open_directory_path(
    root_fd: RawFd,
    root_mount: &MountIdentity,
    paths: &[DirectoryPath],
    path_index: usize,
) -> io::Result<OpenedDirectory> {
    let mut components = Vec::new();
    let mut current_index = Some(path_index);
    while let Some(index) = current_index {
        let path = &paths[index];
        let Some(_) = path.parent else {
            break;
        };
        components.push(index);
        current_index = path.parent;
    }

    let mut current = None;
    for component in components.into_iter().rev() {
        let parent_fd = current.as_ref().map_or(root_fd, AsRawFd::as_raw_fd);
        let name = paths[component]
            .name
            .as_ref()
            .expect("non-root directory path has a name");
        let directory = open_child_directory(parent_fd, name)?;
        match keep_directory_on_root_mount(directory, root_mount)? {
            OpenedDirectory::InsideRoot(directory) => current = Some(directory),
            OpenedDirectory::MountBoundary => return Ok(OpenedDirectory::MountBoundary),
        }
    }
    current.map(OpenedDirectory::InsideRoot).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot reopen the root as a child directory",
        )
    })
}

fn open_child_directory(parent_fd: RawFd, name: &CStr) -> io::Result<File> {
    retry_interrupted(|| {
        let child_fd = unsafe { libc::openat(parent_fd, name.as_ptr(), DIRECTORY_OPEN_FLAGS) };
        if child_fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(child_fd) })
        }
    })
}

fn keep_directory_on_root_mount(
    directory: File,
    root_mount: &MountIdentity,
) -> io::Result<OpenedDirectory> {
    let identity = mount_identity(directory.as_raw_fd())?;
    if same_mount(&identity, root_mount) {
        Ok(OpenedDirectory::InsideRoot(directory))
    } else {
        Ok(OpenedDirectory::MountBoundary)
    }
}

fn same_mount(left: &MountIdentity, right: &MountIdentity) -> bool {
    left == right
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
    let filesystem_id = fsid_words(filesystem.f_fsid);
    let mount_point = filesystem
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();

    Ok(MountIdentity {
        device: metadata.st_dev,
        filesystem_id,
        mount_point,
    })
}

const _: [(); 8] = [(); mem::size_of::<libc::fsid_t>()];

fn fsid_words(filesystem_id: libc::fsid_t) -> [i32; 2] {
    // Apple's fsid_t is exactly two i32 words; libc keeps those fields private.
    unsafe { mem::transmute(filesystem_id) }
}

fn with_stat_at<T>(
    parent_fd: RawFd,
    name: &CStr,
    inspect: impl FnOnce(&libc::stat) -> io::Result<T>,
) -> io::Result<T> {
    let mut stat = mem::MaybeUninit::<libc::stat>::uninit();
    retry_interrupted(|| {
        let result = unsafe {
            libc::fstatat(
                parent_fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })?;
    inspect(unsafe { stat.assume_init_ref() })
}

fn file_size(stat: &libc::stat, apparent: bool) -> io::Result<u64> {
    if apparent {
        u64::try_from(stat.st_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "file size is outside the supported byte-count range",
            )
        })
    } else {
        u64::try_from(stat.st_blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(512))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "allocated file size is outside the supported byte-count range",
                )
            })
    }
}

fn is_directory(mode: libc::mode_t) -> bool {
    mode & FILE_TYPE_MASK == DIRECTORY_TYPE
}

fn add_size(total: &mut u64, size: u64, skipped_entries: &mut usize) {
    match total.checked_add(size) {
        Some(value) => *total = value,
        None => mark_skipped(skipped_entries),
    }
}

fn mark_skipped(skipped_entries: &mut usize) {
    *skipped_entries = skipped_entries.saturating_add(1);
}

fn mark_mount_boundary(mount_boundaries: &mut usize) {
    *mount_boundaries = mount_boundaries.saturating_add(1);
}

fn mark_unsupported_alias(unsupported_aliases: &mut usize) {
    *unsupported_aliases = unsupported_aliases.saturating_add(1);
}

fn readdir_null_result(errno: libc::c_int) -> io::Result<bool> {
    if errno == 0 {
        Ok(true)
    } else {
        Err(io::Error::from_raw_os_error(errno))
    }
}

fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{readdir_null_result, same_mount, MountIdentity, UnknownEntry};
    use std::io;

    #[test]
    fn directory_enumeration_errors_are_distinct_from_end_of_directory() {
        assert!(readdir_null_result(0).unwrap());
        assert_eq!(
            readdir_null_result(libc::EIO).unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert_eq!(
            readdir_null_result(libc::EINTR).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn allocated_file_size_conversion_rejects_negative_and_overflowing_values() {
        let negative_size = libc::stat {
            st_size: -1,
            st_blocks: 0,
            ..unsafe { std::mem::zeroed() }
        };
        assert!(super::file_size(&negative_size, true).is_err());

        let overflowing_blocks = libc::stat {
            st_size: 0,
            st_blocks: libc::blkcnt_t::MAX,
            ..unsafe { std::mem::zeroed() }
        };
        assert!(super::file_size(&overflowing_blocks, false).is_err());
    }

    #[test]
    fn metadata_resolves_unknown_types_before_counting_or_descending() {
        let directory = libc::stat {
            st_mode: libc::S_IFDIR,
            ..unsafe { std::mem::zeroed() }
        };
        assert!(matches!(
            super::unknown_entry(&directory, false).unwrap(),
            UnknownEntry::Directory
        ));

        let file = libc::stat {
            st_mode: libc::S_IFREG,
            st_size: 8,
            st_blocks: 1,
            ..unsafe { std::mem::zeroed() }
        };
        assert!(matches!(
            super::unknown_entry(&file, true).unwrap(),
            UnknownEntry::File(8)
        ));
        assert!(matches!(
            super::unknown_entry(&file, false).unwrap(),
            UnknownEntry::File(512)
        ));
    }

    #[test]
    fn mount_identity_separates_filesystems_and_mount_points() {
        let root = MountIdentity {
            device: 1,
            filesystem_id: [2, 3],
            mount_point: b"/volume".to_vec(),
        };
        assert!(same_mount(&root, &root));
        let nested_mount = MountIdentity {
            device: 1,
            filesystem_id: [2, 3],
            mount_point: b"/volume/nested".to_vec(),
        };
        assert!(!same_mount(&root, &nested_mount));
        assert!(!same_mount(
            &root,
            &MountIdentity {
                device: 4,
                filesystem_id: [2, 3],
                mount_point: b"/volume".to_vec(),
            }
        ));
    }
}
