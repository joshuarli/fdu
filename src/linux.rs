use crate::{ScanReport, TopLevelDirectory};
use rayon::prelude::*;
use std::cell::RefCell;
use std::ffi::{CStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
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
    names: DirectoryNames,
    entries: PooledDirectoryEntries,
}

struct DirectoryFrame {
    directory: File,
    names: DirectoryNames,
    entries: PooledDirectoryEntries,
    next_entry: usize,
    disk_size: u64,
    overflowed: bool,
    ext4_eof_cookie: bool,
}

enum ScannedEntry {
    Item(u64),
    Directory {
        directory: File,
        ext4_eof_cookie: bool,
    },
}

enum UnknownEntry {
    Directory,
    Item(u64),
}

// Keep short directory name lists inline so small directories avoid a separate name allocation.
enum DirectoryNames {
    Inline([u8; 16]),
    Heap(Vec<u8>),
}

impl DirectoryNames {
    fn new() -> Self {
        Self::Inline([0; 16])
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline(bytes) => &bytes[..inline_name_bytes_len(bytes)],
            Self::Heap(names) => names,
        }
    }

    fn reserve(&mut self, additional: usize) {
        if let Self::Heap(names) = self {
            names.reserve(additional);
        }
    }

    fn append_name(&mut self, name: &[u8], reserve_hint: usize) -> usize {
        let additional = name
            .len()
            .checked_add(1)
            .expect("directory name length is representable");
        if let Self::Inline(bytes) = self {
            let current_len = inline_name_bytes_len(bytes);
            if current_len + additional <= bytes.len() {
                bytes[current_len..current_len + name.len()].copy_from_slice(name);
                bytes[current_len + name.len()] = 0;
                return current_len;
            }

            let inline = *bytes;
            let capacity = current_len
                .saturating_add(reserve_hint)
                .max(current_len.saturating_add(additional));
            let mut names = Vec::with_capacity(capacity);
            names.extend_from_slice(&inline[..current_len]);
            names.extend_from_slice(name);
            names.push(0);
            *self = Self::Heap(names);
            return current_len;
        }

        let Self::Heap(names) = self else {
            unreachable!("inline names transition to heap storage before overflow");
        };
        let start = names.len();
        names.extend_from_slice(name);
        names.push(0);
        start
    }
}

#[cfg(target_pointer_width = "64")]
const _: [(); 24] = [(); mem::size_of::<DirectoryNames>()];

// Stored names have no embedded NUL and each has exactly one trailing terminator.
fn inline_name_bytes_len(bytes: &[u8; 16]) -> usize {
    bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last_name_byte| (last_name_byte + 2).min(bytes.len()))
}

impl DirectoryEntry {
    fn as_c_str<'a>(&self, names: &'a [u8]) -> &'a CStr {
        let end = self.name_offset + usize::from(self.name_length) + 1;
        // read_directory copies only bytes before the first NUL, then adds one terminator.
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
    _device_major: u32,
    _device_minor: u32,
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
const _: [(); 136] = [(); mem::offset_of!(LinuxStatx, _device_major)];
const _: [(); 24] = [(); mem::size_of::<OpenHow>()];
const _: [(); 8] = [(); mem::offset_of!(OpenHow, mode)];
const _: [(); 16] = [(); mem::offset_of!(OpenHow, resolve)];

impl LinuxStatx {
    fn is_directory(&self) -> bool {
        self.mask & STATX_TYPE != 0 && self.mode & S_IFMT == S_IFDIR
    }
}

pub(super) fn scan(path: &Path, apparent: bool) -> io::Result<ScanReport> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let ext4_eof_cookie = is_ext4(root.as_raw_fd());
    let DirectoryEntries { names, entries } =
        read_directory(root.as_raw_fd(), ext4_eof_cookie, true)?;
    let name_bytes = names.as_slice();
    let skipped_entries = AtomicUsize::new(0);

    let mut directories = entries
        .par_iter()
        .copied()
        .filter_map(|entry| {
            match scan_top_level_entry(
                &root,
                name_bytes,
                entry,
                apparent,
                ext4_eof_cookie,
                &skipped_entries,
            ) {
                Ok(directory) => directory,
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    directories.sort_unstable_by(|left, right| right.disk_size.cmp(&left.disk_size));

    Ok(ScanReport {
        directories,
        skipped_entries: skipped_entries.load(Ordering::Relaxed),
    })
}

fn scan_top_level_entry(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    apparent: bool,
    ext4_eof_cookie: bool,
    skipped_entries: &AtomicUsize,
) -> io::Result<Option<TopLevelDirectory>> {
    if matches!(entry.kind, EntryKind::Other) {
        return Ok(None);
    }

    if matches!(entry.kind, EntryKind::Unknown) {
        let is_directory = with_stat_at(
            parent.as_raw_fd(),
            entry.as_c_str(names),
            STATX_TYPE,
            |stat| {
                if stat.mask & STATX_TYPE == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "filesystem did not report an unknown entry type",
                    ));
                }
                Ok(stat.is_directory())
            },
        )?;
        if !is_directory {
            return Ok(None);
        }
    }

    let disk_size = scan_child_directory(
        parent,
        entry.as_c_str(names),
        apparent,
        ext4_eof_cookie,
        1,
        skipped_entries,
    )?;
    Ok(Some(TopLevelDirectory {
        name: OsString::from_vec(entry.as_c_str(names).to_bytes().to_vec()),
        disk_size,
    }))
}

fn scan_directory_contents(
    directory: File,
    apparent: bool,
    ext4_eof_cookie: bool,
    depth: usize,
    skipped_entries: &AtomicUsize,
) -> io::Result<u64> {
    if depth >= MAX_RECURSIVE_DIRECTORY_DEPTH {
        return scan_directory_iterative(
            directory,
            apparent,
            ext4_eof_cookie,
            skipped_entries,
        );
    }

    let DirectoryEntries { names, entries } =
        read_directory(directory.as_raw_fd(), ext4_eof_cookie, false)?;
    let name_bytes = names.as_slice();
    entries
        .par_iter()
        .copied()
        .map(|entry| {
            match scan_entry_size(
                &directory,
                name_bytes,
                entry,
                apparent,
                ext4_eof_cookie,
                depth + 1,
                skipped_entries,
            ) {
                Ok(size) => Ok(size),
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    Ok(0)
                }
            }
        })
        .try_reduce(
            || 0u64,
            |left, right| {
                left.checked_add(right).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory total exceeds the supported byte-count range",
                    )
                })
            },
        )
}

impl DirectoryFrame {
    fn new(directory: File, ext4_eof_cookie: bool) -> io::Result<Self> {
        let DirectoryEntries { names, entries } =
            read_directory(directory.as_raw_fd(), ext4_eof_cookie, false)?;
        Ok(Self {
            directory,
            names,
            entries,
            next_entry: 0,
            disk_size: 0,
            overflowed: false,
            ext4_eof_cookie,
        })
    }

    fn add_size(&mut self, size: u64) {
        if let Some(total) = self.disk_size.checked_add(size) {
            self.disk_size = total;
        } else {
            self.overflowed = true;
        }
    }

    fn finish(self) -> io::Result<u64> {
        if self.overflowed {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory total exceeds the supported byte-count range",
            ))
        } else {
            Ok(self.disk_size)
        }
    }
}

fn scan_directory_iterative(
    directory: File,
    apparent: bool,
    ext4_eof_cookie: bool,
    skipped_entries: &AtomicUsize,
) -> io::Result<u64> {
    let root = DirectoryFrame::new(directory, ext4_eof_cookie)?;
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
                    frame.names.as_slice(),
                    entry,
                    apparent,
                    frame.ext4_eof_cookie,
                )
            };

            match scanned {
                Ok(ScannedEntry::Item(size)) => stack
                    .last_mut()
                    .expect("root directory frame remains present")
                    .add_size(size),
                Ok(ScannedEntry::Directory {
                    directory,
                    ext4_eof_cookie,
                }) => match DirectoryFrame::new(directory, ext4_eof_cookie) {
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

        let finished = stack
            .pop()
            .expect("root directory frame remains present")
            .finish();
        if let Some(parent) = stack.last_mut() {
            match finished {
                Ok(size) => parent.add_size(size),
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            return finished;
        }
    }
}

fn scan_entry_size(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    apparent: bool,
    ext4_eof_cookie: bool,
    child_depth: usize,
    skipped_entries: &AtomicUsize,
) -> io::Result<u64> {
    match entry.kind {
        EntryKind::Directory => scan_child_directory(
            parent,
            entry.as_c_str(names),
            apparent,
            ext4_eof_cookie,
            child_depth,
            skipped_entries,
        ),
        EntryKind::Other => with_stat_at(
            parent.as_raw_fd(),
            entry.as_c_str(names),
            entry_size_mask(apparent),
            |stat| file_size(stat, apparent),
        ),
        EntryKind::Unknown => {
            let scanned = with_stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                STATX_TYPE | entry_size_mask(apparent),
                |stat| {
                    if stat.mask & STATX_TYPE == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "filesystem did not report an unknown entry type",
                        ));
                    }
                    if stat.is_directory() {
                        Ok(UnknownEntry::Directory)
                    } else {
                        file_size(stat, apparent).map(UnknownEntry::Item)
                    }
                },
            )?;
            match scanned {
                UnknownEntry::Directory => scan_child_directory(
                    parent,
                    entry.as_c_str(names),
                    apparent,
                    ext4_eof_cookie,
                    child_depth,
                    skipped_entries,
                ),
                UnknownEntry::Item(size) => Ok(size),
            }
        }
    }
}

fn scan_entry_iterative(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    apparent: bool,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    match entry.kind {
        EntryKind::Directory => open_child_entry(
            parent,
            entry.as_c_str(names),
            ext4_eof_cookie,
        ),
        EntryKind::Other => with_stat_at(
            parent.as_raw_fd(),
            entry.as_c_str(names),
            entry_size_mask(apparent),
            |stat| file_size(stat, apparent).map(ScannedEntry::Item),
        ),
        EntryKind::Unknown => {
            let scanned = with_stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                STATX_TYPE | entry_size_mask(apparent),
                |stat| {
                    if stat.mask & STATX_TYPE == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "filesystem did not report an unknown entry type",
                        ));
                    }
                    if stat.is_directory() {
                        Ok(UnknownEntry::Directory)
                    } else {
                        file_size(stat, apparent).map(UnknownEntry::Item)
                    }
                },
            )?;
            match scanned {
                UnknownEntry::Directory => open_child_entry(
                    parent,
                    entry.as_c_str(names),
                    ext4_eof_cookie,
                ),
                UnknownEntry::Item(size) => Ok(ScannedEntry::Item(size)),
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
    name: &CStr,
    apparent: bool,
    ext4_eof_cookie: bool,
    child_depth: usize,
    skipped_entries: &AtomicUsize,
) -> io::Result<u64> {
    let (child, child_ext4_eof_cookie) = open_child_directory(parent, name, ext4_eof_cookie)?;
    scan_directory_contents(
        child,
        apparent,
        child_ext4_eof_cookie,
        child_depth,
        skipped_entries,
    )
}

fn open_child_entry(
    parent: &File,
    name: &CStr,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    let (directory, ext4_eof_cookie) = open_child_directory(parent, name, ext4_eof_cookie)?;
    Ok(ScannedEntry::Directory {
        directory,
        ext4_eof_cookie,
    })
}

fn open_child_directory(
    parent: &File,
    name: &CStr,
    ext4_eof_cookie: bool,
) -> io::Result<(File, bool)> {
    match open_with_nofile_retry(|| openat2_directory(parent.as_raw_fd(), name)) {
        Ok(child) => Ok((child, ext4_eof_cookie)),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS) | Some(libc::EPERM)
            ) =>
        {
            let child = open_with_nofile_retry(|| openat_directory(parent.as_raw_fd(), name))?;
            Ok((child, ext4_eof_cookie))
        }
        Err(error) => Err(error),
    }
}

fn open_with_nofile_retry(mut open: impl FnMut() -> io::Result<File>) -> io::Result<File> {
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
        resolve: 0,
    };
    let child_fd = openat2_call(parent_fd, name, &how)?;
    Ok(unsafe { File::from_raw_fd(child_fd) })
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

fn file_size(stat: &LinuxStatx, apparent: bool) -> io::Result<u64> {
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
    Ok(size)
}

fn with_stat_at<T>(
    parent_fd: libc::c_int,
    name: &CStr,
    mask: libc::c_uint,
    inspect: impl FnOnce(&LinuxStatx) -> io::Result<T>,
) -> io::Result<T> {
    with_statx(
        parent_fd,
        name.as_ptr(),
        libc::AT_SYMLINK_NOFOLLOW | STATX_DONT_SYNC,
        mask,
        inspect,
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

// Inspect the result in place so each 256-byte statx record is not copied on return.
fn with_statx<T>(
    directory_fd: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mask: libc::c_uint,
    inspect: impl FnOnce(&LinuxStatx) -> io::Result<T>,
) -> io::Result<T> {
    let mut stat = mem::MaybeUninit::<LinuxStatx>::uninit();
    retry_interrupted(|| statx_call(directory_fd, path, flags, mask, stat.as_mut_ptr()))?;
    // A successful Linux statx call writes the entire fixed-width result structure.
    inspect(unsafe { stat.assume_init_ref() })
}

// Avoid the variadic libc syscall wrapper on the high-volume x86-64 statx path.
#[cfg(target_arch = "x86_64")]
fn statx_call(
    directory_fd: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mask: libc::c_uint,
    result: *mut LinuxStatx,
) -> io::Result<()> {
    // SAFETY: The syscall ABI assigns statx's five arguments to these registers, and `result`
    // points to the live, 256-byte output buffer owned by `with_statx`.
    let result_code = unsafe {
        let mut result_code = libc::SYS_statx as libc::c_long;
        std::arch::asm!(
            "syscall",
            inlateout("rax") result_code,
            in("rdi") directory_fd as libc::c_long,
            in("rsi") path,
            in("rdx") flags as libc::c_long,
            in("r10") mask as libc::c_long,
            in("r8") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
        result_code
    };
    if result_code < 0 {
        Err(io::Error::from_raw_os_error((-result_code) as i32))
    } else {
        Ok(())
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn statx_call(
    directory_fd: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mask: libc::c_uint,
    result: *mut LinuxStatx,
) -> io::Result<()> {
    // SAFETY: The arguments match the target's statx syscall signature. The result pointer targets
    // a live output buffer.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            directory_fd,
            path,
            flags,
            mask,
            result,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn raw_syscall3(
    number: libc::c_long,
    first: libc::c_long,
    second: libc::c_long,
    third: libc::c_long,
) -> libc::c_long {
    let mut result = number;
    // SAFETY: The caller follows the syscall ABI. Any pointed-to memory stays valid for the call.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn raw_syscall4(
    number: libc::c_long,
    first: libc::c_long,
    second: libc::c_long,
    third: libc::c_long,
    fourth: libc::c_long,
) -> libc::c_long {
    let mut result = number;
    // SAFETY: The caller follows the syscall ABI. Any pointed-to memory stays valid for the call.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            in("r10") fourth,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn getdents64_call(fd: libc::c_int, buffer: &mut [u8]) -> io::Result<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        let result = unsafe {
            raw_syscall3(
                libc::SYS_getdents64 as libc::c_long,
                fd as libc::c_long,
                buffer.as_mut_ptr() as libc::c_long,
                buffer.len() as libc::c_long,
            )
        };
        if result < 0 {
            Err(io::Error::from_raw_os_error((-result) as i32))
        } else {
            Ok(result as usize)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        let result = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}

fn openat2_call(parent_fd: libc::c_int, name: &CStr, how: &OpenHow) -> io::Result<libc::c_int> {
    #[cfg(target_arch = "x86_64")]
    {
        let result = unsafe {
            raw_syscall4(
                libc::SYS_openat2 as libc::c_long,
                parent_fd as libc::c_long,
                name.as_ptr() as libc::c_long,
                how as *const OpenHow as libc::c_long,
                mem::size_of::<OpenHow>() as libc::c_long,
            )
        };
        if result < 0 {
            Err(io::Error::from_raw_os_error((-result) as i32))
        } else {
            Ok(result as libc::c_int)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        let result = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                parent_fd,
                name.as_ptr(),
                how as *const OpenHow,
                mem::size_of::<OpenHow>(),
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as libc::c_int)
        }
    }
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

fn read_directory(
    fd: libc::c_int,
    ext4_eof_cookie: bool,
    directories_only: bool,
) -> io::Result<DirectoryEntries> {
    DIRECTORY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        let mut entries = PooledDirectoryEntries::take();
        let mut names = DirectoryNames::new();
        let mut first_batch = true;

        loop {
            let bytes_read = match getdents64_call(fd, buffer.as_mut_slice()) {
                Ok(bytes_read) => bytes_read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if bytes_read == 0 {
                break;
            }

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
                let cached_capacity_remaining =
                    MAX_CACHED_DIRECTORY_ENTRY_CAPACITY.saturating_sub(entries.len());
                // Keep eager allocation within the reusable-vector limit; larger dirs can grow.
                entries.reserve(estimated_entries.min(cached_capacity_remaining));
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

                if name_bytes != b"."
                    && name_bytes != b".."
                    && (!directories_only || !matches!(kind, EntryKind::Other))
                {
                    let length = u16::try_from(name_bytes.len()).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "directory entry name exceeds the supported length",
                        )
                    })?;
                    let start = names.append_name(name_bytes, bytes_read / 4);
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
        retry_interrupted, scan, scan_entry_size, DirectoryEntry, DirectoryNames, EntryKind,
        DIRECTORY_ENTRY_POOL, MAX_CACHED_DIRECTORY_ENTRY_CAPACITY,
    };
    use std::cell::Cell;
    use std::fs::{self, File};
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{symlink, MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn report_size(report: &super::ScanReport, name: &str) -> u64 {
        report
            .directories
            .iter()
            .find(|directory| directory.name == name)
            .expect("top-level directory is present")
            .disk_size
    }

    fn allocated_size(path: &Path) -> io::Result<u64> {
        Ok(fs::symlink_metadata(path)?.blocks() * 512)
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
    fn directory_names_stay_inline_until_the_fixed_capacity_is_exceeded() {
        let mut names = DirectoryNames::new();
        names.append_name(b"first", 32);
        names.append_name(b"123456789", 32);
        assert_eq!(names.as_slice(), b"first\x00123456789\0");
        assert!(matches!(&names, DirectoryNames::Inline(_)));

        names.append_name(b"second", 32);
        assert_eq!(names.as_slice(), b"first\x00123456789\0second\0");
        assert!(matches!(&names, DirectoryNames::Heap(_)));
    }

    #[test]
    fn reuses_small_entry_vectors_and_drops_oversized_ones() {
        let mut entries = super::PooledDirectoryEntries::take();
        entries.reserve(64);
        let cached_capacity = entries.capacity();
        assert!(cached_capacity <= MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);
        drop(entries);

        let entries = super::PooledDirectoryEntries::take();
        assert_eq!(entries.capacity(), cached_capacity);
        drop(entries);

        let mut oversized = super::PooledDirectoryEntries::take();
        oversized.reserve(MAX_CACHED_DIRECTORY_ENTRY_CAPACITY + 1);
        assert!(oversized.capacity() > MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);
        drop(oversized);

        let replacement = super::PooledDirectoryEntries::take();
        assert!(replacement.capacity() <= MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);
        drop(replacement);

        let mut buffers = (0..=super::MAX_CACHED_DIRECTORY_ENTRY_VECTORS)
            .map(|_| super::PooledDirectoryEntries::take())
            .collect::<Vec<_>>();
        for buffer in &mut buffers {
            buffer.reserve(1);
        }
        drop(buffers);
        let cached_count = DIRECTORY_ENTRY_POOL.with(|pool| pool.borrow().len());
        assert_eq!(cached_count, super::MAX_CACHED_DIRECTORY_ENTRY_VECTORS);
    }

    #[test]
    fn scans_unknown_file_and_directory_types() -> io::Result<()> {
        let temporary = TemporaryDirectory::new()?;
        fs::create_dir(temporary.0.join("child"))?;
        let mut nested = File::create(temporary.0.join("child").join("nested"))?;
        nested.write_all(b"abc")?;
        let mut file = File::create(temporary.0.join("file"))?;
        file.write_all(b"xy")?;
        let root = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&temporary.0)?;
        let names = b"file\0child\0";
        let skipped_entries = AtomicUsize::new(0);

        let file_size = scan_entry_size(
            &root,
            names,
            DirectoryEntry {
                name_offset: 0,
                name_length: 4,
                kind: EntryKind::Unknown,
            },
            true,
            false,
            1,
            &skipped_entries,
        )?;
        assert_eq!(file_size, 2);

        let child_size = scan_entry_size(
            &root,
            names,
            DirectoryEntry {
                name_offset: 5,
                name_length: 5,
                kind: EntryKind::Unknown,
            },
            true,
            false,
            1,
            &skipped_entries,
        )?;
        assert_eq!(child_size, 3);
        assert_eq!(skipped_entries.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[test]
    fn reports_only_immediate_directories_and_counts_each_descendant_path() -> io::Result<()> {
        let temporary = TemporaryDirectory::new()?;
        let root = &temporary.0;
        let included = root.join("included");
        let empty = root.join("empty");
        fs::create_dir(&included)?;
        fs::create_dir(&empty)?;

        let payload = included.join("payload.bin");
        let hardlink = included.join("payload-hardlink.bin");
        let sparse = included.join("sparse.bin");
        let invalid_name = std::ffi::OsString::from_vec(b"invalid-\xff-name".to_vec());
        let invalid_path = included.join(invalid_name);
        let symlink_path = included.join("payload-link");
        let nested = included.join("nested");
        let nested_payload = nested.join("nested.bin");

        fs::write(&payload, vec![0x5a; 8192])?;
        fs::hard_link(&payload, &hardlink)?;
        File::create(&sparse)?.set_len(1024 * 1024)?;
        fs::write(&invalid_path, b"raw bytes")?;
        symlink("payload.bin", &symlink_path)?;
        fs::create_dir(&nested)?;
        fs::write(&nested_payload, vec![0x31; 257])?;
        File::create(root.join("root-file"))?.write_all(b"excluded")?;

        let expected_allocated = allocated_size(&payload)? * 2
            + allocated_size(&sparse)?
            + allocated_size(&invalid_path)?
            + allocated_size(&symlink_path)?
            + allocated_size(&nested_payload)?;
        let report = scan(root, false)?;
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(report.directories.len(), 2);
        assert_eq!(report.directories[0].name, "included");
        assert_eq!(report_size(&report, "included"), expected_allocated);
        assert_eq!(report_size(&report, "empty"), 0);

        let expected_apparent = fs::symlink_metadata(&payload)?.len() * 2
            + fs::symlink_metadata(&sparse)?.len()
            + fs::symlink_metadata(&invalid_path)?.len()
            + fs::symlink_metadata(&symlink_path)?.len()
            + fs::symlink_metadata(&nested_payload)?.len();
        let report = scan(root, true)?;
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(report.directories.len(), 2);
        assert_eq!(report_size(&report, "included"), expected_apparent);
        assert_eq!(report_size(&report, "empty"), 0);
        Ok(())
    }

    #[test]
    fn scans_many_long_names_across_directory_buffer_batches() -> io::Result<()> {
        const FILE_COUNT: usize = 2050;
        const NAME_LENGTH: usize = 250;

        let temporary = TemporaryDirectory::new()?;
        let top_level = temporary.0.join("large");
        fs::create_dir(&top_level)?;
        let mut expected_allocated = 0;
        for index in 0..FILE_COUNT {
            let mut name = format!("entry-{index:04}").into_bytes();
            name.resize(NAME_LENGTH, b'x');
            let path = top_level.join(std::ffi::OsString::from_vec(name));
            File::create(&path)?.write_all(b"x")?;
            expected_allocated += allocated_size(&path)?;
        }

        DIRECTORY_ENTRY_POOL.with(|pool| pool.borrow_mut().clear());
        let report = scan(&temporary.0, false)?;
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(report.directories.len(), 1);
        assert_eq!(report_size(&report, "large"), expected_allocated);
        let cached_entry_capacity = DIRECTORY_ENTRY_POOL.with(|pool| {
            pool.borrow()
                .iter()
                .map(Vec::capacity)
                .max()
                .unwrap_or(0)
        });
        assert!(cached_entry_capacity >= FILE_COUNT);
        assert!(cached_entry_capacity <= MAX_CACHED_DIRECTORY_ENTRY_CAPACITY);

        Ok(())
    }

    #[test]
    fn deep_directory_descent_uses_the_iterative_scanner() -> io::Result<()> {
        let temporary = TemporaryDirectory::new()?;
        let top_level = temporary.0.join("top");
        fs::create_dir(&top_level)?;
        let mut current = top_level.clone();
        for _ in 0..70 {
            current.push("d");
            fs::create_dir(&current)?;
        }
        let payload = current.join("leaf");
        File::create(&payload)?.write_all(b"deep")?;

        let report = scan(&temporary.0, true)?;
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(report_size(&report, "top"), 4);
        Ok(())
    }
}
