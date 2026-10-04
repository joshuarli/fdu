//! Linux primitives for the shared indexed scanner in `crate::indexed`: directory
//! enumeration, mount identity, and worker sizing.
//!
//! `getdents64` yields names and types but no metadata, so the shared scanner sizes
//! each file with one `fstatat`. A directory the kernel reports as a directory is
//! opened without a stat first, and one `statx` on the opened descriptor then yields
//! its identity, link count, and mount.

use super::reports;
use crate::fsutil::{self, retry};
use crate::indexed::{DirectoryFacts, EntryHint, IndexedScanLimits};
use fdu_core::FileIdentity;
use rustix::fs::{self, AtFlags, FileType, RawDir, RawDirEntry, StatxFlags};
use std::ffi::CStr;
use rustix::io::Errno;
use std::cell::RefCell;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::BorrowedFd;
use std::thread;

const INDEXED_DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
const INDEXED_SCAN_WORKER_LIMIT: usize = 64;
const INDEXED_SCAN_WORKER_HEADROOM: usize = 24;
const BASE_WORKERS_PER_CORE: usize = 2;
const INDEXED_TASKS_PER_WORKER: usize = 8;
/// Descriptors left for the process itself (terminal, deletion root, scan root).
const RESERVED_DESCRIPTORS: usize = 128;
/// A queued directory holds one descriptor, and a worker holds the directory it
/// reads and the children it has opened but not yet queued.
const DESCRIPTORS_PER_QUEUED_DIRECTORY: usize = 4;
const MAX_SOFT_DESCRIPTOR_LIMIT: u64 = 1 << 20;

pub(crate) type DirectoryBuffer = Vec<MaybeUninit<u8>>;

thread_local! {
    static INDEXED_DIRECTORY_BUFFER: RefCell<DirectoryBuffer> = const { RefCell::new(Vec::new()) };
}

/// Each scanning thread reads one directory at a time, so it reuses one buffer.
pub(crate) fn with_indexed_directory_buffer<T>(
    use_buffer: impl FnOnce(&mut DirectoryBuffer) -> T,
) -> T {
    INDEXED_DIRECTORY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        if buffer.len() < INDEXED_DIRECTORY_BUFFER_BYTES {
            buffer.resize(INDEXED_DIRECTORY_BUFFER_BYTES, MaybeUninit::uninit());
        }
        use_buffer(&mut buffer)
    })
}

/// On a warm cache a scan is CPU-bound, and a few more threads than cores is the most that helps.
/// On a cold one the threads wait on storage, and many more keep it busy. So the pool is sized for
/// the second case, but only `BASE_WORKERS_PER_CORE` per core start, and the rest are released if
/// the scan is seen to be waiting (see `IndexedScanLimits::base_workers`). Queued directories
/// keep descriptors open, so the sizes are also bounded by the descriptor limit.
pub(crate) fn indexed_scan_limits() -> IndexedScanLimits {
    let descriptors = fsutil::raise_descriptor_limit(MAX_SOFT_DESCRIPTOR_LIMIT);
    let cores = thread::available_parallelism().map(usize::from).unwrap_or(1);
    let workers = cores.saturating_add(INDEXED_SCAN_WORKER_HEADROOM).min(INDEXED_SCAN_WORKER_LIMIT);
    let base = cores.saturating_mul(BASE_WORKERS_PER_CORE);
    let budget = descriptors.saturating_sub(RESERVED_DESCRIPTORS) / DESCRIPTORS_PER_QUEUED_DIRECTORY;
    limits_within_budget(workers, base, budget, INDEXED_TASKS_PER_WORKER)
}

fn limits_within_budget(workers: usize, base: usize, queue_budget: usize, per_worker: usize) -> IndexedScanLimits {
    let workers = workers.min(queue_budget).max(1);
    IndexedScanLimits {
        workers,
        base_workers: base.clamp(1, workers),
        queued_directories: workers
            .saturating_mul(per_worker)
            .min(queue_budget)
            .max(1),
    }
}

/// One directory entry. Linux enumeration carries a name and a type but no usable metadata.
pub(crate) struct StreamEntry<'a>(RawDirEntry<'a>);

impl StreamEntry<'_> {
    pub(crate) fn name(&self) -> &CStr {
        self.0.file_name()
    }

    pub(crate) fn hint(&self) -> EntryHint {
        match self.0.file_type() {
            FileType::Directory => EntryHint::Directory,
            FileType::Unknown => EntryHint::Unknown,
            _ => EntryHint::NotDirectory,
        }
    }

    pub(crate) fn bulk(&self) -> Option<crate::indexed::BulkMetadata> {
        None
    }
}

/// Facts about the scan root's filesystem that apply to every directory scanned below it, since
/// the scan never leaves the root's mount.
#[derive(Clone, Copy)]
pub(crate) struct ScanHints {
    /// Ext4 gives the last entry of a directory a reserved offset cookie, so the read that would
    /// only report the end of the directory can be skipped.
    end_cookie: bool,
}

pub(crate) fn scan_hints(root: BorrowedFd<'_>) -> ScanHints {
    ScanHints { end_cookie: super::is_ext4(root) }
}

pub(crate) struct IndexedDirectoryStream<'a> {
    directory: RawDir<'a, BorrowedFd<'a>>,
    stop_at_end_cookie: bool,
    finished: bool,
}

impl<'a> IndexedDirectoryStream<'a> {
    pub(crate) fn new(directory: BorrowedFd<'a>, buffer: &'a mut DirectoryBuffer, hints: ScanHints) -> Self {
        Self {
            directory: RawDir::new(directory, buffer),
            stop_at_end_cookie: hints.end_cookie,
            finished: false,
        }
    }

    /// Returns the next entry, including `.` and `..`.
    pub(crate) fn next_entry(&mut self) -> io::Result<Option<StreamEntry<'_>>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            match self.directory.next() {
                None => return Ok(None),
                Some(Err(Errno::INTR)) => continue,
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(entry)) => {
                    self.finished = self.stop_at_end_cookie
                        && matches!(entry.next_entry_cookie(), super::EXT4_HTREE_EOF_32BIT | super::EXT4_HTREE_EOF_64BIT);
                    return Ok(Some(StreamEntry(entry)));
                }
            }
        }
    }
}

/// Identifies a mount by device and kernel mount ID. A bind mount of the same
/// filesystem has a different mount ID, and a btrfs subvolume a different device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MountIdentity {
    device: u64,
    mount_id: u64,
}

fn describe(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, u64, u64)> {
    let mask = StatxFlags::INO | StatxFlags::NLINK | StatxFlags::MNT_ID;
    let stat = retry(|| fs::statx(fd, c"", AtFlags::EMPTY_PATH, mask))?;
    if !reports(&stat, mask) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel did not report the directory identity and mount ID",
        ));
    }
    let identity = FileIdentity {
        device: fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
        inode: stat.stx_ino,
    };
    Ok((identity, u64::from(stat.stx_nlink), stat.stx_mnt_id))
}

pub(crate) fn mount_identity(fd: BorrowedFd<'_>) -> io::Result<MountIdentity> {
    let (identity, _, mount_id) = describe(fd)?;
    Ok(MountIdentity { device: identity.device, mount_id })
}

/// Identity, link count, and mount membership of an opened directory, in one `statx`.
pub(crate) fn describe_directory(
    fd: BorrowedFd<'_>,
    root_mount: &MountIdentity,
) -> io::Result<DirectoryFacts> {
    let (identity, link_count, mount_id) = describe(fd)?;
    Ok(DirectoryFacts {
        identity,
        link_count,
        same_mount: identity.device == root_mount.device && mount_id == root_mount.mount_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("fdu-linux-platform-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn open(path: &Path) -> std::os::fd::OwnedFd {
        fsutil::open_directory(path).unwrap()
    }

    #[test]
    fn stream_lists_every_name_once_with_its_type_across_buffer_refills() {
        let temp = TempDir::new("stream");
        let mut expected = vec![(b".".to_vec(), EntryHint::Directory), (b"..".to_vec(), EntryHint::Directory)];
        // Long names fill several 64 KiB reads, exercising record boundaries.
        for index in 0..1500 {
            let name = format!("{index:04}-{}", "n".repeat(120));
            fs::write(temp.0.join(&name), b"").unwrap();
            expected.push((name.into_bytes(), EntryHint::NotDirectory));
        }
        fs::create_dir(temp.0.join("directory")).unwrap();
        expected.push((b"directory".to_vec(), EntryHint::Directory));
        let unusual = b"raw-\xc3\xa9-\n-name".to_vec();
        fs::write(temp.0.join(std::ffi::OsStr::from_bytes(&unusual)), b"").unwrap();
        expected.push((unusual, EntryHint::NotDirectory));

        let mut found = Vec::new();
        with_indexed_directory_buffer(|buffer| {
            let directory = open(&temp.0);
            let mut stream = IndexedDirectoryStream::new(directory.as_fd(), buffer, scan_hints(directory.as_fd()));
            while let Some(entry) = stream.next_entry().unwrap() {
                assert!(entry.bulk().is_none());
                found.push((entry.name().to_bytes().to_vec(), entry.hint()));
            }
        });
        found.sort_by(|left, right| left.0.cmp(&right.0));
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(found, expected);
    }

    #[test]
    fn describing_a_directory_reports_its_identity_links_and_mount() {
        use std::os::unix::fs::MetadataExt;
        let temp = TempDir::new("describe");
        fs::create_dir(temp.0.join("child")).unwrap();
        let root = open(&temp.0);
        let root_mount = mount_identity(root.as_fd()).unwrap();

        let child = fsutil::open_directory_at(root.as_fd(), &CString::new("child").unwrap()).unwrap();
        let facts = describe_directory(child.as_fd(), &root_mount).unwrap();
        let expected = fs::metadata(temp.0.join("child")).unwrap();
        assert_eq!(facts.identity, FileIdentity { device: expected.dev(), inode: expected.ino() });
        assert_eq!(facts.link_count, expected.nlink());
        assert!(facts.same_mount);

        let other = MountIdentity { device: root_mount.device, mount_id: root_mount.mount_id + 1 };
        assert!(!describe_directory(child.as_fd(), &other).unwrap().same_mount);
    }

    #[test]
    fn limits_never_queue_more_directories_than_the_descriptor_budget_allows() {
        let limits = limits_within_budget(64, 64, 10, 8);
        assert_eq!((limits.workers, limits.queued_directories), (10, 10));
        let limits = limits_within_budget(4, 4, 1000, 8);
        assert_eq!((limits.workers, limits.queued_directories), (4, 32));
        let limits = limits_within_budget(8, 8, 0, 8);
        assert_eq!((limits.workers, limits.queued_directories), (1, 1));
    }
}
