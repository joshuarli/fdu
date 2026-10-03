//! Directory size scanning with a platform-selected filesystem backend.

use std::io;
use std::path::PathBuf;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("fdu supports Linux and macOS");

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// Selects a directory tree and size mode for a scan.
pub struct ScanOptions {
    pub path: PathBuf,
    pub apparent: bool,
}

/// One immediate child directory and the non-directory bytes below it.
pub struct TopLevelDirectory {
    pub name: std::ffi::OsString,
    pub disk_size: u64,
}

/// Aggregate scan results and the number of entries omitted after errors.
pub struct ScanReport {
    pub directories: Vec<TopLevelDirectory>,
    pub skipped_entries: usize,
    pub mount_boundaries: usize,
}

/// Scan each immediate child directory without following discovered symlinks.
pub fn scan(options: &ScanOptions) -> io::Result<ScanReport> {
    #[cfg(target_os = "linux")]
    {
        linux::scan(&options.path, options.apparent)
    }
    #[cfg(target_os = "macos")]
    {
        macos::scan(&options.path, options.apparent)
    }
}
