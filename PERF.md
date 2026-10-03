# Performance goals and measurements

## Current scope

FDU scans Linux 6.0+ and macOS directory trees and reports one row for each immediate child directory. Each size sums the non-directory entries below that directory; directory inode metadata is not added. Root-level files are excluded. Rows are sorted from largest to smallest; each row contains a raw byte count, a tab, and the directory name. Performance goals and captures in this document apply to the Linux backend.

The supplied root path is opened normally, so symlinks in that path resolve. Discovered symlinks are counted without following them, and hardlinked paths are counted separately. Allocated bytes are the default; apparent mode uses logical lengths. FDU excludes child directories on another filesystem or mount; the library report counts those boundaries separately from entries skipped after I/O or metadata errors.

The scanner returns aggregate sizes only. It does not retain or render a full tree.

## Quantified optimization goal

The optimization objective is: **double FDU's throughput on the default ext4 fixture, halve CPU work and allocation traffic, and reduce peak memory while preserving its current accounting, four default Rayon workers, and libc-plus-Rayon dependency boundary.** The targets below define success; they are aspirations, not measured improvements.

Use the default `scripts/generate_inode_fixture.py` tree: 1,304,020 entries, including 85,772 directories and 1,218,248 files. The reference scanner is `bd507f7`, whose scanning code is unchanged in `baeb0af`. Timing and CPU baselines below sum two 39-scan release batches. Memory baselines use five fresh processes per size mode. Source, commands, and binary hashes are recorded in `perf/captures/fdu-goal-baseline-2026-10-02.txt` and `perf/captures/fdu-memory-baseline-2026-10-02.json`.

| Metric | Current baseline | Aspirational target |
| --- | --- | --- |
| Allocated-mode elapsed time, 78 scans | 19.11 s, about 5.32 million entries/s | At most 9.5 s, about 10.71 million entries/s, and at most 50% of the paired baseline |
| Allocated-mode task-clock, 78 scans | 71.91 CPU-s | At most 36 CPU-s and at most 50% of the paired baseline |
| Allocated-mode user instructions, 78 scans | 31.69 billion, about 406 million/scan | At most 15.6 billion, or 200 million/scan |
| Successful allocation plus reallocation calls/scan, median | 2,089 allocated; 2,149 apparent | At most 1,000 in each mode |
| Cumulative requested Rust heap bytes/scan, median | 12.09 MiB allocated; 12.10 MiB apparent | At most 6 MiB in every memory sample, in each mode |
| Peak live requested Rust heap bytes/scan, median | 3.59 MiB allocated; 4.57 MiB apparent | At most 2 MiB in every memory sample, in each mode |
| Process peak RSS/scan, median | 2,248 KiB allocated; 2,868 KiB apparent | At least 25% below each mode's paired baseline: currently at most 1,686 KiB and 2,151 KiB respectively |

These absolute budgets apply to the reference host and fixture. On a different host, establish a fresh reference measurement and retain the relative throughput, CPU, and RSS targets. Report median and maximum memory measurements; the candidate's worst observed RSS must also stay at or below the matched reference's worst observed RSS. The five reference samples reached 2,836 KiB in allocated mode and 3,656 KiB in apparent mode.

Correctness and scope are hard requirements. Preserve the same immediate-directory-to-byte-count mapping in allocated and apparent modes, descending size order, root-file exclusion, hardlink path accounting, symlink behavior, and incomplete-total diagnostics. Preserve the existing behavior checks for sparse files, unusual names, large directory batches, interrupted calls, and deep trees. Apparent-mode elapsed time and task-clock must not regress by more than 5% against its paired reference.

Retain one file-metadata operation per non-directory path or fewer, and no extra child-directory opens. The reference counts are 1,218,248 metadata operations and 85,772 child opens. Account for equivalent operations submitted through other interfaces. Directory reads may rise by at most 1% above 85,777 if a smaller buffer helps meet the CPU and memory targets. Primary success is measured on the ordinary-user live VFS backend; snapshot experiments must establish the same accounting and access contract before qualifying.

The goal is achieved when all targets and requirements pass the validation procedure below and the implementation, captures, and interpretation are committed. Partial gains or exhausted experiments should be reported with their measured gap to the targets rather than described as completion.

Work is paused at the user's request. The settled implementation meets the measured memory and user-instruction budgets; throughput and total CPU targets remain unmet. The investigation handoff below records the remaining checks and hypotheses.

## Implementation state

- Linux 6.0 or newer; Linux uses libc and Rayon. The macOS backend uses libc and has no performance target in this document.
- Four Rayon workers by default. RAYON_NUM_THREADS remains an explicit override.
- Size accounting is selected before traversal. `scan_mode` specializes the recursive and iterative walks so each metadata call has a fixed request mask and size field.
- Name validation stays inside each directory record and ends at its first NUL. `directory_record_name` uses bounded SSE2 comparisons on x86-64, with scalar handling for short tails and other architectures. Bytes after the terminator are padding and are not validated as part of the name.
- Directory entries are read with getdents64 into a reusable 64 KiB buffer. `DirectoryBuffer` leases storage from TLS, so a worker can enumerate another directory while a metadata batch yields to Rayon. Each thread retains at most one enumeration buffer.
- The first getdents64 batch estimates directory width. Below 128 estimated entries, known non-directory entries are statted directly during enumeration. Wider directories parallelize metadata calls over records for the current read batch; those records borrow the buffer's name bytes and finish before the next read. Only directories and unknown types remain in `DirectoryEntries` after enumeration.
- Serial versus parallel metadata work is selected once per batch. `parse_directory_batch` checks each record's bounds before reading its name, and completes any borrowed-name metadata work before the enumeration buffer is reused.
- Names stay inline for short name lists. `DirectoryEntryStorage` keeps up to two child records inline; larger lists use a bounded per-thread vector pool. Directories with no retained children return their already-computed subtotal without an empty Rayon reduction.
- File sizes use narrow statx requests and count each hardlinked path independently. On x86-64, statx, getdents64, and openat2 use the Linux syscall ABI directly; other Linux architectures use libc wrappers.
- Child directories use parent-relative openat2 with O_NOFOLLOW and RESOLVE_NO_XDEV, falling back to openat plus descriptor mount-ID checks when openat2 is unavailable or denied.
- On ext4, the walker recognizes HTree end-of-directory cookies and avoids the trailing empty getdents64 call. Other filesystems use read-until-zero.
- Deep trees switch to an explicit directory stack. Interrupted filesystem calls retry. The process raises its soft file limit on EMFILE and retries.

The macOS backend uses `fdopendir`/`readdir`, no-follow `fstatat`, and no-follow `openat`. It checks opened child descriptors with `fstat` and `fstatfs` before enumeration and uses a heap-backed path stack to keep descriptor use bounded on deep trees. Those checks compare device, filesystem ID, and mount point, but they are not atomic with path or mount changes; this backend makes no Linux-style atomic resolution guarantee. No APFS-specific optimization is used.

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

### Throughput and CPU validation

Keep baseline and candidate release binaries with their source revision and hash. Warm each binary on the unchanged fixture, then run at least five pairs of 78 fresh-process scans in allocated mode, alternating which binary runs first. Use `RAYON_NUM_THREADS=4`, the same output destination, toolchain, and build flags. Report every pair, the median candidate/reference ratio, and the median batch time; success requires the median ratio to satisfy the target and at least four of the five individual pairs to agree. Repeat the paired comparison in apparent mode to check its 5% regression limit. Recheck results affected by a workload change or large host-load variation.

Record the host load and runnable-task count before and after each batch. Preserve interrupted or noisy experiments with their limitation stated; changing host load cannot establish an elapsed-time or CPU improvement merely because the runs alternate. Allocation and instruction counts can still be reported separately when their distributions remain stable.

To collect a longer release batch, wrap repeated scans in perf stat. Keep the repetition count fixed when comparing revisions:

    RAYON_NUM_THREADS=4 perf stat -e task-clock:u,cycles:u,instructions:u -- \
      bash -c 'for run in {1..78}; do target/release/fdu "$1" >/dev/null || exit; done' \
      bash "$workload_dir"

An earlier 78-scan release batch on the generated fixture took 21.5 seconds. Its syscall-count pass observed 1,218,248 statx calls, 85,772 openat2 calls, 85,777 getdents64 calls, and 85,773 closes. The strace run perturbs timing; use it for call counts only.

### Allocation and resident-memory validation

`allocation-profile` enables `src/allocation_profile.rs`, a wrapper around Rust's `System` allocator. Build it in a separate target directory so timing captures continue to use the ordinary release executable:

    CARGO_TARGET_DIR=target/allocation-profile cargo build --locked --release --features allocation-profile

After scanning and rendering, the instrumented executable emits one `fdu-allocations` line to stderr. It reports successful allocation calls, successful reallocations, deallocations, cumulative requested bytes, current live requested bytes, and peak live requested bytes. Allocation calls include zeroed allocations; the goal's request count is allocation calls plus reallocations. Each successful resize adds its full new requested size to cumulative bytes. For example, a 64-byte allocation resized to 128 bytes contributes 192 cumulative requested bytes and 128 live bytes. Failed requests do not increment these counters.

The allocator counters cover Rust heap requests across startup, traversal, and output, through the point just before the profiling diagnostic. They exclude allocator rounding and bookkeeping, libc's own allocations, thread stacks, and other mappings. A resize's temporary internal copy is also outside the peak-live calculation. Nonzero live bytes at the end can include idle Rayon workers and their retained buffers; they alone do not establish a leak. Counts can vary with worker assignment and buffer reuse, so preserve the sample distribution.

Measure RSS with the uninstrumented release executable. `/usr/bin/time -v` reports the process's peak resident memory in KiB, covering touched heap pages, stacks, code, and other resident mappings. It measures a different quantity from requested heap capacity: reserved or untouched heap bytes need not be resident. Atomic allocation counters add overhead, so instrumented timings do not qualify for throughput or CPU targets.

Warm each binary once, then record five fresh processes per mode. Run the same procedure on reference and candidate binaries, alternating order. For allocated mode:

    for run in 1 2 3 4 5; do
      /usr/bin/time -v -o "perf/captures/rss-$run.txt" \
        env RAYON_NUM_THREADS=4 target/release/fdu perf/fixture >/dev/null
      env RAYON_NUM_THREADS=4 target/allocation-profile/release/fdu perf/fixture \
        >/dev/null 2>"perf/captures/allocations-$run.txt"
    done

Repeat with `--apparent` and separate capture names. Verify successful exit and absence of incomplete-total warnings; compare the directory-size mappings before accepting memory results. Summarize allocation requests, cumulative requested bytes, peak live bytes, and RSS using both median and maximum. Preserve individual samples, source revisions, binary hashes, kernel, filesystem, generator arguments, mode, and worker count. Use the same instrumentation and release flags on both sides of an allocation comparison.

## Findings and next work

### Investigation handoff

Work is settled and paused. The candidate keeps direct statx, four Rayon workers, the existing accounting and error behavior, and the libc-plus-Rayon dependencies. The accepted changes are bounded streaming metadata batches and inline child storage, followed by bounded name validation and constant accounting/batch selection. Experimental metadata interfaces and inode sorting were reverted.

| Completion requirement | Settled evidence | Remaining work |
| --- | --- | --- |
| Allocated user instructions | All five 78-scan batches at or below 15.331 billion, below 15.6 billion | Recheck after any further scanner change |
| Allocation requests and requested heap | Medians 869 allocated / 834 apparent; maximum requested heap 2.55 MiB and peak live heap 0.61 MiB | Retain five fresh samples per mode after changes |
| RSS | Medians 852 KiB allocated / 832 KiB apparent; maxima 860 / 1,088 KiB; all paired and absolute budgets met | Retain paired reference and worst-sample checks |
| Throughput and total CPU | Targets unmet; latest allocated medians 22.86 s and 86.93 CPU-s per 78 scans on a variable-load host | Establish at most 9.5 s and 36 CPU-s, each at most 50% of paired reference, on a stable host |
| Apparent-mode regression guard | Four of five recheck pairs were within 5%; one exceeded 9% during changing host load | Recheck elapsed time and task-clock under stable load before accepting the guard |
| Accounting and operation bounds | Both size mappings match the reference; metadata/open counts unchanged; directory reads up 0.061% | Preserve all invariants in subsequent candidates |

The last source validation passed `cargo test --locked` (12 unit tests and one CLI integration test), `cargo test --locked --all-features` (14 unit tests and one CLI integration test), the ordinary release build, and the separate allocation-profile release build. Full-fixture comparisons checked both directory-size mappings, descending order, successful exit, and absence of incomplete-total warnings. No formatter, linter, commit hook, or remote push was run.

Resume in this order:

1. Establish a stable host interval before changing the scanner. Reproduce five alternating pairs of 78 fresh-process scans in both modes, recording load and runnable tasks around each batch. Use the same unchanged ext4 fixture, ordinary-user permissions, four workers, warm caches, output destination, compiler, and release flags. Check the apparent-mode guard first; preserve noisy pairs without treating their ratios as a gain. Use the throughput and memory procedures above for the complete acceptance gate.
2. Investigate kernel metadata cost with the precise profile in `perf/captures/vector-precise-profile-2026-10-02.txt`. Pathname lookup and VFS/ext4 attributes dominate its samples. A next prototype needs a concrete hypothesis about reducing full-scanner total CPU, rather than merely reducing user instructions or syscall entries. Compare supported metadata APIs or batching only when the design preserves parent-directory handle lifetime, file-descriptor bounds, symlink behavior, requested-field availability, no-follow/dont-sync semantics, overflow handling, and omission of failed entries or subtrees.
3. Keep rejected trials as evidence: raw newfstatat, inode sorting, and io_uring with bounded workers did not establish a qualifying improvement. Do not repeat them unchanged. The direct-call microbenchmark used less than half the CPU of bounded io-wq; an asynchronous prototype needs a different, testable reason to improve whole-scan work.
4. Require every original target before declaring completion. A snapshot, raw inode reader, or persistent index needs an explicit accounting and access contract and cannot qualify the ordinary-user live-VFS goal by assumption. Keep the four-worker and dependency boundaries; consult the user before adding any dependency.

The full validation JSON captures contain reference/candidate commands, binary hashes, source-file hashes, raw perf counters, and raw RSS reports. They identify the reference as `9b4bd1e` and the candidate as the `c8e6cd8` worktree settled in this commit; `src/linux.rs` SHA-256 is `da1fd0a03de9491d264d043959e61f2d748e13107cfb0e3656fd127ac4333981`. Local immutable binaries and rejected trial sources are retained outside Git in `/home/josh/d/fdu-perf-local-archive/streaming-20261002`: `fdu-reference`, `fdu-reference-allocations`, `fdu-vector-final`, and `fdu-vector-final-allocations`. The archive's `measure.py` is a local capture helper, not a repository interface; the documented commands above remain the reproducible measurement procedure. Do not run measurements concurrently with compilation, profiling, or another benchmark.

### Bounded vector validation and constant traversal modes

The name parser finds the first NUL or path separator using a single vector mask on x86-64. It reads only inside the current record, rejects separators before the terminator, and permits arbitrary bytes in the name and padding. Size accounting, root-only filtering, and serial versus parallel metadata work are selected outside the per-entry loop. The accounting tests cover both size modes, including iterative descent; isolated parser tests cover every name length through 255 bytes, raw non-UTF-8 bytes, separators, padding, and every truncated prefix of a synthetic directory batch.

Short paired probes brought allocated-mode instructions below 200 million per scan. The parser-selection comparisons and raw counters are in `perf/captures/parser-selection-2026-10-02.json`. The first five paired 78-scan runs used at most 15.331 billion allocated-mode user instructions, below the 15.6-billion budget. Allocation-request medians were 869 allocated and 834 apparent; requested heap stayed below 2.55 MiB, peak live heap below 0.61 MiB, and maximum RSS at most 1,088 KiB. Every memory sample met the budgets and each mode's paired RSS reduction requirement.

Unrelated compilation saturated the host during that first validation: load average reached 38.44 on 32 logical CPUs, and later reference and candidate task-clock values rose together. The stable memory and instruction distributions are retained in `perf/captures/vector-validation-loaded-host-2026-10-02.json`; its timing results do not qualify a throughput or CPU improvement. A second complete five-pair, 78-scan comparison is recorded in `perf/captures/vector-validation-2026-10-02.json`, with host observations before and after every batch. Unrelated work fluctuated again, so that recheck also cannot establish a timing gain. Allocated-mode instructions stayed below 15.331 billion in all five batches, about 196.5 million per scan and 48.4% of the paired reference. The second capture explicitly reuses the first capture's five memory samples per mode; it is not a second independent memory set. The 2x throughput and 50% total-CPU targets remain unmet.

A precise on-CPU capture attributes most sampled work to pathname lookup, VFS attributes, and ext4 attributes. Its report and exact command are in `perf/captures/vector-precise-profile-2026-10-02.txt`. On this AMD host, `cycles:p` uses IBS rather than ordinary core-PMU sampling, whose instruction pointers can skid. This profile includes kernel and user work; it is separate from the user-only instruction counters used for the budget. See the [Linux IBS documentation](https://github.com/torvalds/linux/blob/v6.18/tools/perf/Documentation/perf-amd-ibs.txt).

### Rejected metadata interfaces and inode ordering

A full-scanner raw `newfstatat` trial preserved allocated and apparent mappings on the reference host but produced essentially unchanged total CPU and elapsed time. Inlining its size inspector reduced user instructions in one exploratory pair, while the ordinary statx interface also benefited from inlining; the interface replacement did not establish a performance gain. Native-result adaptation was already optimized away for some call sites, so the results do not establish adapter copying as the cause.

Sorting retained child directories by inode used about 6% more user instructions; additionally sorting file batches used roughly 30% more. Both saved only about 1% task-clock and failed to improve elapsed time. These trials were rejected. Their commands, binary hashes, individual paired counters, and interpretations are in `perf/captures/metadata-interface-and-ordering-2026-10-02.json`.

A follow-up io_uring microbenchmark bounded both io-wq worker classes to one. Across three alternating runs per interface, direct statx used a median 155 ms task-clock for 330,000 metadata operations, versus 361 ms with bounded io-wq and 651 ms with the default worker limit. Checksums agreed, but bounded io-wq still produced about 64,600 context switches versus four for direct calls. This is a metadata-only probe, not a full-scanner qualification, and it does not justify an asynchronous backend. The records are in `perf/captures/uring-worker-limit-2026-10-02.json`.

### Bounded streaming metadata batches

File names and records now live only for one enumeration batch. Two retained child records fit inline, and reusable buffers are leased from TLS while Rayon consumes a batch. A serial 8 KiB prototype met the memory budgets but slowed a single 30,000-file child directory from about 6.1 ms to 10.9 ms. Parallel metadata batches restored most of that throughput; 64 KiB batches measured about 5.9 ms against a 6.1 ms reference across six alternating 30-scan pairs. These small timing differences do not establish a flat-directory speedup.

Five fresh-process samples per mode met every memory target. Allocated-mode medians were 890 allocation requests, 2.51 MiB requested heap, 0.60 MiB peak live heap, and 812 KiB peak RSS. Apparent-mode medians were 827 requests, 2.49 MiB requested heap, 0.51 MiB peak live heap, and 800 KiB peak RSS. Maximum requested heap stayed below 2.61 MiB, maximum peak live heap below 0.68 MiB, and maximum RSS below the matched reference in each mode. The records, paired references, binary hashes, and measurement methods are in `perf/captures/streaming-memory-2026-10-02.json`.

The current syscall counts are 1,218,248 metadata calls, 85,772 child-directory opens, 85,829 directory reads, and 85,773 closes. The 52 additional reads are about 0.061% above the reference, within the 1% allowance. The original streaming report is in `perf/captures/streaming-syscalls-2026-10-02.txt`. The settled vector-parser candidate has identical counts, recorded in `perf/captures/vector-syscalls-2026-10-02.txt`.

Across two alternating 39-scan pairs, allocated-mode elapsed time fell from a summed 18.92 s to 18.47 s, task-clock from 71.40 CPU-s to 70.00 CPU-s, and user instructions from 31.69 billion to 28.18 billion. Apparent-mode elapsed rose about 2.8% and task-clock about 2.1%. These are exploratory samples, fewer than the five 78-scan pairs required for throughput validation. At that stage they showed a memory improvement and an 11% instruction reduction; the throughput, CPU, and instruction targets were still unmet. The later vector-parser candidate meets the instruction budget as described above. The commands and raw counters are in `perf/captures/streaming-counters-2026-10-02.json`.

The next measurements should focus on kernel metadata work. Keep the ordinary-user accounting contract and compare full-scanner total CPU cost when evaluating metadata interfaces; user-only instruction counts cannot establish a kernel-time improvement.

### Earlier experiments

Earlier ext4 experiments on synthetic fixtures found that four workers preserved the filesystem-call count while producing substantially fewer futex operations than eight or sixteen workers. The four-worker default remains fixed.

The strongest remaining counter is one statx per non-directory path. A shared inode cache had little opportunity because duplicate non-directory paths were rare. A single-thread statx-versus-fstatat probe favored the narrow statx request in instructions and cycles, while elapsed times overlapped. A single-thread io_uring statx microbenchmark reduced syscall entries but increased context switches, task-clock, and elapsed time. Keep direct statx as the default.

The ext4 EOF-cookie check reduced getdents64 calls from roughly two per opened directory to one in a changing large-tree capture. It did not establish a whole-scan time gain.

On the mixed fixture, the inline sizing path kept the statx, getdents64, openat2, and close counts unchanged. Across paired 39-scan release batches, elapsed time fell from 19.74 seconds to 18.57 seconds, and user instructions fell by about 56%. A one-scan strace pass recorded 20 futex calls, compared with 2,050 for the previous build. Traced durations are perturbed and are not compared. The 30,000-file directory path retains records for Rayon; a 100-scan single-wide fixture took 0.714 seconds, matching the previous build within measurement noise.

A trial that replaced child-directory `openat2` (`resolve = 0`) with `openat` produced byte-identical allocated and apparent output and the same statx, getdents64, close, and clone counts. The only syscall-count change was 85,772 `openat2` calls becoming 85,772 `openat` calls. Across two alternating 39-scan pairs, `openat` was slower in both orders and used about 1.9% more user cycles and 0.6% more instructions on average, so the simpler call was rejected. The four counter summaries are in `perf/captures/openat-vs-openat2-2026-10-02.txt`.

The already-cloned `rsdirstat` scanner uses a 1 MiB directory buffer. A one-scan FDU comparison against its then-current 512 KiB buffer preserved allocated and apparent output, and reduced `getdents64` calls from 85,777 to 85,773; `statx`, `openat2`, `close`, and worker counts were unchanged. Those four saved calls are under 0.005% of the total, while the larger buffer retains another 2 MiB across four workers. One elapsed-time pair was inconclusive, so that trial did not justify 1 MiB. Subsequent streaming experiments use 64 KiB batches. Revisit 1 MiB only for much wider flat directories where saved reads become material. The counter summary is in `perf/captures/directory-buffer-1m-2026-10-02.txt`.

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
