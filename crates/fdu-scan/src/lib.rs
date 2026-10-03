use std::io;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::sync::Arc;

pub use fdu_core::{DirectoryToken, EntryBatch, FileIdentity, ScanEvent};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("fdu supports Linux and macOS");

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// Selects the path and size representation used by the summary scanner.
pub struct ScanOptions {
    pub path: PathBuf,
    pub apparent: bool,
}

/// One immediate child directory and the non-directory bytes below it.
pub struct TopLevelDirectory {
    pub name: std::ffi::OsString,
    pub disk_size: u64,
    pub exclusion: Option<fdu_core::ExclusionReason>,
}

/// Aggregate summary results and counts of omitted work.
pub struct ScanReport {
    pub directories: Vec<TopLevelDirectory>,
    pub skipped_entries: usize,
    pub mount_boundaries: usize,
    pub unsupported_aliases: usize,
}

#[derive(Clone)]
pub struct RootAnchor {
    file: Arc<File>,
    pub name: Vec<u8>,
    pub identity: FileIdentity,
    pub link_count: u64,
}

impl RootAnchor {
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }

    pub fn try_clone_fd(&self) -> io::Result<OwnedFd> {
        use std::os::fd::IntoRawFd;
        let clone = self.file.try_clone()?;
        let raw = clone.into_raw_fd();
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

#[derive(Clone)]
pub struct ScanQueueMetrics {
    capacity: usize,
    enabled: bool,
    outstanding: Arc<std::sync::atomic::AtomicUsize>,
    high_water: Arc<std::sync::atomic::AtomicUsize>,
}

impl Default for ScanQueueMetrics {
    fn default() -> Self {
        Self::new(0, false)
    }
}

impl ScanQueueMetrics {
    pub fn new(capacity: usize, enabled: bool) -> Self {
        Self {
            capacity,
            enabled,
            outstanding: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            high_water: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn event_queue_high_water(&self) -> usize {
        self.high_water.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn send(&self, sender: &SyncSender<ScanEvent>, event: ScanEvent) -> Result<(), ScanEvent> {
        if self.enabled {
            let outstanding = self.outstanding.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            self.high_water
                .fetch_max(outstanding.min(self.capacity), std::sync::atomic::Ordering::Relaxed);
        }
        match sender.send(event) {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.enabled {
                    self.decrement_outstanding();
                }
                Err(error.0)
            }
        }
    }

    /// Records that the indexed-scan consumer has removed an event from the bounded queue.
    pub fn event_received(&self) {
        if self.enabled {
            self.decrement_outstanding();
        }
    }

    fn decrement_outstanding(&self) {
        let mut current = self.outstanding.load(std::sync::atomic::Ordering::Relaxed);
        while current != 0 {
            match self.outstanding.compare_exchange_weak(
                current,
                current - 1,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(updated) => current = updated,
            }
        }
    }
}

pub fn open_root(path: &std::path::Path) -> io::Result<RootAnchor> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    let link_count = metadata.nlink();
    Ok(RootAnchor {
        file: Arc::new(file),
        name: path.as_os_str().as_bytes().to_vec(),
        identity,
        link_count,
    })
}

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

/// Starts an indexed scan on a worker thread. The supplied sender should be bounded.
pub fn start_indexed_scan(
    root: RootAnchor,
    sender: SyncSender<ScanEvent>,
    returned_batches: Receiver<EntryBatch>,
    cancelled: Arc<AtomicBool>,
) -> JoinHandle<()> {
    start_indexed_scan_with_metrics(root, sender, returned_batches, cancelled, ScanQueueMetrics::default())
}

/// Starts an indexed scan with optional event-queue instrumentation.
pub fn start_indexed_scan_with_metrics(
    root: RootAnchor,
    sender: SyncSender<ScanEvent>,
    returned_batches: Receiver<EntryBatch>,
    cancelled: Arc<AtomicBool>,
    metrics: ScanQueueMetrics,
) -> JoinHandle<()> {
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        macos::scan_indexed(&root, sender, returned_batches, cancelled, metrics);
        #[cfg(target_os = "linux")]
        {
            let _ = root;
            let _ = returned_batches;
            let _ = cancelled;
            let _ = metrics.send(
                &sender,
                ScanEvent::Failed {
                    message: "indexed scanning is unavailable in this Linux build".to_owned(),
                },
            );
            let _ = metrics.send(&sender, ScanEvent::Finished);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::ScanQueueMetrics;
    use fdu_core::ScanEvent;
    use std::sync::mpsc;

    #[test]
    fn event_queue_instrumentation_reports_the_bounded_high_water_mark() {
        let metrics = ScanQueueMetrics::new(2, true);
        let (sender, receiver) = mpsc::sync_channel(2);
        metrics.send(&sender, ScanEvent::Cancelled).unwrap();
        metrics.send(&sender, ScanEvent::Finished).unwrap();
        assert_eq!(metrics.event_queue_high_water(), 2);
        receiver.recv().unwrap();
        metrics.event_received();
        receiver.recv().unwrap();
        metrics.event_received();
        assert_eq!(metrics.event_queue_high_water(), 2);
    }
}
