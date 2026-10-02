# Performance notes

## First fixed-four capture

Captured 2026-10-02 on the same ext4 `~/d/crabc` workload as the source scanner. The tree was changing while other work continued, so wall time and scheduler counts are not stable performance evidence. Every measured process ran as the normal user; `perf` ran through `doas`. No caches were dropped.

`fdu` sets `RAYON_NUM_THREADS` to four when the user has not set it, then lets Rayon initialize its global pool on first use. A paired syscall pass recorded four worker clones for both implementations. The filesystem syscall counts matched: 995,293 `statx`, 60,226 directory opens, 60,197 `close`, and 120,467 `getdents64`. `fdu` avoided the source CLI's extra root `newlstat`; both paths performed the same root `statx` and recursive filesystem operations.

Three alternating fixed-four captures on a later snapshot again showed matching traversal calls. The path count in the preceding user-readable inventory was 1,055,519.

| Pass | `statx` source / fdu | `getdents64` source / fdu | `futex` source / fdu | Elapsed seconds source / fdu |
| --- | ---: | ---: | ---: | ---: |
| 1 | 997,961 / 997,961 | 122,285 / 122,285 | 1,214,982 / 800,439 | 0.581 / 0.564 |
| 2 | 997,965 / 997,961 | 122,285 / 122,285 | 729,993 / 812,927 | 0.437 / 0.563 |
| 3 | 997,973 / 997,973 | 122,285 / 122,285 | 702,033 / 1,061,473 | 0.434 / 0.613 |

The mean futex counts differ by about 1%; their run-to-run spread is much larger than that. Elapsed times also vary with host load and do not establish equal throughput. The useful result so far is that the greenfield walker preserves the filesystem syscall path while removing a root metadata lookup. Rerun on a stable tree and quieter host before claiming a throughput change.

A ten-scan on-CPU profile of `fdu` recorded 7,333 samples with zero lost. `ext4_file_getattr` was 5.92% and Rayon `__lock` was 4.96% of samples. `memcpy` was 4.80%, including formatting the full plain-text tree even though stdout was redirected. Raw captures are in the ignored `target/perf-initial/` directory, including `fdu-crabc-pinned-rayon-10.data` and its report.

## Reuse directory-name storage

The dirent parser needs NUL-terminated names for `statx` and `openat2`, but the completed tree only needs each name for output. `scan_child_directory` and `file_item` now consume each `CString` buffer into an `OsString`, avoiding a second owned name allocation per path. Ordinary UTF-8 names retain their output; backslashes, control characters, and invalid UTF-8 bytes are escaped to keep each item on one line.

`write_item` uses an explicit frame stack when rendering the tree, so output depth does not consume the native call stack.

The root `statx` request now asks only for type; Linux returns the containing-device ID unconditionally. Ext4 dirents with a known non-directory `d_type` request only the selected size field. `DT_UNKNOWN` still requests type and is rejected if the returned mask omits it; all file entries are also rejected if the requested size field is absent. The [`statx(2)` documentation](https://man7.org/linux/man-pages/man2/statx.2.html) defines the mask as the caller's requested and returned fields.

On the 8,192-file ext4 flat-directory fixture, `perf stat -r 30` favored the new storage path in both run orders. In the last same-second pair, task-clock was 8.03 ms before and 5.49 ms after, instructions were 16.07 million and 12.97 million, and elapsed time was 4.196 ms and 3.429 ms. The fixture output compared byte-for-byte. This small fixture and noisy host do not establish a full-tree throughput gain. Raw captures are `target/perf-initial/fdu-name-move-before-30.txt`, `target/perf-initial/fdu-name-move-after-30.txt`, `target/perf-initial/fdu-name-move-before-30b.txt`, and `target/perf-initial/fdu-name-move-after-30b.txt`; the pre-change executable is `target/perf-initial/fdu-name-move-baseline`.

Per-entry filesystem or size-overflow errors omit that item and any unreadable subtree, but the walker counts these failures and reports the total on standard error with an incomplete-total warning. An ext4 smoke directory with one inaccessible child produced the warning and retained the readable sibling. Root-level open, enumeration, or size-total overflow failures remain fatal.

## Large-directory read buffer

The thread-local `getdents64` buffer is 512 KiB. On an ext4 directory containing 8,192 short-name files, the 128 KiB version needed three reads (3,277, 3,276, and 1,641 records); 512 KiB returned all 8,194 records, including `.` and `..`, in one read. Across 30 runs, `perf stat` recorded three versus one `getdents64` calls per scan. Task-clock and elapsed time were effectively unchanged in this fixture. A small nested directory kept the same four-call count and showed no measurable cost. On `crabc`, consecutive captures had only three `statx` calls of difference and recorded 83,892 versus 83,780 `getdents64` calls, so the larger buffer affects only a small share of this tree's directories. Both runs warned about 42 inaccessible or changed entries; wall time and scheduler counters varied, so no throughput claim follows. The larger buffer adds at most 1.5 MiB across four workers compared with 128 KiB buffers. Captures: `target/perf-initial/fdu-buffer-128k-flat-30.txt`, `target/perf-initial/fdu-buffer-512k-flat-30.txt`, `target/perf-initial/fdu-buffer-128k-small-30.txt`, `target/perf-initial/fdu-buffer-512k-small-30.txt`, `target/perf-initial/fdu-buffer-128k-flat-strace.txt`, `target/perf-initial/fdu-buffer-512k-flat-strace.txt`, `target/perf-initial/fdu-buffer-128k-crabc.txt`, `target/perf-initial/fdu-buffer-512k-crabc.txt`.

## One directory-open syscall

`open_child_directory` now uses `openat2` with `RESOLVE_NO_XDEV`. The entry name is a single directory component and `O_NOFOLLOW` prevents following a symlink. A successful open cannot cross a mount, so the child inherits its already-validated parent's device and needs no follow-up `File::metadata()` call. The [`openat2(2)` documentation](https://man7.org/linux/man-pages/man2/openat2.2.html) specifies that `RESOLVE_NO_XDEV` blocks mount crossings, including bind mounts, and returns `EXDEV` for one. On `EXDEV`, `ENOSYS`, or `EPERM`, `fdu` falls back to `openat` plus the explicit device check; this preserves traversal into same-device bind mounts and supports restricted syscall environments.

On a later `crabc` snapshot, the task trace counted 70,770 `openat2`, zero `openat`, 1,030,066 `statx`, and 141,555 `getdents64` calls with four worker clones. The paired existing walker used 70,770 `openat` calls on the same snapshot. The fast path therefore removed its per-child descriptor metadata check for all 70,770 directory opens. The host lacks a `sys_enter_fstat` tracepoint, so the removed check is established by the code path rather than a direct counter. The ten-scan profile recorded 7,643 samples with zero lost; `ext4_file_getattr` remained 6.17% of samples. Raw captures are `target/perf-initial/fdu-openat2-crabc-syscalls.txt`, `target/perf-initial/dirstat-openat2-baseline-crabc-syscalls.txt`, `target/perf-initial/fdu-crabc-openat2-10.data`, and `target/perf-initial/fdu-crabc-openat2-10.report.txt`.

After moving pool setup back to Rayon's lazy initialization, a paired pass over 1,124,714 readable paths counted 1,052,051 `statx`, 145,343 `getdents64`, and 72,663 directory opens in each scanner. `fdu` used `openat2`; the source scanner used `openat`. Futex counts were 701,146 for `fdu` and 736,416 for the source scanner, with context switches of 55,172 and 57,094. This single pair does not establish a timing win; the lower futex count is consistent with avoiding the synchronization spike from eager pool initialization, but needs repeated stable runs to isolate. The output paths differ: `fdu` renders every item as plain text, so wall and user time include more formatting work. Captures are `target/perf-initial/fdu-lazy-pool-syscalls.txt` and `target/perf-initial/dirstat-lazy-pool-syscalls.txt`.

## ext4 HTree end-of-directory cookie

Linux writes the filesystem's next directory position to each `getdents64` record's `d_off`; ext4 HTree directories set that position to a reserved EOF cookie once enumeration is complete. The [ext4 directory implementation](https://github.com/torvalds/linux/blob/v6.18/fs/ext4/dir.c) uses the 32-bit or 64-bit EOF value defined in [ext4.h](https://github.com/torvalds/linux/blob/v6.18/fs/ext4/ext4.h), and [getdents64](https://github.com/torvalds/linux/blob/v6.18/fs/readdir.c) returns that position with the final record. `fdu` detects ext4 once with `fstatfs`, then skips the otherwise empty follow-up read only when the final record carries that cookie. Directories opened through the `openat` fallback keep the conservative read-until-zero path; other filesystems do too.

A read-only probe of `crabc` found the cookie in all 62,679 opened directories. It read 62,782 batches for 1,229,128 raw dirents (including `.` and `..`), with 42 directory-open errors and no read errors. The C probe and its raw output are `target/perf-initial/ext4-eof-cookie-probe.c` and `target/perf-initial/fdu-ext4-eof-cookie-probe.txt`.

The paired `perf stat` scans ran 19 seconds apart while `crabc` was changing. The second snapshot had about 900 more child-directory open attempts, so these counts show per-directory syscall shape rather than a timing comparison.

| Scanner path | Child `openat2` attempts | `getdents64` calls | `statx` calls | `fstatfs` calls | Elapsed seconds |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before EOF-cookie check | 61,797 | 123,615 | 1,038,362 | 1 | 0.554 |
| Ext4 EOF-cookie check | 62,716 | 62,778 | 1,041,041 | 2 | 0.610 |

Dividing by child-directory `openat2` attempts plus one root read, the two snapshots recorded about 2.00 and 1.00 `getdents64` calls per directory. The attempt count can include entries that disappeared or became inaccessible during traversal. The extra `fstatfs` probe is once per scan in the FDU code; the trace also covers launch wrappers. Futex and elapsed-time differences are host-noisy and do not establish a speedup. Raw counters are `target/perf-initial/fdu-baseline-ext4-eof-cookie.txt` and `target/perf-initial/fdu-ext4-eof-cookie.txt`.

A ten-scan on-CPU profile after this change recorded 6,977 samples with zero lost. Self samples included `ext4_file_getattr` at 6.88%, `memcpy` at 5.68%, Rayon `__lock` at 4.09%, and `__libc_free` at 3.34%. The metadata lookup and output-building costs remain visible after the directory EOF syscall reduction. Raw data and report are `target/perf-initial/fdu-ext4-eof-cookie-10.data` and `target/perf-initial/fdu-ext4-eof-cookie-10.report.txt`.

## Remaining metadata work

A read-only dirent inode census found 14 duplicate non-directory paths among 1,023,593 readable paths. A shared inode metadata cache would add synchronization and memory for almost no avoided `statx` calls. The remaining per-file metadata lookup is the main syscall target.

An ext4 inode-table experiment could batch inode metadata reads by group and table block, but it must stay isolated from the default walker. An on-disk table read is not a coherent view of a live mounted filesystem: Linux 6.18's [`ext4_file_getattr`](https://github.com/torvalds/linux/blob/v6.18/fs/ext4/inode.c) adds delayed-allocation blocks held in memory and accounts for inline-data inodes using their logical size. An on-disk inode therefore cannot always reproduce the live `statx` allocation value. The [Linux ext4 inode documentation](https://github.com/torvalds/linux/blob/v6.18/Documentation/filesystems/ext4/inodes.rst) describes the inode-table layout and inode-to-group mapping. This host has no stable snapshot of `crabc`, and its block device is not readable by the normal user running the scanner. Keep table reads out of the live path; revisit sparse and dense reads only against a read-only snapshot, with a separate accounting contract.

`io_uring` is enabled on this host, but asynchronous `statx` should remain an experiment. Linux 6.18's [`io_uring/statx.c`](https://github.com/torvalds/linux/blob/v6.18/io_uring/statx.c) marks every `IORING_OP_STATX` request `REQ_F_FORCE_ASYNC`; batching could replace many user `statx` entries with fewer ring submissions, while also moving every metadata lookup through io-wq.

A dispatch microbenchmark on a warm ext4 directory of 8,192 zero-byte files compared direct `statx` calls with `IORING_OP_STATX` batches of 128. Both modes used one thread, the same directory fd, the same statx mask, one warm-up pass, and ten measured passes. The ring reduced syscall entries but increased task scheduling and elapsed time in both runs:

| Pair | Mode | `statx` entries | `io_uring_enter` entries | Context switches | Task-clock ms | Elapsed ms |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | Direct | 90,112 | 0 | 4 | 37.47 | 37.84 |
| 1 | Ring | 0 | 704 | 13,961 | 141.12 | 46.92 |
| 2 | Direct | 90,112 | 0 | 6 | 25.52 | 25.86 |
| 2 | Ring | 0 | 704 | 15,038 | 174.16 | 54.42 |

This is a single-thread metadata microbenchmark, not a full four-worker scan. The result still argues against adding ring submission to the cached metadata path: it replaces 90,112 syscall entries with 704 but routes each operation through asynchronous work. The probe source and captures are `target/perf-initial/statx-uring-probe.c`, `target/perf-initial/statx-direct-perf.txt`, `target/perf-initial/statx-uring-perf.txt`, `target/perf-initial/statx-direct-perf-2.txt`, and `target/perf-initial/statx-uring-perf-2.txt`. Retest only if measurements on cold storage show metadata wait time dominates.

## Reproducing syscall counts

Build an optimized profile with symbols and frame pointers:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --locked --profile profiling
```

Run as the ordinary user under `perf` so the target keeps its normal filesystem permissions. The default pool is capped at four; omit the variable to measure that default.

```sh
doas -n /usr/bin/perf stat \
  -e syscalls:sys_enter_statx,syscalls:sys_enter_openat,syscalls:sys_enter_openat2,\
syscalls:sys_enter_close,\
syscalls:sys_enter_getdents64,syscalls:sys_enter_futex,syscalls:sys_enter_sched_yield,\
syscalls:sys_enter_clone,context-switches \
  -o target/perf-initial/fdu-crabc-syscalls.txt -- \
  /usr/bin/env -u RAYON_NUM_THREADS HOME=/home/josh XDG_CONFIG_HOME=/home/josh/.config \
  /bin/setpriv --reuid=1000 --regid=1000 --init-groups \
  /home/josh/d/fdu/target/profiling/fdu /home/josh/d/crabc \
  >/dev/null
```

`perf stat` tracepoints count calls but do not measure syscall latency. Keep each workload inventory with its counter output, because live builds and cleanup can change the tree between runs. Do not drop caches globally.
