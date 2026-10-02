#[cfg(not(target_os = "linux"))]
compile_error!("fdu supports Linux only");

mod linux;

use std::env;
use std::error::Error;
use std::ffi::{CStr, OsString};
use std::io::{self, BufWriter, Write};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process;

const MAX_DEFAULT_RAYON_THREADS: usize = 4;

pub(crate) struct DiskItem {
    name: OsString,
    disk_size: u64,
    children: Option<Vec<DiskItem>>,
}

pub(crate) struct ScanReport {
    pub(crate) tree: DiskItem,
    pub(crate) skipped_entries: usize,
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
    write_item(&report.tree, 0, &mut output)?;
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
        let workers = std::thread::available_parallelism()
            .map_or(1, |parallelism| parallelism.get().min(MAX_DEFAULT_RAYON_THREADS));
        env::set_var("RAYON_NUM_THREADS", workers.to_string());
    }
}

fn print_help() {
    println!("Usage: fdu [--apparent] [PATH]");
    println!();
    println!("Scan a directory and print an indented tree with raw byte counts.");
    println!("By default, sizes are allocated bytes; --apparent uses logical lengths.");
}

struct OutputFrame<'a> {
    item: &'a DiskItem,
    depth: usize,
    wrote_item: bool,
    next_child: usize,
}

fn write_item(item: &DiskItem, depth: usize, output: &mut impl Write) -> io::Result<()> {
    let mut stack = vec![OutputFrame {
        item,
        depth,
        wrote_item: false,
        next_child: 0,
    }];

    while let Some(frame) = stack.last_mut() {
        if !frame.wrote_item {
            for _ in 0..frame.depth {
                output.write_all(b"  ")?;
            }
            write!(output, "{}\t", frame.item.disk_size)?;
            write_name(&frame.item.name, output)?;
            output.write_all(b"\n")?;
            frame.wrote_item = true;
        }

        let next_child = {
            let frame = stack.last_mut().expect("output frame is present");
            let child = frame
                .item
                .children
                .as_ref()
                .and_then(|children| children.get(frame.next_child));
            if child.is_some() {
                frame.next_child += 1;
            }
            child.map(|child| (child, frame.depth + 1))
        };

        if let Some((child, depth)) = next_child {
            stack.push(OutputFrame {
                item: child,
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

fn write_name(name: &std::ffi::OsStr, output: &mut impl Write) -> io::Result<()> {
    let bytes = name.as_bytes();
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
