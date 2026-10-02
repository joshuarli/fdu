use crate::{DirectoryContents, DirectoryId, DirectoryItem, DiskItem, ScanReport, StoredDirectory};
use rayon::prelude::*;
use std::cell::RefCell;
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, TryLockError};

// A 512 KiB buffer reads large ext4 directories in fewer getdents64 calls.
const DIRECTORY_BUFFER_BYTES: usize = 512 * 1024;
// A record needs a 19-byte header and at least one NUL byte for its name.
const MIN_DIRECTORY_RECORD_BYTES: usize = 20;
// These limits retain at most 512 KiB of parsed records in each scanning thread's pool.
const MAX_CACHED_DIRECTORY_ENTRY_CAPACITY: usize = 4096;
const MAX_CACHED_DIRECTORY_ENTRY_VECTORS: usize = 8;
// Avoid forcing remote metadata refreshes; remote results may reflect cached state.
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

thread_local! {
    static DIRECTORY_ENTRY_POOL: RefCell<Vec<Vec<DirectoryEntry>>> = RefCell::new(Vec::new());
}

// The Rc marker keeps each buffer lease on the thread whose TLS pool owns its capacity.
struct PooledDirectoryEntries {
    entries: Vec<DirectoryEntry>,
    _worker: PhantomData<Rc<()>>,
}

impl PooledDirectoryEntries {
    fn take() -> Self {
        let entries = DIRECTORY_ENTRY_POOL.with(|pool| pool.borrow_mut().pop().unwrap_or_default());
        Self {
            entries,
            _worker: PhantomData,
        }
    }
}

impl Deref for PooledDirectoryEntries {
    type Target = Vec<DirectoryEntry>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl DerefMut for PooledDirectoryEntries {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

impl Drop for PooledDirectoryEntries {
    fn drop(&mut self) {
        if self.entries.capacity() > MAX_CACHED_DIRECTORY_ENTRY_CAPACITY {
            return;
        }
        self.entries.clear();
        let entries = mem::take(&mut self.entries);
        let _ = DIRECTORY_ENTRY_POOL.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < MAX_CACHED_DIRECTORY_ENTRY_VECTORS {
                pool.push(entries);
            }
        });
    }
}

struct DirectoryEntries {
    names: Vec<u8>,
    entries: PooledDirectoryEntries,
}

struct DirectoryFrame {
    directory: File,
    names: Vec<u8>,
    entries: PooledDirectoryEntries,
    next_entry: usize,
    items: Vec<DirectoryItem>,
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
        // read_directory copies only the bytes before the first NUL and appends one terminator.
        unsafe { CStr::from_bytes_with_nul_unchecked(&names[self.name_offset..end]) }
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
        .map(|entry| {
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
                Ok(item) => DirectoryItem::Scanned(item),
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    DirectoryItem::Skipped
                }
            }
        })
        .collect::<Vec<_>>();

    finish_directory(names, items, arena)
}

fn finish_directory(
    names: Vec<u8>,
    mut items: Vec<DirectoryItem>,
    arena: &Mutex<Vec<DirectoryContents>>,
) -> io::Result<DirectoryContents> {
    items.sort_unstable_by(|left, right| right.disk_size().cmp(&left.disk_size()));
    let disk_size = match items
        .iter()
        .try_fold(0u64, |total, child| total.checked_add(child.disk_size()))
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
fn discard_child_directories(arena: &Mutex<Vec<DirectoryContents>>, items: &[DirectoryItem]) {
    let mut pending = items
        .iter()
        .filter_map(DirectoryItem::children)
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
        pending.extend(directory.items.into_iter().filter_map(|item| item.children()));
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
                        .push(DirectoryItem::Scanned(item));
                }
                Ok(ScannedEntry::Directory {
                    directory,
                    name_offset,
                    ext4_eof_cookie,
                }) => match DirectoryFrame::new(directory, Some(name_offset), ext4_eof_cookie) {
                    Ok(frame) => stack.push(frame),
                    Err(_) => {
                        skipped_entries.fetch_add(1, Ordering::Relaxed);
                        stack
                            .last_mut()
                            .expect("root directory frame remains present")
                            .items
                            .push(DirectoryItem::Skipped);
                    }
                },
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    stack
                        .last_mut()
                        .expect("root directory frame remains present")
                        .items
                        .push(DirectoryItem::Skipped);
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
                            .push(DirectoryItem::Scanned(DiskItem::new(
                                name_offset,
                                stored.disk_size,
                                Some(stored.id),
                            )));
                    }
                    None => return Ok(contents),
                }
            }
            Err(error) => match parent_name_offset {
                Some(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    stack
                        .last_mut()
                        .expect("child directory has a parent frame")
                        .items
                        .push(DirectoryItem::Skipped);
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

    Ok(DiskItem::new(
        entry.name_offset,
        directory.disk_size,
        Some(directory.id),
    ))
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
            if file_device_with_retry(|| child.metadata())? != root_device {
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

fn file_device_with_retry(
    mut metadata: impl FnMut() -> io::Result<std::fs::Metadata>,
) -> io::Result<Device> {
    retry_interrupted(&mut metadata).map(|metadata| Device::from_raw(metadata.dev()))
}

// Directory descriptors stay open during descent; increase the process soft limit only on EMFILE.
fn open_with_nofile_retry(
    mut open: impl FnMut() -> io::Result<File>,
) -> io::Result<File> {
    loop {
        match retry_interrupted(&mut open) {
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

    let mut limit = retry_interrupted(|| {
        let mut limit = mem::MaybeUninit::<libc::rlimit>::uninit();
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { limit.assume_init() })
        }
    })?;
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
    retry_interrupted(|| {
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
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
    Ok(DiskItem::new(entry.name_offset, size, None))
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
    let filesystem = retry_interrupted(|| {
        let mut filesystem = mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(fd, filesystem.as_mut_ptr()) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { filesystem.assume_init() })
        }
    });
    match filesystem {
        Ok(filesystem) => filesystem.f_type as u64 == EXT4_SUPER_MAGIC,
        Err(_) => false,
    }
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
    retry_interrupted(|| {
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
    })
}

// Interrupted syscalls have not completed their operation, so retry before reporting an error.
fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn read_directory(fd: libc::c_int, ext4_eof_cookie: bool) -> io::Result<DirectoryEntries> {
    DIRECTORY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        let mut entries = PooledDirectoryEntries::take();
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

#[cfg(test)]
mod tests {
    use super::{
        file_device_with_retry, retry_interrupted, scan, Device, DirectoryContents, DirectoryItem,
        DiskItem,
    };
    use std::cell::Cell;
    use std::error::Error;
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static TEMP_DIRECTORY_ID: AtomicUsize = AtomicUsize::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> io::Result<Self> {
            loop {
                let id = TEMP_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "fdu-test-{}-{id}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn item<'a>(directory: &'a DirectoryContents, name: &[u8]) -> &'a DiskItem {
        directory
            .items
            .iter()
            .find_map(|directory_item| match directory_item {
                DirectoryItem::Scanned(item)
                    if directory.name_bytes(item.name_offset()) == name =>
                {
                    Some(item)
                }
                DirectoryItem::Scanned(_) | DirectoryItem::Skipped => None,
            })
            .expect("fixture entry is present")
    }

    #[test]
    fn retries_interrupted_syscalls() {
        let attempts = Cell::new(0);
        let result = retry_interrupted(|| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(17)
            }
        });

        assert_eq!(result.unwrap(), 17);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn retries_interrupted_file_metadata_before_checking_device() -> io::Result<()> {
        let file = File::open(".")?;
        let expected = Device::from_raw(file.metadata()?.dev());
        let attempts = Cell::new(0);
        let device = file_device_with_retry(|| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                file.metadata()
            }
        })?;

        assert!(device == expected);
        assert_eq!(attempts.get(), 2);
        Ok(())
    }

    #[test]
    fn reuses_small_entry_vectors_and_drops_oversized_ones() {
        let mut entries = super::PooledDirectoryEntries::take();
        entries.reserve(64);
        let cached_capacity = entries.capacity();
        assert!(cached_capacity <= super::MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);
        drop(entries);

        let entries = super::PooledDirectoryEntries::take();
        assert_eq!(entries.capacity(), cached_capacity);
        drop(entries);

        let mut oversized = super::PooledDirectoryEntries::take();
        oversized.reserve(super::MAX_CACHED_DIRECTORY_ENTRY_CAPACITY + 1);
        assert!(oversized.capacity() > super::MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);
        drop(oversized);

        let replacement = super::PooledDirectoryEntries::take();
        assert!(replacement.capacity() <= super::MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);
        drop(replacement);

        let mut buffers = (0..=super::MAX_CACHED_DIRECTORY_ENTRY_VECTORS)
            .map(|_| super::PooledDirectoryEntries::take())
            .collect::<Vec<_>>();
        for buffer in &mut buffers {
            buffer.reserve(1);
        }
        drop(buffers);
        let cached_count = super::DIRECTORY_ENTRY_POOL.with(|pool| pool.borrow().len());
        assert_eq!(cached_count, super::MAX_CACHED_DIRECTORY_ENTRY_VECTORS);
    }

    #[test]
    fn skipped_directory_items_do_not_contribute_to_totals_or_output() -> io::Result<()> {
        let mut names = b"unreadable\0".to_vec();
        let readable_offset = names.len();
        names.extend_from_slice(b"readable\0");
        let items = vec![
            DirectoryItem::Skipped,
            DirectoryItem::Scanned(DiskItem::new(readable_offset, 7, None)),
        ];
        let directory = super::finish_directory(names, items, &Mutex::new(Vec::new()))?;
        assert_eq!(directory.disk_size, 7);

        let mut output = Vec::new();
        crate::write_tree(
            std::ffi::OsStr::new("root"),
            &directory,
            &[],
            &mut output,
        )?;
        assert_eq!(output, b"7\troot\n  7\treadable\n");
        Ok(())
    }

    #[test]
    fn walk_accounts_for_each_path_without_following_symlinks() -> Result<(), Box<dyn Error>> {
        let temporary = TemporaryDirectory::new()?;
        let root = &temporary.0;
        let payload = root.join("payload.bin");
        let hardlink = root.join("payload-hardlink.bin");
        let sparse = root.join("sparse.bin");
        let invalid_name = OsString::from_vec(b"invalid-\xff-name".to_vec());
        let invalid_path = root.join(&invalid_name);
        let symlink_path = root.join("payload-link");
        let nested = root.join("nested");
        let nested_payload = nested.join("nested.bin");
        let empty = root.join("empty");

        fs::write(&payload, vec![0x5a; 8 * 1024])?;
        fs::hard_link(&payload, &hardlink)?;
        File::create(&sparse)?.set_len(1024 * 1024)?;
        fs::write(&invalid_path, b"raw bytes")?;
        symlink("payload.bin", &symlink_path)?;
        fs::create_dir(&nested)?;
        fs::write(&nested_payload, vec![0x31; 257])?;
        fs::create_dir(&empty)?;

        let allocated_size = |path: &Path| -> io::Result<u64> {
            Ok(fs::symlink_metadata(path)?.blocks() * 512)
        };
        let apparent_size = |path: &Path| -> io::Result<u64> {
            Ok(fs::symlink_metadata(path)?.len())
        };
        let expected_allocated = allocated_size(&payload)? * 2
            + allocated_size(&sparse)?
            + allocated_size(&invalid_path)?
            + allocated_size(&symlink_path)?
            + allocated_size(&nested_payload)?;
        let expected_apparent = apparent_size(&payload)? * 2
            + apparent_size(&sparse)?
            + apparent_size(&invalid_path)?
            + apparent_size(&symlink_path)?
            + apparent_size(&nested_payload)?;

        let allocated = scan(root, false)?;
        assert_eq!(allocated.skipped_entries, 0);
        assert_eq!(allocated.root.disk_size, expected_allocated);
        assert_eq!(
            item(&allocated.root, b"payload.bin").disk_size,
            allocated_size(&payload)?
        );
        assert_eq!(
            item(&allocated.root, b"payload-hardlink.bin").disk_size,
            allocated_size(&hardlink)?
        );
        assert_eq!(
            item(&allocated.root, b"invalid-\xff-name").disk_size,
            allocated_size(&invalid_path)?
        );
        assert!(item(&allocated.root, b"payload-link").children.is_none());
        assert_eq!(
            item(&allocated.root, b"payload-link").disk_size,
            allocated_size(&symlink_path)?
        );
        assert!(item(&allocated.root, b"empty").children.is_some());
        assert_eq!(item(&allocated.root, b"empty").disk_size, 0);
        let nested_item = item(&allocated.root, b"nested");
        assert_eq!(nested_item.disk_size, allocated_size(&nested_payload)?);
        let nested_contents = &allocated.directories[
            nested_item
                .children
                .expect("nested directory has child contents")
                .index()
        ];
        assert_eq!(
            item(nested_contents, b"nested.bin").disk_size,
            allocated_size(&nested_payload)?
        );

        let apparent = scan(root, true)?;
        assert_eq!(apparent.skipped_entries, 0);
        assert_eq!(apparent.root.disk_size, expected_apparent);
        assert_eq!(
            item(&apparent.root, b"sparse.bin").disk_size,
            apparent_size(&sparse)?
        );
        assert_eq!(
            item(&apparent.root, b"payload-link").disk_size,
            apparent_size(&symlink_path)?
        );

        Ok(())
    }

    #[test]
    fn reads_all_long_names_across_directory_buffer_batches() -> Result<(), Box<dyn Error>> {
        const FILE_COUNT: usize = 2050;
        const NAME_LENGTH: usize = 250;

        let temporary = TemporaryDirectory::new()?;
        for index in 0..FILE_COUNT {
            let mut name = format!("entry-{index:04}").into_bytes();
            name.resize(NAME_LENGTH, b'x');
            File::create(temporary.0.join(OsString::from_vec(name)))?;
        }

        let report = scan(&temporary.0, false)?;
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(report.root.disk_size, 0);
        assert_eq!(report.root.items.len(), FILE_COUNT);
        assert!(report.root.items.iter().all(|item| match item {
            DirectoryItem::Scanned(item) => {
                item.children.is_none()
                    && report.root.name_bytes(item.name_offset()).len() == NAME_LENGTH
            }
            DirectoryItem::Skipped => false,
        }));

        Ok(())
    }
}
