use crate::{DirectoryContents, DirectoryId, DiskItem, ScanReport, StoredDirectory};
use rayon::prelude::*;
use std::cell::RefCell;
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, TryLockError};

// A 512 KiB buffer reads large ext4 directories in fewer getdents64 calls.
const DIRECTORY_BUFFER_BYTES: usize = 512 * 1024;
// A record needs a 19-byte header and at least one NUL byte for its name.
const MIN_DIRECTORY_RECORD_BYTES: usize = 20;
const STATX_DONT_SYNC: libc::c_int = 0x4000;
const STATX_TYPE: libc::c_uint = 0x0001;
const STATX_SIZE: libc::c_uint = 0x0200;
const STATX_BLOCKS: libc::c_uint = 0x0400;
const RESOLVE_NO_XDEV: u64 = 0x01;
const EXT4_SUPER_MAGIC: u64 = 0xef53;
const EXT4_HTREE_EOF_32BIT: i64 = 0x7fff_ffff;
const EXT4_HTREE_EOF_64BIT: i64 = i64::MAX;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
// Switch to heap-backed descent before a deep tree can exhaust a Rayon worker stack.
const MAX_RECURSIVE_DIRECTORY_DEPTH: usize = 64;

static FILE_LIMIT_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static DIRECTORY_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; DIRECTORY_BUFFER_BYTES]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DirectoryEntry {
    name_offset: usize,
    name_length: u16,
    kind: EntryKind,
}

struct DirectoryEntries {
    names: Vec<u8>,
    entries: Vec<DirectoryEntry>,
}

struct DirectoryFrame {
    directory: File,
    names: Vec<u8>,
    entries: Vec<DirectoryEntry>,
    next_entry: usize,
    items: Vec<DiskItem>,
    parent_name_offset: Option<usize>,
    ext4_eof_cookie: bool,
}

enum ScannedEntry {
    Item(DiskItem),
    Directory {
        directory: File,
        name_offset: usize,
        ext4_eof_cookie: bool,
    },
}

impl DirectoryEntry {
    fn as_c_str<'a>(&self, names: &'a [u8]) -> &'a CStr {
        let end = self.name_offset + usize::from(self.name_length) + 1;
        CStr::from_bytes_with_nul(&names[self.name_offset..end])
            .expect("parsed directory name is NUL-terminated")
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum EntryKind {
    Directory,
    Other,
    Unknown,
}

const _: [(); 16] = [(); mem::size_of::<DirectoryEntry>()];

#[derive(Clone, Copy, Eq, PartialEq)]
struct Device {
    major: u32,
    minor: u32,
}

// The Linux statx structure has a fixed UAPI layout independent of libc bindings.
#[repr(C)]
struct LinuxStatx {
    mask: u32,
    _block_size: u32,
    _attributes: u64,
    _link_count: u32,
    _uid: u32,
    _gid: u32,
    mode: u16,
    _pad1: u16,
    _inode: u64,
    size: u64,
    blocks: u64,
    _attributes_mask: u64,
    _atime: StatxTimestamp,
    _birth_time: StatxTimestamp,
    _change_time: StatxTimestamp,
    _modify_time: StatxTimestamp,
    _rdev_major: u32,
    _rdev_minor: u32,
    device_major: u32,
    device_minor: u32,
    _mount_id: u64,
    _dio_memory_alignment: u32,
    _dio_offset_alignment: u32,
    _spare: [u64; 12],
}

#[repr(C)]
struct StatxTimestamp {
    _seconds: i64,
    _nanoseconds: u32,
    _pad: i32,
}

#[repr(C)]
// openat2 uses a fixed-width UAPI structure; unused fields stay zero.
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

// These offsets must match the fixed-width structures passed directly to Linux syscalls.
const _: [(); 256] = [(); mem::size_of::<LinuxStatx>()];
const _: [(); 28] = [(); mem::offset_of!(LinuxStatx, mode)];
const _: [(); 40] = [(); mem::offset_of!(LinuxStatx, size)];
const _: [(); 48] = [(); mem::offset_of!(LinuxStatx, blocks)];
const _: [(); 136] = [(); mem::offset_of!(LinuxStatx, device_major)];
const _: [(); 24] = [(); mem::size_of::<OpenHow>()];
const _: [(); 8] = [(); mem::offset_of!(OpenHow, mode)];
const _: [(); 16] = [(); mem::offset_of!(OpenHow, resolve)];

impl LinuxStatx {
    fn zeroed() -> Self {
        unsafe { mem::zeroed() }
    }

    fn is_directory(&self) -> bool {
        self.mask & STATX_TYPE != 0 && self.mode & S_IFMT == S_IFDIR
    }

    fn device(&self) -> Device {
        // Linux always returns the containing device ID, independent of the requested mask.
        Device {
            major: self.device_major,
            minor: self.device_minor,
        }
    }
}

impl Device {
    fn from_raw(device: u64) -> Self {
        let device = device as libc::dev_t;
        Self {
            major: libc::major(device) as u32,
            minor: libc::minor(device) as u32,
        }
    }
}

pub(super) fn scan(path: &Path, apparent: bool) -> io::Result<ScanReport> {
    let root_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("."))
        .to_os_string();
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let root_stat = stat_fd(root.as_raw_fd(), STATX_TYPE)?;
    if !root_stat.is_directory() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "path is not a directory",
        ));
    }

    let ext4_eof_cookie = is_ext4(root.as_raw_fd());
    let skipped_entries = AtomicUsize::new(0);
    let arena = Mutex::new(Vec::new());
    let root = scan_directory_contents(
        root,
        root_stat.device(),
        apparent,
        ext4_eof_cookie,
        0,
        &skipped_entries,
        &arena,
    )?;
    let directories = arena
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    Ok(ScanReport {
        root_name,
        root,
        directories,
        skipped_entries: skipped_entries.load(Ordering::Relaxed),
    })
}

fn scan_directory_contents(
    directory: File,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
    depth: usize,
    skipped_entries: &AtomicUsize,
    arena: &Mutex<Vec<DirectoryContents>>,
) -> io::Result<DirectoryContents> {
    if depth >= MAX_RECURSIVE_DIRECTORY_DEPTH {
        return scan_directory_iterative(
            directory,
            root_device,
            apparent,
            ext4_eof_cookie,
            skipped_entries,
            arena,
        );
    }

    let DirectoryEntries { names, entries } =
        read_directory(directory.as_raw_fd(), ext4_eof_cookie)?;
    let child_depth = depth + 1;
    let items = entries
        .par_iter()
        .copied()
        .filter_map(|entry| {
            match scan_entry(
                &directory,
                &names,
                entry,
                root_device,
                apparent,
                ext4_eof_cookie,
                child_depth,
                skipped_entries,
                arena,
            ) {
                Ok(item) => Some(item),
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
        .collect::<Vec<_>>();

    finish_directory(names, items, arena)
}

fn finish_directory(
    names: Vec<u8>,
    mut items: Vec<DiskItem>,
    arena: &Mutex<Vec<DirectoryContents>>,
) -> io::Result<DirectoryContents> {
    items.sort_unstable_by(|left, right| right.disk_size.cmp(&left.disk_size));
    let disk_size = match items
        .iter()
        .try_fold(0u64, |total, child| total.checked_add(child.disk_size))
    {
        Some(disk_size) => disk_size,
        None => {
            discard_child_directories(arena, &items);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory total exceeds the supported byte-count range",
            ));
        }
    };

    Ok(DirectoryContents {
        names,
        items,
        disk_size,
    })
}

// Remove stored descendants when an overflowing total causes the whole subtree to be skipped.
fn discard_child_directories(arena: &Mutex<Vec<DirectoryContents>>, items: &[DiskItem]) {
    let mut pending = items
        .iter()
        .filter_map(|item| item.children)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return;
    }

    let mut directories = arena
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while let Some(directory_id) = pending.pop() {
        let directory = std::mem::replace(
            &mut directories[directory_id.index()],
            DirectoryContents {
                names: Vec::new(),
                items: Vec::new(),
                disk_size: 0,
            },
        );
        pending.extend(directory.items.into_iter().filter_map(|item| item.children));
    }
}

// Vector growth may move node records, so references between directories use stable indices.
fn store_directory(
    arena: &Mutex<Vec<DirectoryContents>>,
    contents: DirectoryContents,
) -> StoredDirectory {
    let disk_size = contents.disk_size;
    let mut directories = arena
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = directories.len();
    directories.push(contents);
    StoredDirectory {
        id: DirectoryId::from_index(index),
        disk_size,
    }
}

impl DirectoryFrame {
    fn new(
        directory: File,
        parent_name_offset: Option<usize>,
        ext4_eof_cookie: bool,
    ) -> io::Result<Self> {
        let DirectoryEntries { names, entries } =
            read_directory(directory.as_raw_fd(), ext4_eof_cookie)?;
        Ok(Self {
            directory,
            names,
            entries,
            next_entry: 0,
            items: Vec::new(),
            parent_name_offset,
            ext4_eof_cookie,
        })
    }
}

fn scan_directory_iterative(
    directory: File,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
    skipped_entries: &AtomicUsize,
    arena: &Mutex<Vec<DirectoryContents>>,
) -> io::Result<DirectoryContents> {
    let root = DirectoryFrame::new(directory, None, ext4_eof_cookie)?;
    let mut stack = vec![root];

    loop {
        let entry = {
            let frame = stack.last_mut().expect("root directory frame remains present");
            match frame.entries.get(frame.next_entry) {
                Some(entry) => {
                    frame.next_entry += 1;
                    Some(*entry)
                }
                None => None,
            }
        };

        if let Some(entry) = entry {
            let scanned = {
                let frame = stack.last().expect("root directory frame remains present");
                scan_entry_iterative(
                    &frame.directory,
                    &frame.names,
                    entry,
                    root_device,
                    apparent,
                    frame.ext4_eof_cookie,
                )
            };

            match scanned {
                Ok(ScannedEntry::Item(item)) => {
                    stack
                        .last_mut()
                        .expect("root directory frame remains present")
                        .items
                        .push(item);
                }
                Ok(ScannedEntry::Directory {
                    directory,
                    name_offset,
                    ext4_eof_cookie,
                }) => match DirectoryFrame::new(directory, Some(name_offset), ext4_eof_cookie) {
                    Ok(frame) => stack.push(frame),
                    Err(_) => {
                        skipped_entries.fetch_add(1, Ordering::Relaxed);
                    }
                },
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                }
            }
            continue;
        }

        let frame = stack.pop().expect("root directory frame remains present");
        let parent_name_offset = frame.parent_name_offset;
        match finish_directory(frame.names, frame.items, arena) {
            Ok(contents) => {
                match parent_name_offset {
                    Some(name_offset) => {
                        let stored = store_directory(arena, contents);
                        stack
                            .last_mut()
                            .expect("child directory has a parent frame")
                            .items
                            .push(DiskItem {
                                name_offset,
                                disk_size: stored.disk_size,
                                children: Some(stored.id),
                            });
                    }
                    None => return Ok(contents),
                }
            }
            Err(error) => match parent_name_offset {
                Some(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                }
                None => return Err(error),
            },
        }
    }
}

fn scan_entry(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
    child_depth: usize,
    skipped_entries: &AtomicUsize,
    arena: &Mutex<Vec<DirectoryContents>>,
) -> io::Result<DiskItem> {
    match entry.kind {
        EntryKind::Directory => {
            scan_child_directory(
                parent,
                names,
                entry,
                root_device,
                apparent,
                ext4_eof_cookie,
                child_depth,
                skipped_entries,
                arena,
            )
        }
        EntryKind::Other => {
            let stat = stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                entry_size_mask(apparent),
            )?;
            file_item(entry, stat, apparent)
        }
        EntryKind::Unknown => {
            let stat = stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                STATX_TYPE | entry_size_mask(apparent),
            )?;
            if stat.mask & STATX_TYPE == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem did not report an unknown entry type",
                ));
            }
            if stat.is_directory() {
                scan_child_directory(
                    parent,
                    names,
                    entry,
                    root_device,
                    apparent,
                    ext4_eof_cookie,
                    child_depth,
                    skipped_entries,
                    arena,
                )
            } else {
                file_item(entry, stat, apparent)
            }
        }
    }
}

fn scan_entry_iterative(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    match entry.kind {
        EntryKind::Directory => {
            open_child_entry(parent, names, entry, root_device, ext4_eof_cookie)
        }
        EntryKind::Other => {
            let stat = stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                entry_size_mask(apparent),
            )?;
            file_item(entry, stat, apparent).map(ScannedEntry::Item)
        }
        EntryKind::Unknown => {
            let stat = stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                STATX_TYPE | entry_size_mask(apparent),
            )?;
            if stat.mask & STATX_TYPE == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem did not report an unknown entry type",
                ));
            }
            if stat.is_directory() {
                open_child_entry(parent, names, entry, root_device, ext4_eof_cookie)
            } else {
                file_item(entry, stat, apparent).map(ScannedEntry::Item)
            }
        }
    }
}

fn entry_size_mask(apparent: bool) -> libc::c_uint {
    if apparent {
        STATX_SIZE
    } else {
        STATX_BLOCKS
    }
}

fn scan_child_directory(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
    child_depth: usize,
    skipped_entries: &AtomicUsize,
    arena: &Mutex<Vec<DirectoryContents>>,
) -> io::Result<DiskItem> {
    let (child, child_ext4_eof_cookie) = open_child_directory(
        parent,
        entry.as_c_str(names),
        root_device,
        ext4_eof_cookie,
    )?;
    let contents = scan_directory_contents(
        child,
        root_device,
        apparent,
        child_ext4_eof_cookie,
        child_depth,
        skipped_entries,
        arena,
    )?;
    let directory = store_directory(arena, contents);

    Ok(DiskItem {
        name_offset: entry.name_offset,
        disk_size: directory.disk_size,
        children: Some(directory.id),
    })
}

fn open_child_entry(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    root_device: Device,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    let (child, child_ext4_eof_cookie) = open_child_directory(
        parent,
        entry.as_c_str(names),
        root_device,
        ext4_eof_cookie,
    )?;
    Ok(ScannedEntry::Directory {
        directory: child,
        name_offset: entry.name_offset,
        ext4_eof_cookie: child_ext4_eof_cookie,
    })
}

// A no-cross-mount open inherits the parent's device; the fallback checks st_dev explicitly.
fn open_child_directory(
    parent: &File,
    name: &CStr,
    root_device: Device,
    ext4_eof_cookie: bool,
) -> io::Result<(File, bool)> {
    match open_with_nofile_retry(|| openat2_directory(parent.as_raw_fd(), name)) {
        Ok(child) => Ok((child, ext4_eof_cookie)),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EXDEV) | Some(libc::ENOSYS) | Some(libc::EPERM)
            ) =>
        {
            // The fallback checks st_dev so same-device bind mounts keep the existing behavior.
            let child = open_with_nofile_retry(|| openat_directory(parent.as_raw_fd(), name))?;
            if Device::from_raw(child.metadata()?.dev()) != root_device {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "filesystem boundary crossed",
                ));
            }
            Ok((child, false))
        }
        Err(error) => Err(error),
    }
}

// Directory descriptors stay open during descent; increase the process soft limit only on EMFILE.
fn open_with_nofile_retry(
    mut open: impl FnMut() -> io::Result<File>,
) -> io::Result<File> {
    loop {
        match open() {
            Err(error) if error.raw_os_error() == Some(libc::EMFILE) => {
                raise_soft_nofile_limit()?;
            }
            result => return result,
        }
    }
}

fn raise_soft_nofile_limit() -> io::Result<()> {
    let _guard = match FILE_LIMIT_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            drop(
                FILE_LIMIT_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            return Ok(());
        }
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
    };

    let mut limit = mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut limit = unsafe { limit.assume_init() };
    if limit.rlim_cur >= limit.rlim_max {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "process reached its hard file descriptor limit",
        ));
    }

    let increment = limit.rlim_cur.max(256);
    let next_soft = limit.rlim_cur.saturating_add(increment).min(limit.rlim_max);
    if next_soft <= limit.rlim_cur {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "process file descriptor limit cannot be increased",
        ));
    }
    limit.rlim_cur = next_soft;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn openat2_directory(parent_fd: libc::c_int, name: &CStr) -> io::Result<File> {
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_NO_XDEV,
    };
    let child_fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent_fd,
            name.as_ptr(),
            &how as *const OpenHow,
            mem::size_of::<OpenHow>(),
        )
    };
    if child_fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(child_fd as libc::c_int) })
    }
}

fn openat_directory(parent_fd: libc::c_int, name: &CStr) -> io::Result<File> {
    let child_fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if child_fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(child_fd) })
    }
}

fn file_item(entry: DirectoryEntry, stat: LinuxStatx, apparent: bool) -> io::Result<DiskItem> {
    let size_mask = entry_size_mask(apparent);
    if stat.mask & size_mask != size_mask {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem did not report the requested file size",
        ));
    }
    let size = if apparent {
        stat.size
    } else {
        stat.blocks.checked_mul(512).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "file total exceeds the supported byte-count range",
            )
        })?
    };
    Ok(DiskItem {
        name_offset: entry.name_offset,
        disk_size: size,
        children: None,
    })
}

fn stat_at(parent_fd: libc::c_int, name: &CStr, mask: libc::c_uint) -> io::Result<LinuxStatx> {
    statx(
        parent_fd,
        name.as_ptr(),
        libc::AT_SYMLINK_NOFOLLOW | STATX_DONT_SYNC,
        mask,
    )
}

fn is_ext4(fd: libc::c_int) -> bool {
    let mut filesystem = mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(fd, filesystem.as_mut_ptr()) } < 0 {
        return false;
    }
    unsafe { filesystem.assume_init().f_type as u64 == EXT4_SUPER_MAGIC }
}

fn stat_fd(fd: libc::c_int, mask: libc::c_uint) -> io::Result<LinuxStatx> {
    // Query the opened root directly without resolving a second pathname.
    statx(
        fd,
        b"\0".as_ptr().cast(),
        libc::AT_EMPTY_PATH | STATX_DONT_SYNC,
        mask,
    )
}

fn statx(
    directory_fd: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mask: libc::c_uint,
) -> io::Result<LinuxStatx> {
    let mut stat = LinuxStatx::zeroed();
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            directory_fd,
            path,
            flags,
            mask,
            &mut stat as *mut LinuxStatx,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(stat)
    }
}

fn read_directory(fd: libc::c_int, ext4_eof_cookie: bool) -> io::Result<DirectoryEntries> {
    DIRECTORY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        let mut entries = Vec::new();
        let mut names = Vec::new();
        let mut first_batch = true;

        loop {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_getdents64,
                    fd,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 {
                break;
            }

            let bytes_read = result as usize;
            if bytes_read > buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "getdents64 returned more data than the supplied buffer",
                ));
            }
            let mut estimated_entries = bytes_read / MIN_DIRECTORY_RECORD_BYTES;
            if first_batch {
                // The first batch includes the two dot entries, which are not retained.
                estimated_entries = estimated_entries.saturating_sub(2);
                first_batch = false;
            }
            if estimated_entries > 0 {
                entries.reserve(estimated_entries);
                // Leave room for common short names while allowing longer names to grow.
                names.reserve(bytes_read / 4);
            }
            let mut offset = 0;
            let mut final_offset = 0;
            while offset < bytes_read {
                if bytes_read - offset < 19 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid getdents64 record header",
                    ));
                }

                let record_length =
                    u16::from_ne_bytes([buffer[offset + 16], buffer[offset + 17]]) as usize;
                if record_length < 20 || record_length > bytes_read - offset {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid getdents64 record length",
                    ));
                }
                final_offset = i64::from_ne_bytes(
                    buffer[offset + 8..offset + 16]
                        .try_into()
                        .expect("fixed-width directory offset"),
                );

                let kind = match buffer[offset + 18] {
                    libc::DT_DIR => EntryKind::Directory,
                    libc::DT_UNKNOWN => EntryKind::Unknown,
                    _ => EntryKind::Other,
                };
                let name_bytes = &buffer[offset + 19..offset + record_length];
                let name_length = name_bytes.iter().position(|byte| *byte == 0).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "unterminated directory entry")
                })?;
                let name_bytes = &name_bytes[..name_length];
                if name_bytes.is_empty() || name_bytes.contains(&b'/') {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid directory entry name",
                    ));
                }

                if name_bytes != b"." && name_bytes != b".." {
                    let length = u16::try_from(name_bytes.len()).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "directory entry name exceeds the supported length",
                        )
                    })?;
                    let start = names.len();
                    names.extend_from_slice(name_bytes);
                    names.push(0);
                    entries.push(DirectoryEntry {
                        name_offset: start,
                        name_length: length,
                        kind,
                    });
                }
                offset += record_length;
            }
            // Ext4 HTree reserves these d_off values for an exhausted directory.
            if ext4_eof_cookie
                && matches!(final_offset, EXT4_HTREE_EOF_32BIT | EXT4_HTREE_EOF_64BIT)
            {
                break;
            }
        }

        Ok(DirectoryEntries { names, entries })
    })
}
