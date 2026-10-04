use crate::fsutil::{self, retry};
use crate::indexed::{BulkMetadata, DirectoryFacts, EntryHint, IndexedScanLimits};
use crate::{ScanReport, TopLevelDirectory};
use fdu_core::{EntryType, ExclusionReason, FileIdentity};
use rustix::fs::{self, Dir, DirEntry, FileType};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::thread;

mod attributes;

const BULK_RECORD_BUFFER_BYTES: usize = 64 * 1024;
const INDEXED_SCAN_WORKER_LIMIT: usize = 128;
const INDEXED_SCAN_WORKER_HEADROOM: usize = 118;
const INDEXED_TASKS_PER_WORKER: usize = 8;

pub(super) type DirectoryBuffer = attributes::AlignedBuffer<BULK_RECORD_BUFFER_BYTES>;

// Directory reads can block in the filesystem, so bounded worker headroom keeps other
// independent subtrees moving while a worker waits for a bulk read.
pub(super) fn indexed_scan_limits() -> IndexedScanLimits {
    let workers = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .saturating_add(INDEXED_SCAN_WORKER_HEADROOM)
        .min(INDEXED_SCAN_WORKER_LIMIT);
    IndexedScanLimits {
        workers,
        base_workers: workers,
        queued_directories: workers.saturating_mul(INDEXED_TASKS_PER_WORKER),
    }
}

// Each scanning thread handles one directory at a time, so it can reuse its aligned bulk buffer.
thread_local! {
    static INDEXED_DIRECTORY_BUFFER: RefCell<Box<DirectoryBuffer>> =
        RefCell::new(Box::new(attributes::AlignedBuffer::new()));
}

pub(super) fn with_indexed_directory_buffer<T>(
    use_buffer: impl FnOnce(&mut DirectoryBuffer) -> T,
) -> T {
    INDEXED_DIRECTORY_BUFFER.with(|buffer| use_buffer(buffer.borrow_mut().as_mut()))
}

/// Facts about the scan root's filesystem that apply to every directory below it. macOS has none
/// that the scanner uses.
#[derive(Clone, Copy)]
pub(super) struct ScanHints;

pub(super) fn scan_hints(_root: BorrowedFd<'_>) -> ScanHints {
    ScanHints
}

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

fn entry_kind(file_type: FileType) -> EntryKind {
    match file_type {
        FileType::Directory => EntryKind::Directory,
        FileType::Unknown => EntryKind::Unknown,
        _ => EntryKind::Other,
    }
}

struct DirectoryPath {
    parent: Option<usize>,
    name: Option<CString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MountIdentity {
    device: u64,
    filesystem_id: [i32; 2],
    mount_point: Vec<u8>,
}

enum OpenedDirectory {
    InsideRoot(OwnedFd),
    MountBoundary,
}

/// Lists a directory, without `.` and `..`.
fn read_entries(directory: &OwnedFd) -> io::Result<Vec<DirectoryEntry>> {
    let mut entries = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            entries.push(DirectoryEntry { name: name.to_owned(), kind: entry_kind(entry.file_type()) });
        }
    }
    Ok(entries)
}

/// One directory entry. Bulk enumeration carries its metadata; the fallback carries a name and a
/// type.
pub(super) enum StreamEntry<'a> {
    Bulk { name: &'a CStr, metadata: Option<BulkMetadata> },
    Listed(DirEntry),
}

impl StreamEntry<'_> {
    pub(super) fn name(&self) -> &CStr {
        match self {
            Self::Bulk { name, .. } => name,
            Self::Listed(entry) => entry.file_name(),
        }
    }

    pub(super) fn hint(&self) -> EntryHint {
        match self {
            Self::Bulk { metadata: Some(metadata), .. } if metadata.entry_type == EntryType::Directory => {
                EntryHint::Directory
            }
            Self::Bulk { metadata: Some(_), .. } => EntryHint::NotDirectory,
            Self::Bulk { metadata: None, .. } => EntryHint::Unknown,
            Self::Listed(entry) => match entry.file_type() {
                FileType::Directory => EntryHint::Directory,
                FileType::Unknown => EntryHint::Unknown,
                _ => EntryHint::NotDirectory,
            },
        }
    }

    pub(super) fn bulk(&self) -> Option<BulkMetadata> {
        match self {
            Self::Bulk { metadata, .. } => *metadata,
            Self::Listed(_) => None,
        }
    }
}

pub(super) struct IndexedDirectoryStream<'buffer> {
    directory: BorrowedFd<'buffer>,
    buffer: &'buffer mut DirectoryBuffer,
    offset: usize,
    remaining: usize,
    exhausted: bool,
    returned_any: bool,
    fallback: Option<Dir>,
}

impl<'buffer> IndexedDirectoryStream<'buffer> {
    pub(super) fn new(
        directory: BorrowedFd<'buffer>,
        buffer: &'buffer mut DirectoryBuffer,
        _hints: ScanHints,
    ) -> Self {
        Self {
            directory,
            buffer,
            offset: 0,
            remaining: 0,
            exhausted: false,
            returned_any: false,
            fallback: None,
        }
    }

    pub(super) fn next_entry(&mut self) -> io::Result<Option<StreamEntry<'_>>> {
        loop {
            if let Some(fallback) = &mut self.fallback {
                return match fallback.read() {
                    Some(Ok(entry)) => Ok(Some(StreamEntry::Listed(entry))),
                    Some(Err(error)) => Err(error.into()),
                    None => Ok(None),
                };
            }
            if self.exhausted {
                return Ok(None);
            }
            if self.remaining == 0 && !self.refill()? {
                return Ok(None);
            }
            if self.fallback.is_some() {
                continue;
            }

            let start = self.offset;
            let length = attributes::read_record_length(&self.buffer.as_bytes()[start..])?;
            let end = start
                .checked_add(length)
                .filter(|end| length >= 24 && *end <= self.buffer.as_bytes().len())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "macOS directory record exceeds its buffer",
                    )
                })?;
            self.offset = end;
            self.remaining -= 1;
            self.returned_any = true;
            let entry = attributes::parse_record(&self.buffer.as_bytes()[start..end])?;
            return Ok(Some(StreamEntry::Bulk { name: entry.name, metadata: entry.metadata }));
        }
    }

    fn refill(&mut self) -> io::Result<bool> {
        loop {
            let mut requested = attributes::requested_attributes();
            // SAFETY: the descriptor stays open for the lifetime of the stream, the attrlist is
            // initialized, and the aligned buffer is writable for its full size.
            let count = unsafe {
                libc::getattrlistbulk(
                    self.directory.as_raw_fd(),
                    (&mut requested as *mut libc::attrlist).cast(),
                    self.buffer.as_mut_bytes().as_mut_ptr().cast(),
                    self.buffer.as_bytes().len(),
                    u64::from(libc::FSOPT_PACK_INVAL_ATTRS),
                )
            };
            if count > 0 {
                self.offset = 0;
                self.remaining = usize::try_from(count).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid macOS directory record count")
                })?;
                return Ok(true);
            }
            if count == 0 {
                self.exhausted = true;
                return Ok(false);
            }

            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let fallback_error = matches!(
                error.raw_os_error(),
                Some(libc::EACCES | libc::EPERM | libc::ENOTSUP | libc::EOPNOTSUPP | libc::ENOSYS)
            ) || error.kind() == io::ErrorKind::Unsupported;
            if !self.returned_any && fallback_error {
                // Nothing was returned, so the plain listing starts from the beginning.
                self.fallback = Some(Dir::read_from(self.directory)?);
                return Ok(true);
            }
            return Err(error);
        }
    }
}

pub(super) fn scan(path: &Path, apparent: bool) -> io::Result<ScanReport> {
    let root = fsutil::open_directory(path)?;
    let entries = read_entries(&root)?;
    let root_fd = root.as_fd();
    let root_mount = mount_identity(root_fd)?;
    let mut seen_directories = HashSet::new();
    seen_directories.insert(fsutil::identity_of(root_fd)?);
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
            EntryKind::Unknown => match fsutil::stat_at(root_fd, &entry.name) {
                Ok(stat) => fsutil::entry_type(&stat) == EntryType::Directory,
                Err(_) => {
                    mark_skipped(&mut report.skipped_entries);
                    continue;
                }
            },
        };
        if !is_directory {
            continue;
        }

        let directory = match fsutil::open_directory_at(root_fd, entry.name.as_c_str()) {
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
        let identity = match fsutil::identity_of(directory.as_fd()) {
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

fn scan_subtree(
    root: OwnedFd,
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
            OpenedDirectory::InsideRoot(root.try_clone()?)
        } else {
            match open_directory_path(root.as_fd(), root_mount, &paths, path_index) {
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
            let identity = match fsutil::identity_of(directory.as_fd()) {
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

        let entries = match read_entries(&directory) {
            Ok(entries) => entries,
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
                    match fsutil::stat_at(directory.as_fd(), &entry.name)
                        .and_then(|stat| fsutil::file_size(&stat, apparent))
                    {
                        Ok(size) => add_size(&mut disk_size, size, skipped_entries),
                        Err(_) => mark_skipped(skipped_entries),
                    }
                    continue;
                }
                EntryKind::Unknown => {
                    match fsutil::stat_at(directory.as_fd(), &entry.name) {
                        Ok(stat) if fsutil::entry_type(&stat) == EntryType::Directory => true,
                        Ok(stat) => {
                            match fsutil::file_size(&stat, apparent) {
                                Ok(size) => add_size(&mut disk_size, size, skipped_entries),
                                Err(_) => mark_skipped(skipped_entries),
                            }
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

fn open_directory_path(
    root: BorrowedFd<'_>,
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

    let mut current: Option<OwnedFd> = None;
    for component in components.into_iter().rev() {
        let parent = current.as_ref().map_or(root, |directory| directory.as_fd());
        let name = paths[component]
            .name
            .as_ref()
            .expect("non-root directory path has a name");
        let directory = fsutil::open_directory_at(parent, name.as_c_str())?;
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

fn keep_directory_on_root_mount(
    directory: OwnedFd,
    root_mount: &MountIdentity,
) -> io::Result<OpenedDirectory> {
    let device = fsutil::identity_of(directory.as_fd())?.device;
    if same_mount_on_fd(directory.as_fd(), root_mount, device)? {
        Ok(OpenedDirectory::InsideRoot(directory))
    } else {
        Ok(OpenedDirectory::MountBoundary)
    }
}

#[cfg(test)]
fn same_mount(left: &MountIdentity, right: &MountIdentity) -> bool {
    left == right
}

pub(super) fn mount_identity(fd: BorrowedFd<'_>) -> io::Result<MountIdentity> {
    let device = fsutil::identity_of(fd)?.device;
    let filesystem = retry(|| fs::fstatfs(fd))?;
    let mount_point = filesystem
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();

    Ok(MountIdentity {
        device,
        filesystem_id: fsid_words(&filesystem),
        mount_point,
    })
}

fn same_mount_on_fd(
    fd: BorrowedFd<'_>,
    root_mount: &MountIdentity,
    device: u64,
) -> io::Result<bool> {
    let filesystem = retry(|| fs::fstatfs(fd))?;
    let mount_point = filesystem
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8);
    Ok(device == root_mount.device
        && fsid_words(&filesystem) == root_mount.filesystem_id
        && mount_point.eq(root_mount.mount_point.iter().copied()))
}

/// Identity, link count, and mount membership of an opened directory.
pub(super) fn describe_directory(
    fd: BorrowedFd<'_>,
    root_mount: &MountIdentity,
) -> io::Result<DirectoryFacts> {
    let (identity, link_count) = fsutil::identity_and_link_count(fd)?;
    Ok(DirectoryFacts {
        identity,
        link_count,
        same_mount: same_mount_on_fd(fd, root_mount, identity.device)?,
    })
}

fn fsid_words(filesystem: &rustix::fs::StatFs) -> [i32; 2] {
    // Apple's fsid_t is exactly two i32 words; libc keeps those fields private.
    const _: () = assert!(std::mem::size_of::<[i32; 2]>() == 8);
    // SAFETY: fsid_t is two i32 words, so reading eight bytes from it stays in bounds.
    unsafe { std::mem::transmute_copy::<_, [i32; 2]>(&filesystem.f_fsid) }
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

#[cfg(test)]
mod tests {
    use super::{same_mount, MountIdentity};

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
