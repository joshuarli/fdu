# Performance notes

## First fixed-four capture

Captured 2026-10-02 on the same ext4 `~/d/crabc` workload as the source scanner. The tree was changing while other work continued, so wall time and scheduler counts are not stable performance evidence. Every measured process ran as the normal user; `perf` ran through `doas`. No caches were dropped.

`fdu` leaves `RAYON_NUM_THREADS` unset and creates a four-worker global pool by default. A paired syscall pass recorded four worker clones for both implementations. The filesystem syscall counts matched: 995,293 `statx`, 60,226 `openat`, 60,197 `close`, and 120,467 `getdents64`. `fdu` avoided the source CLI's extra root `newlstat`; both paths performed the same root `statx` and recursive filesystem operations.

Three alternating fixed-four captures on a later snapshot again showed matching traversal calls. The path count in the preceding user-readable inventory was 1,055,519.

| Pass | `statx` source / fdu | `getdents64` source / fdu | `futex` source / fdu | Elapsed seconds source / fdu |
| --- | ---: | ---: | ---: | ---: |
| 1 | 997,961 / 997,961 | 122,285 / 122,285 | 1,214,982 / 800,439 | 0.581 / 0.564 |
| 2 | 997,965 / 997,961 | 122,285 / 122,285 | 729,993 / 812,927 | 0.437 / 0.563 |
| 3 | 997,973 / 997,973 | 122,285 / 122,285 | 702,033 / 1,061,473 | 0.434 / 0.613 |

The mean futex counts differ by about 1%; their run-to-run spread is much larger than that. Elapsed times also vary with host load and do not establish equal throughput. The useful result so far is that the greenfield walker preserves the filesystem syscall path while removing a root metadata lookup. Rerun on a stable tree and quieter host before claiming a throughput change.

A ten-scan on-CPU profile of `fdu` recorded 7,333 samples with zero lost. `ext4_file_getattr` was 5.92% and Rayon `__lock` was 4.96% of samples. `memcpy` was 4.80%, including formatting the full plain-text tree even though stdout was redirected. Raw captures are in the ignored `target/perf-initial/` directory, including `fdu-crabc-pinned-rayon-10.data` and its report.

## Reproducing syscall counts

Build an optimized profile with symbols and frame pointers:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --locked --profile profiling
```

Run as the ordinary user under `perf` so the target keeps its normal filesystem permissions. The default pool is capped at four; omit the variable to measure that default.

```sh
doas -n /usr/bin/perf stat \
  -e syscalls:sys_enter_statx,syscalls:sys_enter_openat,syscalls:sys_enter_close,\
syscalls:sys_enter_getdents64,syscalls:sys_enter_futex,syscalls:sys_enter_sched_yield,\
syscalls:sys_enter_clone,context-switches \
  -o target/perf-initial/fdu-crabc-syscalls.txt -- \
  /usr/bin/env -u RAYON_NUM_THREADS HOME=/home/josh XDG_CONFIG_HOME=/home/josh/.config \
  /bin/setpriv --reuid=1000 --regid=1000 --init-groups \
  /home/josh/d/fdu/target/profiling/fdu /home/josh/d/crabc \
  >/dev/null
```

`perf stat` tracepoints count calls but do not measure syscall latency. Keep each workload inventory with its counter output, because live builds and cleanup can change the tree between runs. Do not drop caches globally.
