#[cfg(not(target_os = "linux"))]
compile_error!("fdu supports Linux only");

mod linux;

use std::env;
use std::error::Error;
use std::ffi::{CStr, OsString};
use std::io::{self, BufWriter, Write};
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process;

const DEFAULT_RAYON_THREADS: usize = 4;

struct DiskItem {
    name_offset: NonZeroUsize,
    disk_size: u64,
    children: Option<DirectoryId>,
}

impl DiskItem {
    fn new(name_offset: usize, disk_size: u64, children: Option<DirectoryId>) -> Self {
        // The zero niche keeps the skipped outcome the same size as a scanned directory item.
        let name_offset = name_offset
            .checked_add(1)
            .and_then(NonZeroUsize::new)
            .expect("directory name offset is representable");
        Self {
            name_offset,
            disk_size,
            children,
        }
    }

    fn name_offset(&self) -> usize {
        self.name_offset.get() - 1
    }
}

// One outcome per entry keeps Rayon collection indexed and avoids merging filtered worker vectors.
enum DirectoryItem {
    Scanned(DiskItem),
    Skipped,
}

impl DirectoryItem {
    fn disk_size(&self) -> u64 {
        match self {
            Self::Scanned(item) => item.disk_size,
            Self::Skipped => 0,
        }
    }

    fn children(&self) -> Option<DirectoryId> {
        match self {
            Self::Scanned(item) => item.children,
            Self::Skipped => None,
        }
    }
}

// Nonzero indices keep an optional child link to one machine word.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct DirectoryId(NonZeroUsize);

impl DirectoryId {
    fn from_index(index: usize) -> Self {
        Self(NonZeroUsize::new(index + 1).expect("directory arena index is representable"))
    }

    fn index(self) -> usize {
        self.0.get() - 1
    }
}

#[derive(Clone, Copy)]
struct StoredDirectory {
    id: DirectoryId,
    disk_size: u64,
}

struct DirectoryContents {
    names: Vec<u8>,
    items: Vec<DirectoryItem>,
    disk_size: u64,
}

#[cfg(target_pointer_width = "64")]
const _: [(); 24] = [(); std::mem::size_of::<DiskItem>()];
const _: [(); std::mem::size_of::<DiskItem>()] =
    [(); std::mem::size_of::<DirectoryItem>()];

struct ScanReport {
    root_name: OsString,
    root: DirectoryContents,
    directories: Vec<DirectoryContents>,
    skipped_entries: usize,
}

impl DirectoryContents {
    fn name_bytes(&self, offset: usize) -> &[u8] {
        let name_and_terminator = &self.names[offset..];
        let length = name_and_terminator
            .iter()
            .position(|byte| *byte == 0)
            .expect("directory name has a terminator");
        &name_and_terminator[..length]
    }
}

struct Options {
    path: PathBuf,
    apparent: bool,
    help: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fdu: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_args()?;
    if options.help {
        print_help();
        return Ok(());
    }

    require_linux_6()?;
    configure_rayon_threads();

    let report = linux::scan(&options.path, options.apparent)?;
    if report.skipped_entries != 0 {
        eprintln!(
            "fdu: warning: skipped {} entries after errors; size totals may be incomplete",
            report.skipped_entries
        );
    }
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    write_tree(
        &report.root_name,
        &report.root,
        &report.directories,
        &mut output,
    )?;
    Ok(())
}

fn parse_args() -> io::Result<Options> {
    let mut path = None;
    let mut apparent = false;
    let mut help = false;
    let mut positional_only = false;

    for argument in env::args_os().skip(1) {
        if !positional_only {
            match argument.to_str() {
                Some("--") => {
                    positional_only = true;
                    continue;
                }
                Some("-h") | Some("--help") => {
                    help = true;
                    continue;
                }
                Some("-a") | Some("--apparent") => {
                    apparent = true;
                    continue;
                }
                Some(option) if option.starts_with('-') => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown option: {option}"),
                    ));
                }
                _ => {}
            }
        }

        if path.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only one path may be scanned",
            ));
        }
        path = Some(PathBuf::from(argument));
    }

    Ok(Options {
        path: path.unwrap_or_else(|| PathBuf::from(".")),
        apparent,
        help,
    })
}

fn require_linux_6() -> io::Result<()> {
    let mut system = MaybeUninit::<libc::utsname>::zeroed();
    if unsafe { libc::uname(system.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let system = unsafe { system.assume_init() };
    let release = unsafe { CStr::from_ptr(system.release.as_ptr()) };
    let major_bytes = release
        .to_bytes()
        .split(|byte| *byte == b'.')
        .next()
        .unwrap_or_default();
    let major = std::str::from_utf8(major_bytes)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "could not parse the running Linux kernel version",
            )
        })?;

    if major < 6 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Linux kernel 6.0 or newer is required; running {}", release.to_string_lossy()),
        ));
    }
    Ok(())
}

// Set the default before Rayon creates its global pool on the first parallel operation.
fn configure_rayon_threads() {
    if env::var_os("RAYON_NUM_THREADS").is_none() {
        env::set_var("RAYON_NUM_THREADS", DEFAULT_RAYON_THREADS.to_string());
    }
}

fn print_help() {
    println!("Usage: fdu [--apparent] [PATH]");
    println!();
    println!("Scan a directory and print an indented tree with raw byte counts.");
    println!("By default, sizes are allocated bytes; --apparent uses logical lengths.");
}

#[derive(Clone, Copy)]
enum DirectoryReference<'a> {
    Inline(&'a DirectoryContents),
    Stored(DirectoryId),
}

struct OutputFrame<'a> {
    name: &'a [u8],
    disk_size: u64,
    directory: Option<DirectoryReference<'a>>,
    depth: usize,
    wrote_item: bool,
    next_child: usize,
}

fn write_tree(
    root_name: &std::ffi::OsStr,
    root: &DirectoryContents,
    directories: &[DirectoryContents],
    output: &mut impl Write,
) -> io::Result<()> {
    let mut stack = vec![OutputFrame {
        name: root_name.as_bytes(),
        disk_size: root.disk_size,
        directory: Some(DirectoryReference::Inline(root)),
        depth: 0,
        wrote_item: false,
        next_child: 0,
    }];

    while let Some(frame) = stack.last_mut() {
        if !frame.wrote_item {
            for _ in 0..frame.depth {
                output.write_all(b"  ")?;
            }
            write_size_and_tab(frame.disk_size, output)?;
            write_name(frame.name, output)?;
            output.write_all(b"\n")?;
            frame.wrote_item = true;
        }

        let next_child = loop {
            let frame = stack.last_mut().expect("output frame is present");
            let directory = match frame.directory {
                Some(DirectoryReference::Inline(directory)) => directory,
                Some(DirectoryReference::Stored(directory_id)) => {
                    &directories[directory_id.index()]
                }
                None => break None,
            };
            match directory.items.get(frame.next_child) {
                Some(DirectoryItem::Skipped) => frame.next_child += 1,
                Some(DirectoryItem::Scanned(child)) => {
                    frame.next_child += 1;
                    break Some((
                        directory.name_bytes(child.name_offset()),
                        child.disk_size,
                        child.children.map(DirectoryReference::Stored),
                        frame.depth + 1,
                    ));
                }
                None => break None,
            }
        };

        if let Some((name, disk_size, directory, depth)) = next_child {
            stack.push(OutputFrame {
                name,
                disk_size,
                directory,
                depth,
                wrote_item: false,
                next_child: 0,
            });
        } else {
            stack.pop();
        }
    }

    Ok(())
}

fn write_size_and_tab(mut size: u64, output: &mut impl Write) -> io::Result<()> {
    let mut digits = [0u8; 21];
    digits[20] = b'\t';
    let mut start = 20;
    loop {
        start -= 1;
        digits[start] = b'0' + (size % 10) as u8;
        size /= 10;
        if size == 0 {
            break;
        }
    }
    output.write_all(&digits[start..])
}

fn write_name(bytes: &[u8], output: &mut impl Write) -> io::Result<()> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let mut segment_start = 0;
        for (byte_index, character) in text.char_indices() {
            let escape = match character {
                '\\' => Some(&b"\\\\"[..]),
                '\t' => Some(&b"\\t"[..]),
                '\n' => Some(&b"\\n"[..]),
                '\r' => Some(&b"\\r"[..]),
                _ => None,
            };
            if escape.is_none() && !character.is_control() {
                continue;
            }

            output.write_all(&bytes[segment_start..byte_index])?;
            if let Some(escape) = escape {
                output.write_all(escape)?;
            } else {
                write!(output, "\\u{{{:x}}}", character as u32)?;
            }
            segment_start = byte_index + character.len_utf8();
        }
        return output.write_all(&bytes[segment_start..]);
    }

    let mut segment_start = 0;
    for (byte_index, byte) in bytes.iter().copied().enumerate() {
        let escape = match byte {
            b'\\' => Some(&b"\\\\"[..]),
            b'\t' => Some(&b"\\t"[..]),
            b'\n' => Some(&b"\\n"[..]),
            b'\r' => Some(&b"\\r"[..]),
            b' '..=b'~' => None,
            _ => None,
        };
        let escaped_byte = byte != b'\\'
            && byte != b'\t'
            && byte != b'\n'
            && byte != b'\r'
            && !(b' '..=b'~').contains(&byte);
        if escape.is_none() && !escaped_byte {
            continue;
        }

        output.write_all(&bytes[segment_start..byte_index])?;
        if let Some(escape) = escape {
            output.write_all(escape)?;
        } else {
            write!(output, "\\x{byte:02x}")?;
        }
        segment_start = byte_index + 1;
    }
    output.write_all(&bytes[segment_start..])
}
