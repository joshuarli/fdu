use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

// Successful Rust allocator requests measure heap churn independently of resident memory.
// The counters add synchronization overhead, so this allocator is enabled only for measurement.
struct ProfiledAllocator {
    allocation_calls: AtomicU64,
    reallocation_calls: AtomicU64,
    deallocation_calls: AtomicU64,
    requested_bytes: AtomicU64,
    live_bytes: AtomicU64,
    peak_live_bytes: AtomicU64,
}

#[global_allocator]
static ALLOCATOR: ProfiledAllocator = ProfiledAllocator::new();

#[derive(Debug)]
struct AllocationSnapshot {
    allocation_calls: u64,
    reallocation_calls: u64,
    deallocation_calls: u64,
    requested_bytes: u64,
    live_bytes: u64,
    peak_live_bytes: u64,
}

impl ProfiledAllocator {
    const fn new() -> Self {
        Self {
            allocation_calls: AtomicU64::new(0),
            reallocation_calls: AtomicU64::new(0),
            deallocation_calls: AtomicU64::new(0),
            requested_bytes: AtomicU64::new(0),
            live_bytes: AtomicU64::new(0),
            peak_live_bytes: AtomicU64::new(0),
        }
    }

    fn add_live_bytes(&self, bytes: u64) {
        let live = self.live_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak_live_bytes.fetch_max(live, Ordering::Relaxed);
    }

    fn record_allocation(&self, bytes: usize) {
        self.allocation_calls.fetch_add(1, Ordering::Relaxed);
        self.requested_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.add_live_bytes(bytes as u64);
    }

    fn snapshot(&self) -> AllocationSnapshot {
        AllocationSnapshot {
            allocation_calls: self.allocation_calls.load(Ordering::Relaxed),
            reallocation_calls: self.reallocation_calls.load(Ordering::Relaxed),
            deallocation_calls: self.deallocation_calls.load(Ordering::Relaxed),
            requested_bytes: self.requested_bytes.load(Ordering::Relaxed),
            live_bytes: self.live_bytes.load(Ordering::Relaxed),
            peak_live_bytes: self.peak_live_bytes.load(Ordering::Relaxed),
        }
    }
}

// The underlying allocator receives each pointer and layout unchanged; counters never allocate.
unsafe impl GlobalAlloc for ProfiledAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            self.record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            self.record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        self.deallocation_calls.fetch_add(1, Ordering::Relaxed);
        self.live_bytes.fetch_sub(layout.size() as u64, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() {
            self.reallocation_calls.fetch_add(1, Ordering::Relaxed);
            // Count the full successful resize request, rather than only its size difference.
            self.requested_bytes.fetch_add(new_size as u64, Ordering::Relaxed);
            if new_size >= layout.size() {
                self.add_live_bytes((new_size - layout.size()) as u64);
            } else {
                self.live_bytes.fetch_sub((layout.size() - new_size) as u64, Ordering::Relaxed);
            }
        }
        resized
    }
}

pub(super) fn report() {
    // Snapshot after scanning and rendering, before reporting can create its own allocations.
    let stats = ALLOCATOR.snapshot();
    let _ = writeln!(
        io::stderr().lock(),
        "fdu-allocations allocation_calls={} reallocation_calls={} deallocation_calls={} requested_bytes={} live_bytes={} peak_live_bytes={}",
        stats.allocation_calls,
        stats.reallocation_calls,
        stats.deallocation_calls,
        stats.requested_bytes,
        stats.live_bytes,
        stats.peak_live_bytes,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn counts_zeroed_allocations_and_resize_growth_and_shrinkage() {
        let allocator = ProfiledAllocator::new();
        let first = Layout::from_size_align(64, 8).unwrap();
        let second = Layout::from_size_align(32, 8).unwrap();
        unsafe {
            let mut pointer = allocator.alloc(first);
            let zeroed = allocator.alloc_zeroed(second);
            assert!(!pointer.is_null());
            assert!(!zeroed.is_null());
            assert!(std::slice::from_raw_parts(zeroed, second.size()).iter().all(|byte| *byte == 0));
            pointer = allocator.realloc(pointer, first, 128);
            assert!(!pointer.is_null());
            pointer = allocator.realloc(pointer, Layout::from_size_align(128, 8).unwrap(), 16);
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, Layout::from_size_align(16, 8).unwrap());
            allocator.dealloc(zeroed, second);
        }
        let stats = allocator.snapshot();
        assert_eq!(stats.allocation_calls, 2);
        assert_eq!(stats.reallocation_calls, 2);
        assert_eq!(stats.deallocation_calls, 2);
        assert_eq!(stats.requested_bytes, 240);
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.peak_live_bytes, 160);
    }

    #[test]
    fn measures_simultaneously_live_allocations_across_threads() {
        let allocator = ProfiledAllocator::new();
        let allocated = Barrier::new(5);
        let release = Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let layout = Layout::from_size_align(16 * 1024, 8).unwrap();
                    unsafe {
                        let pointer = allocator.alloc(layout);
                        assert!(!pointer.is_null());
                        allocated.wait();
                        release.wait();
                        allocator.dealloc(pointer, layout);
                    }
                });
            }
            allocated.wait();
            let stats = allocator.snapshot();
            assert_eq!(stats.live_bytes, 64 * 1024);
            assert_eq!(stats.peak_live_bytes, 64 * 1024);
            release.wait();
        });
        let stats = allocator.snapshot();
        assert_eq!(stats.allocation_calls, 4);
        assert_eq!(stats.deallocation_calls, 4);
        assert_eq!(stats.requested_bytes, 64 * 1024);
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.peak_live_bytes, 64 * 1024);
    }
}
