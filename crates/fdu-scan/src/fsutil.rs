//! Platform-neutral filesystem helpers shared by the scanners, built on rustix.

use fdu_core::{EntryType, FileIdentity};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
use std::ffi::CStr;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::{Mutex, TryLockError};

pub(crate) const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

static FILE_LIMIT_LOCK: Mutex<()> = Mutex::new(());

/// Interrupted syscalls have not completed their operation, so retry before reporting an error.
pub(crate) fn retry<T>(mut operation: impl FnMut() -> rustix::io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(Errno::INTR) => continue,
            result => return result.map_err(io::Error::from),
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

// `st_dev` is 32 bits on macOS but 64 bits on Linux: the checked conversion is
// required on one platform and a no-op where the field is already `u64`.
#[allow(clippy::useless_conversion)]
pub(crate) fn identity(stat: &Stat) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        device: u64::try_from(stat.st_dev).map_err(|_| invalid("device number is outside supported range"))?,
        inode: stat.st_ino,
    })
}

// `st_nlink` is narrower than `u64` on every supported platform, so the checked
// spelling is a no-op where clippy sees a target it cannot fail on.
#[allow(clippy::unnecessary_fallible_conversions, clippy::useless_conversion)]
pub(crate) fn link_count(stat: &Stat) -> io::Result<u64> {
    u64::try_from(stat.st_nlink).map_err(|_| invalid("negative link count"))
}

pub(crate) fn entry_type(stat: &Stat) -> EntryType {
    match FileType::from_raw_mode(stat.st_mode as _) {
        FileType::Directory => EntryType::Directory,
        FileType::RegularFile => EntryType::RegularFile,
        FileType::Symlink => EntryType::Symlink,
        _ => EntryType::Other,
    }
}

pub(crate) fn file_size(stat: &Stat, apparent: bool) -> io::Result<u64> {
    if apparent {
        u64::try_from(stat.st_size).map_err(|_| invalid("file size is outside the supported byte-count range"))
    } else {
        u64::try_from(stat.st_blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(512))
            .ok_or_else(|| invalid("allocated file size is outside the supported byte-count range"))
    }
}

/// What one `fstatat` of a directory entry reports. Sizes are results so a bad size can mark
/// the entry incomplete without hiding its identity and type.
pub(crate) struct EntryStat {
    pub(crate) identity: FileIdentity,
    pub(crate) link_count: u64,
    pub(crate) entry_type: EntryType,
    pub(crate) apparent_bytes: io::Result<u64>,
    pub(crate) allocated_bytes: io::Result<u64>,
}

pub(crate) fn stat_at(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<Stat> {
    retry(|| fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))
}

pub(crate) fn stat_entry_at(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<EntryStat> {
    let stat = stat_at(parent, name)?;
    Ok(EntryStat {
        identity: identity(&stat)?,
        link_count: link_count(&stat)?,
        entry_type: entry_type(&stat),
        apparent_bytes: file_size(&stat, true),
        allocated_bytes: file_size(&stat, false),
    })
}

pub(crate) fn identity_and_link_count(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, u64)> {
    let stat = retry(|| fs::fstat(fd))?;
    Ok((identity(&stat)?, link_count(&stat)?))
}

pub(crate) fn identity_of(fd: BorrowedFd<'_>) -> io::Result<FileIdentity> {
    identity_and_link_count(fd).map(|(identity, _)| identity)
}

/// Runs `open`, raising the soft descriptor limit and trying again when the process runs
/// out of descriptors.
pub(crate) fn with_descriptor_retry(mut open: impl FnMut() -> io::Result<OwnedFd>) -> io::Result<OwnedFd> {
    loop {
        match open() {
            Err(error) if error.raw_os_error() == Some(Errno::MFILE.raw_os_error()) => {
                raise_soft_nofile_limit()?;
            }
            result => return result,
        }
    }
}

/// Opens a child directory without following a symlink.
pub(crate) fn open_directory_at(
    parent: BorrowedFd<'_>,
    name: impl rustix::path::Arg + Copy,
) -> io::Result<OwnedFd> {
    with_descriptor_retry(|| retry(|| fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())))
}

pub(crate) fn open_directory(path: &std::path::Path) -> io::Result<OwnedFd> {
    retry(|| fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()))
}

fn raise_soft_nofile_limit() -> io::Result<()> {
    let _guard = match FILE_LIMIT_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            drop(FILE_LIMIT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
            return Ok(());
        }
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
    };
    let limit = getrlimit(Resource::Nofile);
    let (Some(current), maximum) = (limit.current, limit.maximum) else {
        return Err(io::Error::other("process file descriptor limit is unlimited yet exhausted"));
    };
    if maximum.is_some_and(|maximum| current >= maximum) {
        return Err(io::Error::other("process reached its hard file descriptor limit"));
    }
    let mut next = current.saturating_add(current.max(256));
    if let Some(maximum) = maximum {
        next = next.min(maximum);
    }
    if next <= current {
        return Err(io::Error::other("process file descriptor limit cannot be increased"));
    }
    setrlimit(Resource::Nofile, Rlimit { current: Some(next), maximum })?;
    Ok(())
}

#[cfg(target_os = "linux")]
/// Raises the soft descriptor limit toward the hard limit, capped at `cap`, and returns the limit
/// in effect. The limit is process-wide.
pub(crate) fn raise_descriptor_limit(cap: u64) -> usize {
    let limit = getrlimit(Resource::Nofile);
    let target = limit.maximum.map_or(cap, |maximum| maximum.min(cap));
    let mut current = limit.current;
    if current.is_none_or(|current| current < target)
        && setrlimit(Resource::Nofile, Rlimit { current: Some(target), maximum: limit.maximum }).is_ok()
    {
        current = Some(target);
    }
    current.map_or(usize::MAX, |current| usize::try_from(current).unwrap_or(usize::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn retries_interrupted_syscalls() {
        let mut attempts = 0;
        let result = retry(|| {
            attempts += 1;
            if attempts == 1 { Err(Errno::INTR) } else { Ok(17) }
        });
        assert_eq!(result.unwrap(), 17);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn size_conversion_rejects_negative_and_overflowing_values() {
        let mut stat: Stat = unsafe { std::mem::zeroed() };
        stat.st_size = -1;
        assert!(file_size(&stat, true).is_err());
        stat.st_size = 0;
        stat.st_blocks = i64::MAX as _;
        assert!(file_size(&stat, false).is_err());
        stat.st_blocks = 2;
        assert_eq!(file_size(&stat, false).unwrap(), 1024);
    }

    #[test]
    fn directory_open_does_not_follow_symlinks() {
        let temp = std::env::temp_dir().join(format!("fdu-fsutil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join("child")).unwrap();
        std::os::unix::fs::symlink("child", temp.join("link")).unwrap();
        let root = open_directory(&temp).unwrap();
        assert!(open_directory_at(root.as_fd(), c"child").is_ok());
        assert!(open_directory_at(root.as_fd(), c"link").is_err());
        let stat = stat_entry_at(root.as_fd(), c"link").unwrap();
        assert_eq!(stat.entry_type, EntryType::Symlink);
        std::fs::remove_dir_all(&temp).unwrap();
    }
}
