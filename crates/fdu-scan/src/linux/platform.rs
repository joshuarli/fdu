//! Linux primitives for the shared indexed scanner in `crate::indexed`: directory
//! streaming, per-entry metadata, mount identity, and worker sizing.
//!
//! Unlike the summary scanner this layer favors simplicity over syscall tuning.
//! `getdents64` yields names and no metadata, so every entry is sized with one
//! `fstatat(AT_SYMLINK_NOFOLLOW)` call by the shared scanner.

use super::{
    directory_record_name, getdents64_call, mount_id_for_fd, open_with_nofile_retry,
    openat_directory, retry_interrupted, MIN_DIRECTORY_RECORD_BYTES,
};
use crate::indexed::{BulkMetadata, IndexedScanLimits};
use fdu_core::{EntryType, FileIdentity};
use std::cell::RefCell;
use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, RawFd};
use std::thread;

const INDEXED_DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
const INDEXED_SCAN_WORKER_LIMIT: usize = 64;
const INDEXED_SCAN_WORKER_HEADROOM: usize = 24;
const INDEXED_TASKS_PER_WORKER: usize = 8;
/// Descriptors left for the process itself (terminal, deletion root, scan root).
const RESERVED_DESCRIPTORS: usize = 128;
/// A queued directory holds one descriptor, and a worker holds the directory it
/// reads, a duplicate of it, and the children it has opened but not yet queued.
const DESCRIPTORS_PER_QUEUED_DIRECTORY: usize = 4;
const MAX_SOFT_DESCRIPTOR_LIMIT: libc::rlim_t = 1 << 20;
/// Offset of `d_reclen` in `struct linux_dirent64`: `d_ino` and `d_off` come first.
const RECORD_LENGTH_OFFSET: usize = 16;
const RECORD_HEADER_BYTES: usize = 19;

pub(crate) type DirectoryBuffer = Vec<u8>;

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
            buffer.resize(INDEXED_DIRECTORY_BUFFER_BYTES, 0);
        }
        use_buffer(&mut buffer)
    })
}

/// Scan workers mostly wait on storage, so a modest amount of headroom over the CPU
/// count keeps independent subtrees moving. Queued directories keep descriptors
/// open, so both numbers are bounded by the descriptor limit.
pub(crate) fn indexed_scan_limits() -> IndexedScanLimits {
    let descriptors = raise_descriptor_limit();
    let workers = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .saturating_add(INDEXED_SCAN_WORKER_HEADROOM)
        .min(INDEXED_SCAN_WORKER_LIMIT);
    let budget = descriptors.saturating_sub(RESERVED_DESCRIPTORS) / DESCRIPTORS_PER_QUEUED_DIRECTORY;
    limits_within_budget(workers, budget)
}

fn limits_within_budget(workers: usize, queue_budget: usize) -> IndexedScanLimits {
    let workers = workers.min(queue_budget).max(1);
    IndexedScanLimits {
        workers,
        queued_directories: workers
            .saturating_mul(INDEXED_TASKS_PER_WORKER)
            .min(queue_budget)
            .max(1),
    }
}

/// Raises the soft descriptor limit toward the hard limit and returns the limit in
/// effect. The limit is process-wide, as in the summary scanner's retry path.
fn raise_descriptor_limit() -> usize {
    let mut limit = mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return RESERVED_DESCRIPTORS;
    }
    let mut limit = unsafe { limit.assume_init() };
    let target = limit.rlim_max.min(MAX_SOFT_DESCRIPTOR_LIMIT);
    if limit.rlim_cur < target {
        let raised = libc::rlimit { rlim_cur: target, rlim_max: limit.rlim_max };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            limit = raised;
        }
    }
    usize::try_from(limit.rlim_cur).unwrap_or(usize::MAX)
}

pub(crate) struct IndexedDirectoryStream<'buffer> {
    directory: File,
    buffer: &'buffer mut DirectoryBuffer,
    offset: usize,
    filled: usize,
}

impl<'buffer> IndexedDirectoryStream<'buffer> {
    pub(crate) fn from_file(directory: File, buffer: &'buffer mut DirectoryBuffer) -> Self {
        Self { directory, buffer, offset: 0, filled: 0 }
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.directory.as_raw_fd()
    }

    /// Returns the next entry name, including `.` and `..`. Linux directory
    /// enumeration carries no usable metadata, so the second element is always `None`.
    pub(crate) fn next_entry_name(
        &mut self,
    ) -> io::Result<Option<(&CStr, Option<BulkMetadata>)>> {
        loop {
            if self.offset >= self.filled && !self.refill()? {
                return Ok(None);
            }
            let start = self.offset;
            let records = &self.buffer[start..self.filled];
            if records.len() < RECORD_HEADER_BYTES {
                return Err(truncated_record());
            }
            let inode = u64::from_ne_bytes(records[..8].try_into().expect("eight header bytes"));
            let length = usize::from(u16::from_ne_bytes([
                records[RECORD_LENGTH_OFFSET],
                records[RECORD_LENGTH_OFFSET + 1],
            ]));
            if length < MIN_DIRECTORY_RECORD_BYTES || length > records.len() {
                return Err(truncated_record());
            }
            self.offset = start + length;
            // Filesystems may leave deleted entries behind with a zero inode.
            if inode == 0 {
                continue;
            }
            let name = directory_record_name(&self.buffer[start..start + length])?;
            return Ok(Some((name, None)));
        }
    }

    fn refill(&mut self) -> io::Result<bool> {
        let bytes_read = retry_interrupted(|| getdents64_call(self.fd(), &mut self.buffer[..]))?;
        if bytes_read > self.buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "getdents64 returned more data than the supplied buffer",
            ));
        }
        self.offset = 0;
        self.filled = bytes_read;
        Ok(bytes_read != 0)
    }
}

fn truncated_record() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "truncated directory record")
}

pub(crate) fn open_child_directory(parent_fd: RawFd, name: &CStr) -> io::Result<File> {
    open_with_nofile_retry(|| openat_directory(parent_fd, name))
}

/// Identifies a mount by device and kernel mount ID. A bind mount of the same
/// filesystem has a different mount ID, and a btrfs subvolume a different device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MountIdentity {
    device: u64,
    mount_id: u64,
}

pub(crate) fn mount_identity(fd: RawFd) -> io::Result<MountIdentity> {
    Ok(MountIdentity {
        device: identity_for_fd(fd)?.device,
        mount_id: mount_id_for_fd(fd)?,
    })
}

pub(crate) fn same_mount_on_fd(
    fd: RawFd,
    root_mount: &MountIdentity,
    device: u64,
) -> io::Result<bool> {
    Ok(device == root_mount.device && mount_id_for_fd(fd)? == root_mount.mount_id)
}

pub(crate) fn file_identity(stat: &libc::stat) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        device: u64::try_from(stat.st_dev).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "device number is outside supported range")
        })?,
        inode: u64::try_from(stat.st_ino).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "inode number is outside supported range")
        })?,
    })
}

pub(crate) fn identity_for_fd(fd: RawFd) -> io::Result<FileIdentity> {
    identity_and_link_count_for_fd(fd).map(|(identity, _)| identity)
}

pub(crate) fn identity_and_link_count_for_fd(fd: RawFd) -> io::Result<(FileIdentity, u64)> {
    let mut metadata = mem::MaybeUninit::<libc::stat>::uninit();
    retry_interrupted(|| {
        if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })?;
    // SAFETY: a successful fstat initializes the structure.
    let metadata = unsafe { metadata.assume_init_ref() };
    let link_count = u64::try_from(metadata.st_nlink)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative link count"))?;
    Ok((file_identity(metadata)?, link_count))
}

pub(crate) fn entry_type_for_mode(mode: libc::mode_t) -> EntryType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => EntryType::Directory,
        libc::S_IFREG => EntryType::RegularFile,
        libc::S_IFLNK => EntryType::Symlink,
        _ => EntryType::Other,
    }
}

pub(crate) fn with_stat_at<T>(
    parent_fd: RawFd,
    name: &CStr,
    inspect: impl FnOnce(&libc::stat) -> io::Result<T>,
) -> io::Result<T> {
    let mut stat = mem::MaybeUninit::<libc::stat>::uninit();
    retry_interrupted(|| {
        let result = unsafe {
            libc::fstatat(parent_fd, name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })?;
    // SAFETY: a successful fstatat initializes the structure.
    inspect(unsafe { stat.assume_init_ref() })
}

pub(crate) fn file_size(stat: &libc::stat, apparent: bool) -> io::Result<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
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

    fn open(path: &Path) -> File {
        File::open(path).unwrap()
    }

    #[test]
    fn stream_lists_every_name_once_across_buffer_refills() {
        let temp = TempDir::new("stream");
        let mut expected = vec![b".".to_vec(), b"..".to_vec()];
        // Long names fill several 64 KiB reads, exercising record boundaries.
        for index in 0..1500 {
            let name = format!("{index:04}-{}", "n".repeat(120));
            fs::write(temp.0.join(&name), b"").unwrap();
            expected.push(name.into_bytes());
        }
        let unusual = b"raw-\xc3\xa9-\n-name".to_vec();
        fs::write(temp.0.join(std::ffi::OsStr::from_bytes(&unusual)), b"").unwrap();
        expected.push(unusual);

        let mut names = Vec::new();
        with_indexed_directory_buffer(|buffer| {
            let mut stream = IndexedDirectoryStream::from_file(open(&temp.0), buffer);
            while let Some((name, metadata)) = stream.next_entry_name().unwrap() {
                assert!(metadata.is_none());
                names.push(name.to_bytes().to_vec());
            }
        });
        names.sort();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn stream_reports_a_truncated_record_instead_of_reading_past_the_buffer() {
        let temp = TempDir::new("truncated");
        with_indexed_directory_buffer(|buffer| {
            let mut stream = IndexedDirectoryStream::from_file(open(&temp.0), buffer);
            stream.filled = 10;
            let error = stream.next_entry_name().err().expect("short header is rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn metadata_matches_the_standard_library() {
        use std::os::unix::fs::MetadataExt;
        let temp = TempDir::new("metadata");
        fs::write(temp.0.join("file"), vec![1u8; 5000]).unwrap();
        std::os::unix::fs::symlink("file", temp.0.join("link")).unwrap();
        let directory = open(&temp.0);

        let name = CString::new("file").unwrap();
        let (identity, kind, apparent, allocated) = with_stat_at(directory.as_raw_fd(), &name, |stat| {
            Ok((
                file_identity(stat)?,
                entry_type_for_mode(stat.st_mode),
                file_size(stat, true)?,
                file_size(stat, false)?,
            ))
        })
        .unwrap();
        let expected = fs::symlink_metadata(temp.0.join("file")).unwrap();
        assert_eq!(identity, FileIdentity { device: expected.dev(), inode: expected.ino() });
        assert_eq!(kind, EntryType::RegularFile);
        assert_eq!(apparent, 5000);
        assert_eq!(allocated, expected.blocks() * 512);

        let link = CString::new("link").unwrap();
        let kind = with_stat_at(directory.as_raw_fd(), &link, |stat| Ok(entry_type_for_mode(stat.st_mode))).unwrap();
        assert_eq!(kind, EntryType::Symlink);
        let (_, links) = identity_and_link_count_for_fd(directory.as_raw_fd()).unwrap();
        assert_eq!(links, fs::metadata(&temp.0).unwrap().nlink());
    }

    #[test]
    fn child_directories_stay_on_the_root_mount_and_symlinks_are_not_followed() {
        let temp = TempDir::new("mount");
        fs::create_dir(temp.0.join("child")).unwrap();
        std::os::unix::fs::symlink("child", temp.0.join("link")).unwrap();
        let root = open(&temp.0);
        let root_mount = mount_identity(root.as_raw_fd()).unwrap();

        let child = open_child_directory(root.as_raw_fd(), &CString::new("child").unwrap()).unwrap();
        let (identity, _) = identity_and_link_count_for_fd(child.as_raw_fd()).unwrap();
        assert!(same_mount_on_fd(child.as_raw_fd(), &root_mount, identity.device).unwrap());
        assert!(!same_mount_on_fd(child.as_raw_fd(), &root_mount, identity.device + 1).unwrap());
        assert!(open_child_directory(root.as_raw_fd(), &CString::new("link").unwrap()).is_err());
    }

    #[test]
    fn limits_never_queue_more_directories_than_the_descriptor_budget_allows() {
        let limits = limits_within_budget(64, 10);
        assert_eq!((limits.workers, limits.queued_directories), (10, 10));
        let limits = limits_within_budget(4, 1000);
        assert_eq!((limits.workers, limits.queued_directories), (4, 32));
        let limits = limits_within_budget(8, 0);
        assert_eq!((limits.workers, limits.queued_directories), (1, 1));
    }
}
