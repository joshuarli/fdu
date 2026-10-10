#[cfg(feature = "allocation-profile")]
mod allocation_profile;
#[cfg(feature = "interactive")]
mod browser;
#[cfg(feature = "interactive")]
mod rmrf;

use fdu_core::ExclusionReason;
use fdu_scan::{scan, ScanOptions, TopLevelDirectory};
use std::env;
use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process;

#[cfg(target_os = "linux")]
const DEFAULT_RAYON_THREADS: usize = 4;

struct Options {
    path: PathBuf,
    apparent: bool,
    help: bool,
    mode: RunMode,
    read_only: bool,
    rmrf: bool,
    assume_yes: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RunMode {
    Automatic,
    Summary,
    Interactive,
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

    #[cfg(target_os = "linux")]
    {
        require_linux_6()?;
        configure_rayon_threads();
    }

    if options.rmrf {
        return run_rmrf(options.path, options.assume_yes);
    }

    let terminal_available = io::stdin().is_terminal() && io::stdout().is_terminal();
    let use_interactive = match options.mode {
        RunMode::Summary => false,
        RunMode::Interactive if !terminal_available => {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "--interactive requires terminal input and output",
            )
            .into());
        }
        RunMode::Interactive => true,
        RunMode::Automatic => automatic_mode_uses_tui(terminal_available),
    };

    if use_interactive {
        return run_interactive(options.path, options.read_only, options.apparent);
    }

    let report = scan(&ScanOptions {
        path: options.path,
        apparent: options.apparent,
    })?;
    if report.skipped_entries != 0 {
        eprintln!(
            "fdu: warning: skipped {} entries after errors; size totals may be incomplete",
            report.skipped_entries
        );
    }
    if report.mount_boundaries != 0 {
        eprintln!(
            "fdu: warning: excluded {} filesystem or mount boundaries; totals omit their contents",
            report.mount_boundaries
        );
    }
    if report.unsupported_aliases != 0 {
        eprintln!(
            "fdu: warning: skipped {} unsupported directory aliases; affected totals omit their contents",
            report.unsupported_aliases
        );
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for directory in report.directories {
        write_summary_entry(&mut output, &directory)?;
    }
    output.flush()?;
    Ok(())
}

fn write_summary_entry(output: &mut impl Write, directory: &TopLevelDirectory) -> io::Result<()> {
    match directory.exclusion {
        Some(ExclusionReason::MountBoundary) => {
            writeln!(output, "MOUNT BOUNDARY\t{:?}", directory.name)
        }
        Some(ExclusionReason::UnsupportedAlias) => {
            writeln!(output, "UNSUPPORTED DIRECTORY ALIAS\t{:?}", directory.name)
        }
        None => writeln!(output, "{}\t{:?}", directory.disk_size, directory.name),
    }
}

fn parse_args() -> io::Result<Options> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut apparent = false;
    let mut help = false;
    let mut summary = false;
    let mut interactive = false;
    let mut read_only = false;
    let mut rmrf = false;
    let mut assume_yes = false;
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
                Some("--summary") => {
                    summary = true;
                    continue;
                }
                Some("--interactive") => {
                    interactive = true;
                    continue;
                }
                Some("--read-only") => {
                    read_only = true;
                    continue;
                }
                Some("--rmrf") => {
                    rmrf = true;
                    continue;
                }
                Some("--yes") => {
                    assume_yes = true;
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

        paths.push(PathBuf::from(argument));
        if paths.len() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only one path may be given",
            ));
        }
    }

    if summary && interactive {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--summary and --interactive cannot be used together",
        ));
    }
    if rmrf {
        for (conflicting, flag) in [
            (summary, "--summary"),
            (interactive, "--interactive"),
            (read_only, "--read-only"),
            (apparent, "--apparent"),
        ] {
            if conflicting {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("--rmrf cannot be used with {flag}"),
                ));
            }
        }
        // Emptying the current directory must be spelled out, never implied.
        if paths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--rmrf requires an explicit PATH",
            ));
        }
    } else if assume_yes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--yes only applies to --rmrf",
        ));
    }

    Ok(Options {
        path: paths.pop().unwrap_or_else(|| PathBuf::from(".")),
        apparent,
        help,
        mode: if summary {
            RunMode::Summary
        } else if interactive {
            RunMode::Interactive
        } else {
            RunMode::Automatic
        },
        read_only,
        rmrf,
        assume_yes,
    })
}

fn automatic_mode_uses_tui(terminal_available: bool) -> bool {
    terminal_available && cfg!(feature = "interactive")
}

#[cfg(target_os = "linux")]
fn require_linux_6() -> io::Result<()> {
    let system = rustix::system::uname();
    let release = system.release();
    let major = release
        .to_bytes()
        .split(|byte| *byte == b'.')
        .next()
        .and_then(|major| std::str::from_utf8(major).ok())
        .and_then(|major| major.parse::<u32>().ok())
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
#[cfg(target_os = "linux")]
fn configure_rayon_threads() {
    if env::var_os("RAYON_NUM_THREADS").is_none() {
        env::set_var("RAYON_NUM_THREADS", DEFAULT_RAYON_THREADS.to_string());
    }
}

fn print_help() {
    println!("Usage: fdu [--apparent] [--summary | --interactive] [--read-only] [PATH]");
    println!("       fdu --rmrf [--yes] PATH");
    println!();
    println!("Browse a directory in the terminal, or print immediate child-directory totals.");
    println!("By default, sizes are allocated bytes; --apparent uses logical lengths.");
    println!("--summary forces output mode; --interactive requires a usable terminal.");
    println!("--read-only disables deletion in interactive mode.");
    println!("--rmrf deletes every entry below PATH, keeping PATH itself, without a terminal");
    println!("interface. PATH is required, must not be a symlink, and must not be / , your");
    println!("home directory, or a parent of the current directory. It asks for confirmation");
    println!("on a terminal; --yes skips that and is required when stdin is not a terminal.");
}

fn run_rmrf(path: PathBuf, assume_yes: bool) -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "interactive")]
    {
        rmrf::run(path, assume_yes)
    }
    #[cfg(not(feature = "interactive"))]
    {
        let _ = (path, assume_yes);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "--rmrf support is disabled in this summary-only build",
        )
        .into())
    }
}

fn run_interactive(path: PathBuf, read_only: bool, apparent: bool) -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "interactive")]
    {
        browser::run(path, read_only, apparent)
    }
    #[cfg(not(feature = "interactive"))]
    {
        let _ = (path, read_only, apparent);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive support is disabled in this summary-only build",
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::{automatic_mode_uses_tui, write_summary_entry};
    use fdu_core::ExclusionReason;
    use fdu_scan::TopLevelDirectory;
    use std::ffi::OsString;

    #[test]
    fn summary_keeps_regular_row_format_and_names_excluded_boundaries() {
        let mut output = Vec::new();
        write_summary_entry(
            &mut output,
            &TopLevelDirectory { name: OsString::from("included"), disk_size: 42, exclusion: None },
        ).unwrap();
        write_summary_entry(
            &mut output,
            &TopLevelDirectory {
                name: OsString::from("mounted"),
                disk_size: 0,
                exclusion: Some(ExclusionReason::MountBoundary),
            },
        ).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "42\t\"included\"\nMOUNT BOUNDARY\t\"mounted\"\n");
    }

    #[test]
    fn automatic_mode_uses_tui_only_when_the_build_supports_it() {
        assert!(!automatic_mode_uses_tui(false));
        assert_eq!(
            automatic_mode_uses_tui(true),
            cfg!(feature = "interactive"),
        );
    }
}
