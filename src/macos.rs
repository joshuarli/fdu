use crate::{ScanReport, TopLevelDirectory};
use std::ffi::{CStr, CString, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr::NonNull;

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
            let name = CString::new(name_bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "macOS directory entry name contains a NUL byte",
                )
            })?;
            let kind = match entry.d_type {
                DIRECTORY_ENTRY => EntryKind::Directory,
                UNKNOWN_ENTRY => EntryKind::Unknown,
                _ => EntryKind::Other,
            };
            return Ok(Some(DirectoryEntry { name, kind }));
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
    let mut report = ScanReport {
        directories: Vec::new(),
        skipped_entries: 0,
        mount_boundaries: 0,
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
                });
                continue;
            }
            Err(_) => {
                mark_skipped(&mut report.skipped_entries);
                continue;
            }
        };
        match scan_subtree(
            directory,
            &root_mount,
            apparent,
            &mut report.skipped_entries,
            &mut report.mount_boundaries,
        ) {
            Ok(disk_size) => report.directories.push(TopLevelDirectory {
                name: OsString::from_vec(entry.name.as_bytes().to_vec()),
                disk_size,
            }),
            Err(_) => mark_skipped(&mut report.skipped_entries),
        }
    }

    report
        .directories
        .sort_unstable_by(|left, right| right.disk_size.cmp(&left.disk_size));
    Ok(report)
}

fn scan_subtree(
    root: File,
    root_mount: &MountIdentity,
    apparent: bool,
    skipped_entries: &mut usize,
    mount_boundaries: &mut usize,
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
