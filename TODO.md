# TODO

## Try `rustc-hash` against hashbrown's default hasher

All hash tables use `hashbrown` with its default hasher (foldhash). Keys are trusted local filesystem data, so a faster non-cryptographic hasher is acceptable.

- Benchmark `hashbrown::HashSet<FileIdentity, rustc_hash::FxBuildHasher>` (dev/ino keys) in the indexed scan's seen-directory shards in `crates/fdu-scan/src/indexed.rs`. This is the hottest table.
- Then try it for the `NodeId` sets and maps in `fdu-delete`, `fdu-tui` and `src/browser.rs`.
- Fx can cluster badly on aligned or strided IDs (inode numbers may be), so measure on a large real tree, not a synthetic loop.
- Needs a quiet host. Run it when timings are stable.
- Adopt it only if it wins on wall-clock time. Otherwise keep foldhash and add no dependency.

## Check whether `NodeId` is dense

If `NodeId`s are dense indexes into the tree, hashing may be unnecessary:

- `status_by_node: HashMap<NodeId, String>` in `src/browser.rs` could be a `Vec<Option<String>>` indexed by `NodeId`.
- The marks sets (`HashSet<NodeId>` in `src/browser.rs`, `fdu-tui` and `fdu-delete`) could be a bitset or `Vec<bool>`.

First confirm how `NodeId`s are allocated in `fdu-core` and whether they stay dense after deletions. Try this before the Fx experiment, since it could remove the hashing entirely for these collections.

## Investigate io_uring for deletion on Linux

Deletion issues one blocking `unlinkat` per entry from a 32-thread pool. On Linux, io_uring has an `unlinkat` operation with batched submission, which could cut per-call syscall overhead.

- First time the scan against the deletion on a quiet host with a large fixture. If scan dominates, or deletion is limited by the filesystem journal and directory locks rather than syscall cost, stop here.
- If syscall overhead is visible, prototype batched `unlinkat` relative to the parent directory descriptor, keeping the existing identity check before each unlink.
- Linux only; macOS has no equivalent, so the thread pool stays there.
- Needs a new dependency (an io_uring crate), so ask before adding it. Adopt it only if it wins on wall-clock time.

## Adaptive deletion thread count

The fixed pool size is 16, chosen from a sweep on the layout fixture: on an idle host 32 was slightly faster, but on a busy host (load average 20 to 40) 8 to 16 beat 32 and 64 was twice as slow. So the best size depends on host load. The busy-host differences between 8, 16 and 32 were within run-to-run noise of a few seconds, so an adaptive pool could not be validated on that host; build it only after finding a quiet host to confirm gains on, and a way to repeat measurements under controlled load.

Design constraints if it is built:

- Hill-climb on throughput (deletions per second over a window long enough to smooth journal-commit bursts), not per-call latency. Workers pull from a shared queue and a controller parks or unparks them, since rayon's pool cannot be resized.
- Ramp up slowly from a small count and stop growing as soon as throughput stops rising. A busy host must never be flooded with syscalls: when throughput falls while threads are added, or per-call latency climbs sharply, shrink.
- Treat noise as the normal case. Act on several consecutive windows, not one, and prefer a lower thread count when two counts measure within noise of each other.
- Short runs finish before the controller converges, so start from a conservative count and do not adapt below a minimum entry count.
- Keep the controller apart from the identity-check and unlink path.
