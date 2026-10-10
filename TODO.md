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

## Adaptive deletion thread count: built, opt-in, not yet a win

`FDU_DELETE_THREADS=adaptive` runs the controller in `crates/fdu-delete/src/adaptive.rs`: workers share the Rayon pool (built at 32 threads) and a gate limits how many are inside a filesystem call, so a controller thread can park and unpark them by moving the limit. The default stays a fixed 16.

Measured on a quiet 4 vCPU ext4 host, interleaved runs, whole-fixture deletion:

| tree | load | adaptive | 16 threads | 32 threads |
|---|---|---|---|---|
| 192 thousand entries (5 runs) | idle | 2.63 s | 2.54 s | 2.43 s |
| 192 thousand entries (3 runs) | 8 parallel `dd oflag=dsync` writers (journal contention) | 2.45 s | 2.64 s | 2.26 s |
| 192 thousand entries (3 runs) | 28 CPU-bound processes | 4.74 s | 3.49 s | 3.43 s |
| 1.5 million entries (3 runs) | idle | 21.3 s | 21.2 s | 19.9 s |

It matches 16 threads when idle, beat it in all three paired runs under journal contention (by 5% to 11%, still short of 32), and loses under CPU-bound load, because it starts at 4 and a 5 s deletion ends before it climbs. On this host a fixed 32 was the best or tied for best in every row, which is not what the 16-thread default was chosen on (see `DEFAULT_THREADS`); that sweep was on a different, busier host and should be repeated before changing the default. It also never finds the 7% that 32 gives on the large tree, since that is under its 10% noise threshold. A window varies by tens of percent on its own, as the mix of cheap file removals and slow directory removals shifts, so neighboring thread counts cannot be told apart in a trial of about a second.

Open questions before making it the default:

- CPU-bound load makes more threads faster, because more runnable threads win more CPU time, which is the opposite of the busy host the 16-thread default was chosen on (load average 20 to 40, where 8 to 16 beat 32). Find a load that reproduces that host, and a deletion long enough (millions of entries) to converge under it. Without that there is nothing to show the controller helping.
- Try starting at 16 and probing 8 and 32 from there, so a short run costs nothing. The TODO's original constraint was to start small; the numbers above say that is what loses on short runs.
- Longer trials or paired (interleaved) comparisons of neighboring counts would cut the noise, at the cost of converging more slowly.
