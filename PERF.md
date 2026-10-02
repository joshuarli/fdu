# Performance handoff

## Current scope

FDU scans a Linux 6.0+ directory tree and reports one row for each immediate child directory. Each size sums the non-directory entries below that directory; directory inode metadata is not added. Root-level files are excluded. Rows are sorted from largest to smallest; each row contains a raw byte count, a tab, and the directory name.

The scan assumes a single filesystem. It does not detect or stop at mount boundaries. Symlinks are counted without following them, and hardlinked paths are counted separately. Allocated bytes are the default; apparent mode uses logical lengths. Per-entry failures omit that entry and increment the incomplete-total warning.

The scanner returns aggregate sizes only. It does not retain or render a full tree.

## Implementation state

- Linux 6.0 or newer; direct Rust dependencies are libc and Rayon.
- Four Rayon workers by default. RAYON_NUM_THREADS remains an explicit override.
- Directory entries are read with getdents64 into a 512 KiB thread-local buffer. Names stay inline for small directories and promote to a byte vector when needed. Parsed entry vectors use a bounded per-thread pool.
- The first getdents64 batch estimates directory width. Below 128 estimated entries, known non-directory entries are statted while their names still borrow the read buffer; only directories and unknown types are retained. Wider directories retain file records so Rayon can parallelize their metadata calls.
- File sizes use narrow statx requests and count each hardlinked path independently. On x86-64, statx, getdents64, and openat2 use the Linux syscall ABI directly; other Linux architectures use libc wrappers.
- Child directories use parent-relative openat2 with O_NOFOLLOW and fall back to openat when openat2 is unavailable or denied.
- On ext4, the walker recognizes HTree end-of-directory cookies and avoids the trailing empty getdents64 call. Other filesystems use read-until-zero.
- Deep trees switch to an explicit directory stack. Interrupted filesystem calls retry. The process raises its soft file limit on EMFILE and retries.

## Release and profiling builds

The release profile uses thin LTO and incremental compilation. Cargo's default codegen-unit count is left unset. The profiling profile inherits these settings and adds symbols.

    cargo build --locked --release
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --locked --profile profiling

## Repeatable inode-heavy workload

Generate a fresh fixture on the filesystem to be measured. The generated directory is ignored by Git.

    python3 scripts/generate_inode_fixture.py perf/fixture
    target/release/fdu perf/fixture
    target/release/fdu --apparent perf/fixture

The default contains 85,772 directories and 1,218,248 tiny files, or 1,304,020 entries. Four top-level directories each contain 30,000 files to exercise large directory reads. Eight chains each add 128 nested directory levels to exercise the iterative scanner. About one in sixteen files contains one, four, or eight bytes; the rest are empty. A local release run took about 0.26 seconds, so 78 repetitions produce a batch near 20 seconds. Adjust the repetition count after a warm single scan on the target filesystem. For a larger scan, use explicit counts such as 128 top-level directories, 32 subdirectories per top-level directory, and 512 files per subdirectory (2,097,152 files total). Ensure the target filesystem has enough free inodes before choosing large counts.

The integration test builds a small instance of the same shape in a temporary directory. It verifies that only immediate directories are printed and that each size includes nested files in allocated and apparent modes.

## Capturing measurements

Store selected, human-readable captures in the tracked perf/captures directory, never under target. Keep each capture with its command, source revision, kernel, filesystem, and generator parameters. Preserve perf stat output and a perf report --stdio export; add raw perf.data only when it is useful alongside the rendered report. Do not commit generated fixture trees or full scan output.

Build the profiling executable, set workload_dir to the generated fixture, then capture filesystem counters for a single scan:

    workload_dir=perf/fixture
    mkdir -p perf/captures
    perf stat \
      -e syscalls:sys_enter_statx,syscalls:sys_enter_openat,syscalls:sys_enter_openat2 \
      -e syscalls:sys_enter_close,syscalls:sys_enter_getdents64,syscalls:sys_enter_futex \
      -e syscalls:sys_enter_sched_yield,syscalls:sys_enter_clone,context-switches \
      -o perf/captures/fdu-inode-fixture-syscalls.txt -- \
      target/profiling/fdu "$workload_dir" >/dev/null

For an on-CPU profile, use perf record with frame-pointer call stacks and write the data under perf/captures. The counter tracepoints describe call counts, not latency. Do not drop caches globally.

To collect a longer release batch, wrap repeated scans in perf stat. Keep the repetition count fixed when comparing revisions:

    RAYON_NUM_THREADS=4 perf stat -e task-clock:u,cycles:u,instructions:u -- \
      bash -c 'for run in {1..78}; do target/release/fdu "$1" >/dev/null || exit; done' \
      bash "$workload_dir"

On the generated fixture, a 78-scan release batch took 21.5 seconds. A separate syscall-count pass observed 1,218,248 statx calls, 85,772 openat2 calls, 85,777 getdents64 calls, and 85,773 closes. The strace run perturbs timing; use it for call counts only.

## Findings and next work

Earlier ext4 experiments on synthetic fixtures found that four workers preserved the filesystem-call count while producing substantially fewer futex operations than eight or sixteen workers. The four-worker default is fixed; no scheduling change is planned.

The strongest remaining counter is one statx per non-directory path. A shared inode cache had little opportunity because duplicate non-directory paths were rare. A single-thread statx-versus-fstatat probe favored the narrow statx request in instructions and cycles, while elapsed times overlapped. A single-thread io_uring statx microbenchmark reduced syscall entries but increased context switches, task-clock, and elapsed time. Keep direct statx as the default.

The ext4 EOF-cookie check reduced getdents64 calls from roughly two per opened directory to one in a changing large-tree capture. It did not establish a whole-scan time gain.

On the mixed fixture, the inline sizing path kept the statx, getdents64, openat2, and close counts unchanged. Across paired 39-scan release batches, elapsed time fell from 19.74 seconds to 18.57 seconds, and user instructions fell by about 56%. A one-scan strace pass recorded 20 futex calls, compared with 2,050 for the previous build. Traced durations are perturbed and are not compared. The 30,000-file directory path retains records for Rayon; a 100-scan single-wide fixture took 0.714 seconds, matching the previous build within measurement noise.

A trial that replaced child-directory `openat2` (`resolve = 0`) with `openat` produced byte-identical allocated and apparent output and the same statx, getdents64, close, and clone counts. The only syscall-count change was 85,772 `openat2` calls becoming 85,772 `openat` calls. Across two alternating 39-scan pairs, `openat` was slower in both orders and used about 1.9% more user cycles and 0.6% more instructions on average, so the simpler call was rejected. The four counter summaries are in `perf/captures/openat-vs-openat2-2026-10-02.txt`.

The already-cloned `rsdirstat` scanner uses a 1 MiB directory buffer. A one-scan FDU comparison on the current fixture preserved allocated and apparent output, and reduced `getdents64` calls from 85,777 to 85,773; `statx`, `openat2`, `close`, and worker counts were unchanged. Those four saved calls are under 0.005% of the total, while the larger buffer retains another 2 MiB across four workers. One elapsed-time pair was inconclusive, so FDU keeps 512 KiB. Revisit 1 MiB only for much wider flat directories where saved reads become material. The counter summary is in `perf/captures/directory-buffer-1m-2026-10-02.txt`.

## Ext4 inode-table research direction

The current live-accounting path still needs one `statx` call for each non-directory path. `getdents64` also supplies the inode number, but `read_directory` currently retains only each name and type; inode numbers are unused because hardlinked paths are counted separately. An optional ext4 experiment could retain those inode numbers and split traversal from size lookup:

1. Walk directories through the VFS, collecting names, types, and inode numbers while opening child directories relative to their parent.
2. Map the collected inode numbers through the ext4 superblock and group descriptors to inode-table offsets. Sort and deduplicate touched table blocks, then coalesce adjacent reads. Compare sparse reads of touched blocks with a sequential scan when the selected tree covers much of the filesystem.
3. Decode the selected inode fields for block accounting and logical size, then join those values back to every observed path. Preserve the current behavior of counting hardlinked paths separately and omitting directory inode sizes; compare allocated and apparent output with the live `statx` backend on a stable snapshot.

`ext4-view-rs` has the inode-location calculation in `src/inode.rs` and group/superblock parsing in `src/block_group.rs` and `src/superblock.rs`. `e2fsprogs/libext2fs` demonstrates buffered inode-table scans in `lib/ext2fs/inode.c`, especially `ext2fs_open_inode_scan` and `get_next_blocks`. These are format and algorithm references; FDU keeps its direct dependency set at libc and Rayon.

The generic Linux scanners converge on `getdents64`, `d_type`, parent-relative directory handles, and narrow `statx` requests, which FDU already uses. Their inode-number use often supports hardlink deduplication; do not copy that accounting here, where each path is counted separately. `rsdirstat`'s `crates/linux/src/scan.rs` is a useful reference for the directory-walk half, while an ext4-specific backend would need `d_ino` only for the on-disk lookup join.

This remains an experimental backend, not a replacement for live `statx`. The kernel can report delayed allocations and ext4 inline-data block usage that are not represented by a raw on-disk inode read. Correct decoding must also handle `huge_file` block counts, inode sizes, group descriptors, and filesystem feature flags. A valid comparison therefore needs a read-only snapshot, explicit raw-device access, and path-by-path parity checks before any user-visible accounting contract changes. See the [ext4 inode format documentation](https://docs.kernel.org/filesystems/ext4/inodes.html) and the Linux [`ext4_file_getattr`](https://github.com/torvalds/linux/blob/v6.18/fs/ext4/inode.c) implementation.

`FS_IOC_GETFSMAP` is not an ext4 inode-to-size index: ext4 does not report file extents through that interface. Drop it from the ext4 optimization candidates; the [ext4 GETFSMAP implementation](https://github.com/torvalds/linux/blob/v6.18/fs/ext4/ioctl.c) documents the limitation.

For one-shot scans, BPF is useful for observing kernel work, but it does not replace directory traversal or load inode sizes without another source. A persistent index is a separate architecture: fanotify could invalidate cached directory/inode records, but whole-mount or filesystem marks require `CAP_SYS_ADMIN` and queue overflow requires reconciliation. Keep this outside the current scanner until repeated-query workloads justify persistent state. See [`fanotify_mark(2)`](https://man7.org/linux/man-pages/man2/fanotify_mark.2.html) and [`fanotify(7)`](https://man7.org/linux/man-pages/man7/fanotify.7.html).

Continue with the generated fixture, keep the four-worker and two-dependency boundaries, and report counters separately from elapsed time. In the live backend, each non-directory path still needs one `statx` lookup for allocated or apparent size accounting.
