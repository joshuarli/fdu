#[cfg(not(target_os = "linux"))]
compile_error!("fdu supports Linux only");

mod linux;
#[cfg(feature = "allocation-profile")]
mod allocation_profile;

use std::env;
use std::error::Error;
use std::ffi::{CStr, OsString};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::process;

const DEFAULT_RAYON_THREADS: usize = 4;

struct TopLevelDirectory {
    pub name: OsString,
    pub disk_size: u64,
}

struct ScanReport {
    pub directories: Vec<TopLevelDirectory>,
    pub skipped_entries: usize,
}

struct Options {
    path: PathBuf,
    apparent: bool,
    help: bool,
}

fn main() {
    let result = run();
    #[cfg(feature = "allocation-profile")]
    allocation_profile::report();
    if let Err(error) = result {
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
    let mut output = stdout.lock();
    for directory in report.directories {
        writeln!(
            output,
            "{}\t{:?}",
            directory.disk_size,
            directory.name
        )?;
    }
    output.flush()?;
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
    println!("Count disk usage and print each immediate child directory's size.");
    println!("By default, sizes are allocated bytes; --apparent uses logical lengths.");
}
