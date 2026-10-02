# Performance handoff

## Current scope

FDU scans a Linux 6.0+ directory tree and reports one row for each immediate child directory. Each size sums the non-directory entries below that directory; directory inode metadata is not added. Root-level files are excluded. Rows are sorted from largest to smallest; each row contains a raw byte count, a tab, and the directory name.

The scan assumes a single filesystem. It does not detect or stop at mount boundaries. Symlinks are counted without following them, and hardlinked paths are counted separately. Allocated bytes are the default; apparent mode uses logical lengths. Per-entry failures omit that entry and increment the incomplete-total warning.

The scanner returns aggregate sizes only. It does not retain or render a full tree.

## Implementation state

- Linux 6.0 or newer; direct Rust dependencies are libc and Rayon.
- Four Rayon workers by default. RAYON_NUM_THREADS remains an explicit override.
- Directory entries are read with getdents64 into a 512 KiB thread-local buffer. Names stay inline for small directories and promote to a byte vector when needed. Parsed entry vectors use a bounded per-thread pool.
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

The ext4 EOF-cookie check reduced getdents64 calls from roughly two per opened directory to one in a changing large-tree capture. It did not establish a whole-scan time gain. The host and workload varied during those runs, so use the generated fixture for future comparisons.

Raw inode-table reads remain a separate research direction. Live ext4 in-memory allocation state can differ from on-disk inode values; any such experiment needs a read-only snapshot and a separate accounting contract.

This work is paused. Resume with the generated fixture, keep the four-worker and two-dependency boundaries, and report counters separately from elapsed time.
