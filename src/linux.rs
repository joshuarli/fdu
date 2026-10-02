use crate::DiskItem;
use rayon::prelude::*;
use std::cell::RefCell;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const DIRECTORY_BUFFER_BYTES: usize = 128 * 1024;
const STATX_DONT_SYNC: libc::c_int = 0x4000;
const STATX_TYPE: libc::c_uint = 0x0001;
const STATX_SIZE: libc::c_uint = 0x0200;
const STATX_BLOCKS: libc::c_uint = 0x0400;
const STATX_BASIC_STATS: libc::c_uint = 0x07ff;
const RESOLVE_NO_XDEV: u64 = 0x01;
const EXT4_SUPER_MAGIC: u64 = 0xef53;
const EXT4_HTREE_EOF_32BIT: i64 = 0x7fff_ffff;
const EXT4_HTREE_EOF_64BIT: i64 = i64::MAX;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;

thread_local! {
    static DIRECTORY_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; DIRECTORY_BUFFER_BYTES]);
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

impl LinuxStatx {
    fn zeroed() -> Self {
        unsafe { mem::zeroed() }
    }

    fn is_directory(&self) -> bool {
        self.mask & STATX_TYPE != 0 && self.mode & S_IFMT == S_IFDIR
    }

    fn device(&self) -> Device {
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

pub(super) fn scan(path: &Path, apparent: bool) -> io::Result<DiskItem> {
    let name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("."))
        .to_string_lossy()
        .into_owned();
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let root_stat = stat_fd(root.as_raw_fd(), STATX_BASIC_STATS)?;
    if !root_stat.is_directory() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "path is not a directory",
        ));
    }

    let ext4_eof_cookie = is_ext4(root.as_raw_fd());
    scan_directory(name, root, root_stat.device(), apparent, ext4_eof_cookie)
}

fn scan_directory(
    name: String,
    directory: File,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
) -> io::Result<DiskItem> {
    let entries = read_directory(directory.as_raw_fd(), ext4_eof_cookie)?;
    let mut children = entries
        .into_par_iter()
        .filter_map(|entry| {
            scan_entry(&directory, entry, root_device, apparent, ext4_eof_cookie).ok()
        })
        .collect::<Vec<_>>();

    children.sort_unstable_by(|left, right| right.disk_size.cmp(&left.disk_size));
    let disk_size = children.iter().map(|child| child.disk_size).sum();

    Ok(DiskItem {
        name,
        disk_size,
        children: Some(children),
    })
}

fn scan_entry(
    parent: &File,
    entry: DirectoryEntry,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
) -> io::Result<DiskItem> {
    match entry.kind {
        EntryKind::Directory => {
            scan_child_directory(parent, entry, root_device, apparent, ext4_eof_cookie)
        }
        EntryKind::Other | EntryKind::Unknown => {
            let stat = stat_at(
                parent.as_raw_fd(),
                &entry.name,
                entry_metadata_mask(apparent),
            )?;
            if stat.is_directory() {
                scan_child_directory(parent, entry, root_device, apparent, ext4_eof_cookie)
            } else {
                Ok(file_item(entry, stat, apparent))
            }
        }
    }
}

fn entry_metadata_mask(apparent: bool) -> libc::c_uint {
    // Unknown dirent types need the mode; file sizes need only the selected accounting field.
    STATX_TYPE
        | if apparent {
            STATX_SIZE
        } else {
            STATX_BLOCKS
        }
}

fn scan_child_directory(
    parent: &File,
    entry: DirectoryEntry,
    root_device: Device,
    apparent: bool,
    ext4_eof_cookie: bool,
) -> io::Result<DiskItem> {
    let (child, child_ext4_eof_cookie) =
        open_child_directory(parent, &entry.name, root_device, ext4_eof_cookie)?;

    scan_directory(
        entry.name.to_string_lossy().into_owned(),
        child,
        root_device,
        apparent,
        child_ext4_eof_cookie,
    )
}

// A no-cross-mount open inherits the parent's device; the fallback checks st_dev explicitly.
fn open_child_directory(
    parent: &File,
    name: &CString,
    root_device: Device,
    ext4_eof_cookie: bool,
) -> io::Result<(File, bool)> {
    match openat2_directory(parent.as_raw_fd(), name) {
        Ok(child) => Ok((child, ext4_eof_cookie)),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EXDEV) | Some(libc::ENOSYS) | Some(libc::EPERM)
            ) =>
        {
            // The fallback checks st_dev so same-device bind mounts keep the existing behavior.
            let child = openat_directory(parent.as_raw_fd(), name)?;
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

fn openat2_directory(parent_fd: libc::c_int, name: &CString) -> io::Result<File> {
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

fn openat_directory(parent_fd: libc::c_int, name: &CString) -> io::Result<File> {
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

fn file_item(entry: DirectoryEntry, stat: LinuxStatx, apparent: bool) -> DiskItem {
    let size = if apparent { stat.size } else { stat.blocks * 512 };
    DiskItem {
        name: entry.name.to_string_lossy().into_owned(),
        disk_size: size,
        children: None,
    }
}

fn stat_at(parent_fd: libc::c_int, name: &CString, mask: libc::c_uint) -> io::Result<LinuxStatx> {
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

fn read_directory(fd: libc::c_int, ext4_eof_cookie: bool) -> io::Result<Vec<DirectoryEntry>> {
    DIRECTORY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        let mut entries = Vec::new();

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

                if name_bytes != b"." && name_bytes != b".." {
                    let name = CString::new(name_bytes).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid directory entry name")
                    })?;
                    entries.push(DirectoryEntry { name, kind });
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

        Ok(entries)
    })
}
