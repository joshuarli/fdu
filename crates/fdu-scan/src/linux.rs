use crate::{ScanReport, TopLevelDirectory};
use rayon::prelude::*;
use std::cell::RefCell;
use std::ffi::{CStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, TryLockError};

pub(crate) mod platform;

// A small reusable buffer bounds retained memory while batching wide directories.
const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
// A record needs a 19-byte header and at least one NUL byte for its name.
const MIN_DIRECTORY_RECORD_BYTES: usize = 20;
// These limits retain at most 512 KiB of parsed records in each scanning thread's pool.
const MAX_CACHED_DIRECTORY_ENTRY_CAPACITY: usize = 4096;
const MAX_CACHED_DIRECTORY_ENTRY_VECTORS: usize = 8;
// Only wide batches need metadata jobs beyond the parallel directory traversal.
const PARALLEL_METADATA_ENTRY_ESTIMATE_THRESHOLD: usize = 128;
// Avoid forcing remote metadata refreshes; remote results may reflect cached state.
const STATX_DONT_SYNC: libc::c_int = 0x4000;
const STATX_TYPE: libc::c_uint = 0x0001;
const STATX_SIZE: libc::c_uint = 0x0200;
const STATX_BLOCKS: libc::c_uint = 0x0400;
const STATX_MNT_ID: libc::c_uint = 0x1000;
const AT_EMPTY_PATH: libc::c_int = 0x1000;
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
    static DIRECTORY_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::new());
}

// A metadata batch can yield to another directory job on the same Rayon worker.
// Move the buffer out of TLS so that nested enumeration leases independent storage.
struct DirectoryBuffer {
    bytes: Vec<u8>,
    _worker: PhantomData<Rc<()>>,
}

impl DirectoryBuffer {
    fn with<T>(inspect: impl FnOnce(&mut [u8]) -> T) -> T {
        let mut bytes = DIRECTORY_BUFFER.with(|buffer| mem::take(&mut *buffer.borrow_mut()));
        if bytes.is_empty() {
            bytes.resize(DIRECTORY_BUFFER_BYTES, 0);
        }
        let mut buffer = Self {
            bytes,
            _worker: PhantomData,
        };
        inspect(&mut buffer.bytes)
    }
}

impl Drop for DirectoryBuffer {
    fn drop(&mut self) {
        let bytes = mem::take(&mut self.bytes);
        let _ = DIRECTORY_BUFFER.try_with(|buffer| {
            let mut cached = buffer.borrow_mut();
            if cached.is_empty() {
                *cached = bytes;
            }
        });
    }
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
    entries: DirectoryEntryStorage,
    _worker: PhantomData<Rc<()>>,
}

// Single-child chains retain their records without allocating at every level of descent.
enum DirectoryEntryStorage {
    Inline {
        entries: [DirectoryEntry; 2],
        len: usize,
    },
    Heap(Vec<DirectoryEntry>),
}

impl PooledDirectoryEntries {
    fn take() -> Self {
        Self {
            entries: DirectoryEntryStorage::Inline {
                entries: [DirectoryEntry {
                    name_offset: 0,
                    name_length: 0,
                    kind: EntryKind::Unknown,
                }; 2],
                len: 0,
            },
            _worker: PhantomData,
        }
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        match &self.entries {
            DirectoryEntryStorage::Inline { entries, .. } => entries.len(),
            DirectoryEntryStorage::Heap(entries) => entries.capacity(),
        }
    }

    fn reserve(&mut self, additional: usize) {
        if let DirectoryEntryStorage::Inline { entries, len } = &self.entries {
            if *len + additional <= entries.len() {
                return;
            }
            let mut heap = DIRECTORY_ENTRY_POOL.with(|pool| {
                pool.borrow_mut().pop().unwrap_or_default()
            });
            heap.reserve(*len + additional);
            heap.extend_from_slice(&entries[..*len]);
            self.entries = DirectoryEntryStorage::Heap(heap);
        } else if let DirectoryEntryStorage::Heap(entries) = &mut self.entries {
            entries.reserve(additional);
        }
    }

    fn push(&mut self, entry: DirectoryEntry, capacity_hint: usize) {
        if let DirectoryEntryStorage::Inline { entries, len } = &mut self.entries {
            if *len < entries.len() {
                entries[*len] = entry;
                *len += 1;
                return;
            }
            let additional = capacity_hint.saturating_sub(*len).max(1);
            self.reserve(additional);
        }
        let DirectoryEntryStorage::Heap(entries) = &mut self.entries else {
            unreachable!("inline entries promote before exceeding their capacity");
        };
        entries.push(entry);
    }
}

impl Deref for PooledDirectoryEntries {
    type Target = [DirectoryEntry];

    fn deref(&self) -> &Self::Target {
        match &self.entries {
            DirectoryEntryStorage::Inline { entries, len } => &entries[..*len],
            DirectoryEntryStorage::Heap(entries) => entries,
        }
    }
}

impl Drop for PooledDirectoryEntries {
    fn drop(&mut self) {
        let DirectoryEntryStorage::Heap(entries) = &mut self.entries else {
            return;
        };
        if entries.capacity() > MAX_CACHED_DIRECTORY_ENTRY_CAPACITY {
            return;
        }
        entries.clear();
        let entries = mem::take(entries);
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
    // Inline subtotal for known non-directory entries sized during enumeration.
    disk_size: u64,
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
    MountBoundary,
    Directory {
        directory: File,
        ext4_eof_cookie: bool,
    },
}

enum OpenedChildDirectory {
    MountBoundary,
    Directory(File, bool),
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

// Choose size accounting before descent so every metadata call requests a fixed field.
pub(super) fn scan(path: &Path, apparent: bool) -> io::Result<ScanReport> {
    if apparent {
        scan_mode::<true>(path)
    } else {
        scan_mode::<false>(path)
    }
}

fn scan_mode<const APPARENT: bool>(path: &Path) -> io::Result<ScanReport> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    let ext4_eof_cookie = is_ext4(root.as_raw_fd());
    let skipped_entries = AtomicUsize::new(0);
    let mount_boundaries = AtomicUsize::new(0);
    let DirectoryEntries { names, entries, .. } = read_directory::<APPARENT, true>(
        root.as_raw_fd(),
        ext4_eof_cookie,
        &skipped_entries,
    )?;
    let name_bytes = names.as_slice();

    let mut directories = entries
        .par_iter()
        .copied()
        .filter_map(|entry| {
            match scan_top_level_entry::<APPARENT>(
                &root,
                name_bytes,
                entry,
                ext4_eof_cookie,
                &skipped_entries,
                &mount_boundaries,
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
        mount_boundaries: mount_boundaries.load(Ordering::Relaxed),
        unsupported_aliases: 0,
    })
}

fn scan_top_level_entry<const APPARENT: bool>(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    ext4_eof_cookie: bool,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
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

    let disk_size = scan_child_directory::<APPARENT>(
        parent,
        entry.as_c_str(names),
        ext4_eof_cookie,
        1,
        skipped_entries,
        mount_boundaries,
    )?;
    Ok(Some(TopLevelDirectory {
        name: OsString::from_vec(entry.as_c_str(names).to_bytes().to_vec()),
        disk_size,
        exclusion: None,
    }))
}

fn scan_directory_contents<const APPARENT: bool>(
    directory: File,
    ext4_eof_cookie: bool,
    depth: usize,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    if depth >= MAX_RECURSIVE_DIRECTORY_DEPTH {
        return scan_directory_iterative::<APPARENT>(
            directory,
            ext4_eof_cookie,
            skipped_entries,
            mount_boundaries,
        );
    }

    let DirectoryEntries {
        names,
        entries,
        disk_size,
    } = read_directory::<APPARENT, false>(
        directory.as_raw_fd(),
        ext4_eof_cookie,
        skipped_entries,
    )?;
    if entries.is_empty() {
        return Ok(disk_size);
    }
    let name_bytes = names.as_slice();
    let child_size = entries
        .par_iter()
        .copied()
        .map(|entry| {
            match scan_entry_size::<APPARENT>(
                &directory,
                name_bytes,
                entry,
                ext4_eof_cookie,
                depth + 1,
                skipped_entries,
                mount_boundaries,
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
        )?;
    disk_size.checked_add(child_size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory total exceeds the supported byte-count range",
        )
    })
}

impl DirectoryFrame {
    fn new<const APPARENT: bool>(
        directory: File,
        ext4_eof_cookie: bool,
        skipped_entries: &AtomicUsize,
    ) -> io::Result<Self> {
        let DirectoryEntries {
            names,
            entries,
            disk_size,
        } = read_directory::<APPARENT, false>(
            directory.as_raw_fd(),
            ext4_eof_cookie,
            skipped_entries,
        )?;
        Ok(Self {
            directory,
            names,
            entries,
            next_entry: 0,
            disk_size,
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

fn scan_directory_iterative<const APPARENT: bool>(
    directory: File,
    ext4_eof_cookie: bool,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    let root = DirectoryFrame::new::<APPARENT>(directory, ext4_eof_cookie, skipped_entries)?;
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
                scan_entry_iterative::<APPARENT>(
                    &frame.directory,
                    frame.names.as_slice(),
                    entry,
                    frame.ext4_eof_cookie,
                )
            };

            match scanned {
                Ok(ScannedEntry::Item(size)) => stack
                    .last_mut()
                    .expect("root directory frame remains present")
                    .add_size(size),
                Ok(ScannedEntry::MountBoundary) => {
                    mount_boundaries.fetch_add(1, Ordering::Relaxed);
                }
                Ok(ScannedEntry::Directory {
                    directory,
                    ext4_eof_cookie,
                }) => match DirectoryFrame::new::<APPARENT>(
                    directory,
                    ext4_eof_cookie,
                    skipped_entries,
                ) {
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

fn scan_entry_size<const APPARENT: bool>(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
    ext4_eof_cookie: bool,
    child_depth: usize,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    match entry.kind {
        EntryKind::Directory => scan_child_directory::<APPARENT>(
            parent,
            entry.as_c_str(names),
            ext4_eof_cookie,
            child_depth,
            skipped_entries,
            mount_boundaries,
        ),
        EntryKind::Other => with_stat_at(
            parent.as_raw_fd(),
            entry.as_c_str(names),
            entry_size_mask::<APPARENT>(),
            |stat| file_size::<APPARENT>(stat),
        ),
        EntryKind::Unknown => {
            let scanned = with_stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                STATX_TYPE | entry_size_mask::<APPARENT>(),
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
                        file_size::<APPARENT>(stat).map(UnknownEntry::Item)
                    }
                },
            )?;
            match scanned {
                UnknownEntry::Directory => scan_child_directory::<APPARENT>(
                    parent,
                    entry.as_c_str(names),
                    ext4_eof_cookie,
                    child_depth,
                    skipped_entries,
                    mount_boundaries,
                ),
                UnknownEntry::Item(size) => Ok(size),
            }
        }
    }
}

fn scan_entry_iterative<const APPARENT: bool>(
    parent: &File,
    names: &[u8],
    entry: DirectoryEntry,
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
            entry_size_mask::<APPARENT>(),
            |stat| file_size::<APPARENT>(stat).map(ScannedEntry::Item),
        ),
        EntryKind::Unknown => {
            let scanned = with_stat_at(
                parent.as_raw_fd(),
                entry.as_c_str(names),
                STATX_TYPE | entry_size_mask::<APPARENT>(),
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
                        file_size::<APPARENT>(stat).map(UnknownEntry::Item)
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

fn entry_size_mask<const APPARENT: bool>() -> libc::c_uint {
    if APPARENT {
        STATX_SIZE
    } else {
        STATX_BLOCKS
    }
}

fn scan_child_directory<const APPARENT: bool>(
    parent: &File,
    name: &CStr,
    ext4_eof_cookie: bool,
    child_depth: usize,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    let (child, child_ext4_eof_cookie) = match open_child_directory(
        parent.as_raw_fd(),
        name,
        ext4_eof_cookie,
    )? {
        OpenedChildDirectory::MountBoundary => {
            mount_boundaries.fetch_add(1, Ordering::Relaxed);
            return Ok(0);
        }
        OpenedChildDirectory::Directory(child, child_ext4_eof_cookie) => {
            (child, child_ext4_eof_cookie)
        }
    };
    scan_directory_contents::<APPARENT>(
        child,
        child_ext4_eof_cookie,
        child_depth,
        skipped_entries,
        mount_boundaries,
    )
}

fn open_child_entry(
    parent: &File,
    name: &CStr,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    match open_child_directory(parent.as_raw_fd(), name, ext4_eof_cookie)? {
        OpenedChildDirectory::MountBoundary => Ok(ScannedEntry::MountBoundary),
        OpenedChildDirectory::Directory(directory, ext4_eof_cookie) => {
            Ok(ScannedEntry::Directory {
                directory,
                ext4_eof_cookie,
            })
        }
    }
}

fn open_child_directory(
    parent_fd: libc::c_int,
    name: &CStr,
    ext4_eof_cookie: bool,
) -> io::Result<OpenedChildDirectory> {
    match open_with_nofile_retry(|| openat2_directory(parent_fd, name)) {
        Ok(child) => Ok(OpenedChildDirectory::Directory(child, ext4_eof_cookie)),
        Err(error)
            if error.raw_os_error() == Some(libc::EXDEV) =>
        {
            Ok(OpenedChildDirectory::MountBoundary)
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS) | Some(libc::EPERM)
            ) =>
        {
            let child = open_with_nofile_retry(|| openat_directory(parent_fd, name))?;
            let parent_mount_id = mount_id_for_fd(parent_fd)?;
            let child_mount_id = mount_id_for_fd(child.as_raw_fd())?;
            if parent_mount_id == child_mount_id {
                Ok(OpenedChildDirectory::Directory(child, ext4_eof_cookie))
            } else {
                Ok(OpenedChildDirectory::MountBoundary)
            }
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
        resolve: RESOLVE_NO_XDEV,
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

#[inline(always)]
fn file_size<const APPARENT: bool>(stat: &LinuxStatx) -> io::Result<u64> {
    let size_mask = entry_size_mask::<APPARENT>();
    if stat.mask & size_mask != size_mask {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem did not report the requested file size",
        ));
    }
    let size = if APPARENT {
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

fn mount_id_for_fd(fd: libc::c_int) -> io::Result<u64> {
    with_statx(
        fd,
        b"\0".as_ptr().cast(),
        AT_EMPTY_PATH,
        STATX_MNT_ID,
        |stat| {
            if stat.mask & STATX_MNT_ID == 0 {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "kernel did not report the directory mount ID",
                ))
            } else {
                Ok(stat._mount_id)
            }
        },
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

// A directory record includes its 19-byte header, one NUL-terminated name, and padding.
// Validate only the name before that terminator; padding can contain arbitrary bytes.
#[inline]
fn directory_record_name(record: &[u8]) -> io::Result<&CStr> {
    let invalid_name = || {
        io::Error::new(io::ErrorKind::InvalidData, "invalid directory entry name")
    };
    if record.len() < MIN_DIRECTORY_RECORD_BYTES {
        return Err(invalid_name());
    }
    #[cfg(not(target_arch = "x86_64"))]
    let offset = 19;
    #[cfg(target_arch = "x86_64")]
    let offset = {
        use std::arch::x86_64::{
            _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128, _mm_set1_epi8,
            _mm_setzero_si128,
        };
        // Including three header bytes lets 32-byte records use one bounded vector load.
        let mut chunk_offset = 16;
        let mut header_bytes = 3;
        while record.len() - chunk_offset >= 16 {
            // SAFETY: x86-64 guarantees SSE2, and this unaligned load stays in the record.
            let special_bytes = unsafe {
                let bytes = _mm_loadu_si128(record.as_ptr().add(chunk_offset).cast());
                let terminators = _mm_cmpeq_epi8(bytes, _mm_setzero_si128());
                let separators = _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'/' as i8));
                (_mm_movemask_epi8(_mm_or_si128(terminators, separators)) as u32)
                    >> header_bytes
            };
            if special_bytes != 0 {
                let end = chunk_offset + header_bytes + special_bytes.trailing_zeros() as usize;
                if end == 19 || record[end] != 0 {
                    return Err(invalid_name());
                }
                // SAFETY: the first special byte is a NUL, and every load stayed in the record.
                return Ok(unsafe { CStr::from_bytes_with_nul_unchecked(&record[19..=end]) });
            }
            chunk_offset += 16;
            header_bytes = 0;
        }
        chunk_offset + header_bytes
    };

    for (index, byte) in record[offset..].iter().enumerate() {
        match byte {
            b'/' => return Err(invalid_name()),
            0 => {
                let end = offset + index;
                if end == 19 {
                    return Err(invalid_name());
                }
                // SAFETY: this loop stops at the first NUL within the record.
                return Ok(unsafe { CStr::from_bytes_with_nul_unchecked(&record[19..=end]) });
            }
            _ => {}
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "unterminated directory entry"))
}

fn read_directory<const APPARENT: bool, const DIRECTORIES_ONLY: bool>(
    fd: libc::c_int,
    ext4_eof_cookie: bool,
    skipped_entries: &AtomicUsize,
) -> io::Result<DirectoryEntries> {
    DirectoryBuffer::with(|buffer| {
        let mut collected = DirectoryEntries {
            names: DirectoryNames::new(),
            entries: PooledDirectoryEntries::take(),
            disk_size: 0,
        };
        let mut parallel_file_entries = false;
        let mut file_entries = Vec::new();
        let mut first_batch = true;

        loop {
            let bytes_read = match getdents64_call(fd, buffer) {
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
                parallel_file_entries = !DIRECTORIES_ONLY
                    && estimated_entries >= PARALLEL_METADATA_ENTRY_ESTIMATE_THRESHOLD;
                first_batch = false;
            }
            let final_offset = if parallel_file_entries {
                parse_directory_batch::<APPARENT, DIRECTORIES_ONLY, true>(
                    fd, &buffer[..bytes_read], &mut collected, &mut file_entries,
                    estimated_entries, skipped_entries,
                )?
            } else {
                parse_directory_batch::<APPARENT, DIRECTORIES_ONLY, false>(
                    fd, &buffer[..bytes_read], &mut collected, &mut file_entries,
                    estimated_entries, skipped_entries,
                )?
            };
            // Ext4 HTree reserves these d_off values for an exhausted directory.
            if ext4_eof_cookie
                && matches!(final_offset, EXT4_HTREE_EOF_32BIT | EXT4_HTREE_EOF_64BIT)
            {
                break;
            }
        }

        Ok(collected)
    })
}

// Each record is bounded before its name is inspected. Borrowed file names finish
// their metadata work before this enumeration batch is reused.
fn parse_directory_batch<
    const APPARENT: bool,
    const DIRECTORIES_ONLY: bool,
    const PARALLEL_METADATA: bool,
>(
    fd: libc::c_int,
    buffer: &[u8],
    collected: &mut DirectoryEntries,
    file_entries: &mut Vec<DirectoryEntry>,
    estimated_entries: usize,
    skipped_entries: &AtomicUsize,
) -> io::Result<i64> {
    debug_assert!(file_entries.is_empty());
    let bytes_read = buffer.len();
    let DirectoryEntries { names, entries, disk_size } = collected;
    let mut offset = 0;
    let mut final_offset = 0;
    while offset < bytes_read {
        let remaining = &buffer[offset..];
        if remaining.len() < 19 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid getdents64 record header",
            ));
        }

        let record_length = u16::from_ne_bytes([remaining[16], remaining[17]]) as usize;
        if record_length < 20 || record_length > remaining.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid getdents64 record length",
            ));
        }
        let record = &remaining[..record_length];
        final_offset = i64::from_ne_bytes(
            record[8..16]
                .try_into()
                .expect("fixed-width directory offset"),
        );

        let kind = match record[18] {
            libc::DT_DIR => EntryKind::Directory,
            libc::DT_UNKNOWN => EntryKind::Unknown,
            _ => EntryKind::Other,
        };
        let name = directory_record_name(record)?;
        let name_bytes = name.to_bytes();
        let name_length = name_bytes.len();

        if name_bytes != b"." && name_bytes != b".." {
            if !DIRECTORIES_ONLY && matches!(kind, EntryKind::Other) {
                if PARALLEL_METADATA {
                    if file_entries.is_empty() {
                        file_entries.reserve(estimated_entries);
                    }
                    // File records borrow only this batch; the u16 record length bounds names.
                    file_entries.push(DirectoryEntry {
                        name_offset: offset + 19,
                        name_length: name_length as u16,
                        kind,
                    });
                    offset += record_length;
                    continue;
                }
                // Narrow directories size files without retaining their names or records.
                match with_stat_at(
                    fd,
                    name,
                    entry_size_mask::<APPARENT>(),
                    |stat| file_size::<APPARENT>(stat),
                ) {
                    Ok(size) => {
                        *disk_size = disk_size.checked_add(size).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "directory total exceeds the supported byte-count range",
                            )
                        })?;
                    }
                    Err(_) => {
                        skipped_entries.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else if !DIRECTORIES_ONLY || !matches!(kind, EntryKind::Other) {
                let length = u16::try_from(name_bytes.len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory entry name exceeds the supported length",
                    )
                })?;
                let start = names.append_name(name_bytes, bytes_read / 2);
                entries.push(
                    DirectoryEntry {
                        name_offset: start,
                        name_length: length,
                        kind,
                    },
                    estimated_entries.min(MAX_CACHED_DIRECTORY_ENTRY_CAPACITY),
                );
            }
        }
        offset += record_length;
    }
    if PARALLEL_METADATA && !file_entries.is_empty() {
        let batch_size = file_batch_size::<APPARENT>(
            fd,
            buffer,
            &file_entries,
            skipped_entries,
        )?;
        *disk_size = disk_size.checked_add(batch_size).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory total exceeds the supported byte-count range",
            )
        })?;
        file_entries.clear();
    }
    Ok(final_offset)
}

fn file_batch_size<const APPARENT: bool>(
    fd: libc::c_int,
    names: &[u8],
    entries: &[DirectoryEntry],
    skipped_entries: &AtomicUsize,
) -> io::Result<u64> {
    entries
        .par_iter()
        .map(|entry| {
            match with_stat_at(
                fd,
                entry.as_c_str(names),
                entry_size_mask::<APPARENT>(),
                |stat| file_size::<APPARENT>(stat),
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

#[cfg(test)]
mod tests {
    use super::{
        retry_interrupted, scan_entry_size, DirectoryEntry, DirectoryNames, EntryKind,
        DIRECTORY_ENTRY_POOL, MAX_CACHED_DIRECTORY_ENTRY_CAPACITY,
    };
    use std::cell::Cell;
    use std::fs::{self, File};
    use std::io::{self, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
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

    fn allocated_size(path: &std::path::Path) -> io::Result<u64> {
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
    fn directory_record_names_respect_terminators_record_bounds_and_raw_bytes() {
        for length in 1..=255 {
            let record_length = (19 + length + 1 + 7) & !7;
            let mut record = vec![b'/'; record_length];
            let name = (0..length)
                .map(|index| if index % 3 == 0 { 0xff } else { b'x' })
                .collect::<Vec<_>>();
            record[19..19 + length].copy_from_slice(&name);
            record[19 + length] = 0;
            assert_eq!(super::directory_record_name(&record).unwrap().to_bytes(), name);

            record[19 + length] = b'x';
            assert!(super::directory_record_name(&record).is_err());
            record[19 + length] = 0;
            for slash_offset in 0..length {
                record[19 + slash_offset] = b'/';
                assert!(super::directory_record_name(&record).is_err());
                record[19 + slash_offset] = name[slash_offset];
            }
        }
        assert!(super::directory_record_name(&[0; 24]).is_err());
    }

    #[test]
    fn root_directory_batches_retain_only_child_candidates_and_reject_truncated_records() {
        let mut buffer = Vec::new();
        let mut record_boundaries = vec![0];
        for (name, kind) in [
            (b".".as_slice(), libc::DT_DIR),
            (b"..".as_slice(), libc::DT_DIR),
            (b"kept".as_slice(), libc::DT_DIR),
            (b"ignored".as_slice(), libc::DT_REG),
            (b"unknown".as_slice(), libc::DT_UNKNOWN),
        ] {
            let length = (19 + name.len() + 1 + 7) & !7;
            let start = buffer.len();
            buffer.resize(start + length, 0);
            buffer[start + 8..start + 16].copy_from_slice(&super::EXT4_HTREE_EOF_64BIT.to_ne_bytes());
            buffer[start + 16..start + 18].copy_from_slice(&(length as u16).to_ne_bytes());
            buffer[start + 18] = kind;
            buffer[start + 19..start + 19 + name.len()].copy_from_slice(name);
            record_boundaries.push(buffer.len());
        }
        let skipped_entries = AtomicUsize::new(0);
        for length in 0..=buffer.len() {
            let mut collected = super::DirectoryEntries {
                names: DirectoryNames::new(),
                entries: super::PooledDirectoryEntries::take(),
                disk_size: 0,
            };
            let mut files = Vec::new();
            let result = super::parse_directory_batch::<true, true, false>(
                -1, &buffer[..length], &mut collected, &mut files, 5, &skipped_entries,
            );
            assert_eq!(result.is_ok(), record_boundaries.contains(&length));
            assert_eq!(collected.disk_size, 0);
            assert!(files.is_empty());
            if length == buffer.len() {
                assert_eq!(result.unwrap(), super::EXT4_HTREE_EOF_64BIT);
                let names = collected.entries.iter()
                    .map(|entry| entry.as_c_str(collected.names.as_slice()).to_bytes())
                    .collect::<Vec<_>>();
                assert_eq!(names, [b"kept".as_slice(), b"unknown".as_slice()]);
            }
        }
        assert_eq!(skipped_entries.load(Ordering::Relaxed), 0);
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

        let mut entries = super::PooledDirectoryEntries::take();
        entries.reserve(64);
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
            buffer.reserve(64);
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
        let mount_boundaries = AtomicUsize::new(0);

        let file_size = scan_entry_size::<true>(
            &root,
            names,
            DirectoryEntry {
                name_offset: 0,
                name_length: 4,
                kind: EntryKind::Unknown,
            },
            false,
            1,
            &skipped_entries,
            &mount_boundaries,
        )?;
        assert_eq!(file_size, 2);

        let child_size = scan_entry_size::<true>(
            &root,
            names,
            DirectoryEntry {
                name_offset: 5,
                name_length: 5,
                kind: EntryKind::Unknown,
            },
            false,
            1,
            &skipped_entries,
            &mount_boundaries,
        )?;
        assert_eq!(child_size, 3);
        assert_eq!(skipped_entries.load(Ordering::Relaxed), 0);
        assert_eq!(mount_boundaries.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[test]
    fn directory_buffer_leases_preserve_outer_bytes_during_nested_enumeration() {
        super::DirectoryBuffer::with(|outer| {
            outer[0] = 41;
            let outer_pointer = outer.as_ptr();
            super::DirectoryBuffer::with(|nested| {
                assert_ne!(nested.as_ptr(), outer_pointer);
                nested[0] = 73;
                assert_eq!(outer[0], 41);
            });
            assert_eq!(outer[0], 41);
        });
        super::DirectoryBuffer::with(|reused| {
            assert_eq!(reused[0], 73);
            assert_eq!(reused.len(), super::DIRECTORY_BUFFER_BYTES);
        });
    }

    #[test]
    fn keeps_two_child_directories_inline_during_enumeration() -> io::Result<()> {
        let temporary = TemporaryDirectory::new()?;
        fs::create_dir(temporary.0.join("first"))?;
        fs::create_dir(temporary.0.join("second"))?;
        let directory = File::open(&temporary.0)?;
        let skipped_entries = AtomicUsize::new(0);
        let result = super::read_directory::<true, false>(
            std::os::fd::AsRawFd::as_raw_fd(&directory),
            false,
            &skipped_entries,
        )?;
        assert_eq!(result.entries.len(), 2);
        assert!(matches!(result.entries.entries, super::DirectoryEntryStorage::Inline { .. }));
        assert_eq!(result.disk_size, 0);
        assert_eq!(skipped_entries.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[test]
    fn sizes_wide_file_batches_without_retaining_file_records() -> io::Result<()> {
        let temporary = TemporaryDirectory::new()?;
        let mut expected_allocated = 0;
        for index in 0..160 {
            let path = temporary.0.join(format!("file-{index:04}"));
            fs::write(&path, b"batch")?;
            expected_allocated += allocated_size(&path)?;
        }
        let directory = File::open(&temporary.0)?;
        let skipped_entries = AtomicUsize::new(0);
        let result = super::read_directory::<false, false>(
            std::os::fd::AsRawFd::as_raw_fd(&directory),
            false,
            &skipped_entries,
        )?;
        assert_eq!(result.disk_size, expected_allocated);
        assert!(result.entries.is_empty());
        assert!(result.names.as_slice().is_empty());
        assert_eq!(skipped_entries.load(Ordering::Relaxed), 0);
        Ok(())
    }

}
