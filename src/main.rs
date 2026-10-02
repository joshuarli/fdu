#[cfg(not(target_os = "linux"))]
compile_error!("fdu supports Linux only");

mod linux;

use std::env;
use std::error::Error;
use std::ffi::CStr;
use std::io::{self, BufWriter, Write};
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::process;

const MAX_DEFAULT_RAYON_THREADS: usize = 4;

pub(crate) struct DiskItem {
    name: String,
    disk_size: u64,
    children: Option<Vec<DiskItem>>,
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

    let tree = linux::scan(&options.path, options.apparent)?;
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    write_item(&tree, 0, &mut output)?;
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

fn write_item(item: &DiskItem, depth: usize, output: &mut impl Write) -> io::Result<()> {
    for _ in 0..depth {
        output.write_all(b"  ")?;
    }
    writeln!(output, "{}\t{}", item.disk_size, item.name)?;

    if let Some(children) = &item.children {
        for child in children {
            write_item(child, depth + 1, output)?;
        }
    }
    Ok(())
}
