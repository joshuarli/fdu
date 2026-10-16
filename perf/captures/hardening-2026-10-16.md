# Hardening baseline, 2026-10-16

Deletion is substantially slower with quarantine and durable recovery. The
median rises from 0.258 seconds to 360.1 seconds for 192,238 entries, about
1,398 times longer. Indexed-tree CPU cost also increases. Scan elapsed ranges
overlap. The baseline lacks the new deletion safety and recovery guarantees.

## Method

- Baseline: `54ca1a3`, before the backlog campaign.
- Candidate: working tree at `dd3013a`, including FDU-1 quarantine and recovery.
  FDU-2 and FDU-4 are already in that revision. The deletion engine was unchanged
  between `54ca1a3` and `dd3013a`.
- Linux 7.2.9-2-stable, 32 logical CPUs, NVMe-backed ext4 with `rw,relatime`.
- Pinned Rust nightly 1.100.0, 2026-09-14. Production release profile: opt-level
  3, fat LTO, one codegen unit, panic abort, stripped. Separate target directories
  prevent artifact reuse across revisions. No dependencies were added.
- Generated workspace-layout fixture: 192,238 entries, 34,017 directories
  including the root, 157,466 regular files, 750 symlinks, six special entries,
  maximum depth 13. Metadata caches are warm.
- Four completed alternating baseline/candidate pairs. The owner cancelled the
  fifth pair, which is excluded throughout this report. Every completed run
  passed `perf-gate-ready` before and `perf-gate-ready -1` immediately after.
- Each benchmark uses one iteration in a fresh process. Summary uses four Rayon
  workers. Indexed scanning uses native defaults. Deletion uses 16 workers.
- Deletion timing excludes fixture cloning, scanning, and planning. The harness
  asserts 192,238 successful removals and an empty preserved root. Indexed-tree
  scanning asserts 192,239 nodes. No completed benchmark failed.

## Elapsed time

Values are medians with minimum–maximum ranges, from four samples per side.

| Workload | Baseline | Hardened |
| --- | ---: | ---: |
| Summary, allocated | 45.39 ms [43.60–45.66] | 47.13 ms [41.22–47.59] |
| Summary, apparent | 44.04 ms [39.85–44.20] | 43.78 ms [39.33–44.33] |
| Indexed events drained | 105.20 ms [95.35–111.41] | 115.90 ms [107.18–119.35] |
| Indexed tree built | 53.01 ms [52.80–53.98] | 56.35 ms [52.25–67.50] |
| Delete whole fixture | 0.258 s [0.256–0.263] | 360.1 s [349.6–387.9] |

## CPU time

Process CPU includes worker threads. Values are medians with ranges.

| Workload | Baseline | Hardened |
| --- | ---: | ---: |
| Summary, allocated | 178.8 ms [172.3–180.7] | 183.0 ms [162.7–188.0] |
| Summary, apparent | 174.1 ms [157.7–174.7] | 173.4 ms [154.6–175.3] |
| Indexed events drained | 1425.1 ms [1289.1–1487.1] | 1502.8 ms [1372.2–1589.5] |
| Indexed tree built | 1253.5 ms [1179.4–1325.9] | 1515.4 ms [1432.1–1904.8] |
| Delete whole fixture | 0.926 s [0.920–0.936] | 10.940 s [10.532–11.292] |

Indexed-tree CPU increases about 21%, with disjoint observed ranges. Its elapsed
ranges overlap. Four pairs provide a limited baseline rather than the planned
five-pair measurement.

## Memory and binary size

Peak RSS comes from `/usr/bin/time -v` for the whole benchmark process,
including input preparation. The scan process runs all four scan benchmarks.
RSS excludes kernel filesystem and cache memory.

| Measurement | Baseline | Hardened |
| --- | ---: | ---: |
| Scan peak RSS | 43.28 MiB [41.44–44.32] | 35.40 MiB [28.74–37.53] |
| Deletion peak RSS | 41.84 MiB [40.85–42.31] | 41.18 MiB [41.05–41.30] |
| Stripped executable | 996,904 bytes | 1,040,616 bytes |

The executable grows 43,712 bytes, or 4.4%.

## Cost and limits

Code inspection finds six synchronous durability calls per successful entry:
intent-file sync, intent-publication directory sync, both capture directories,
capture-removal sync, and intent-removal sync. That is 1,153,428 calls across
this fixture, plus session setup and cleanup. Workers were observed waiting in
`jbd2_log_wait_commit`. The long elapsed time and low CPU utilization are
consistent with journal-commit latency. All workers share the quarantine
directory. Extra profiling was skipped at the owner's request to finish.

Batching durable intents and directory synchronization is the strongest next
optimization to investigate. It needs recovery-window and parent-ordering tests.
Capture identity checks and conflict-safe restoration must remain intact.

These measurements cover this Linux filesystem and warm metadata caches. They
do not establish cold-cache, macOS, UI-rendering, or physical power-loss results.

## Reproduce

Build the baseline and candidate separately with `cargo build --offline --locked
--release -p fdu --bin fdu --bench fixture`, setting a distinct
`CARGO_TARGET_DIR` for each revision. Generate the fixture with
`scripts/generate_layout_fixture.py`. Set `TMPDIR`, `FDU_BENCH_FIXTURE`, and
`FDU_BENCH_SCRATCH` to paths under the repository's `.work/` directory, and set
`RAYON_NUM_THREADS=4`.

Run the resulting fixture executable directly with `--bench scan --sample-count
1 --sample-size 1 --format json`, then with `--bench delete --sample-count 1
--sample-size 1 --format json`. Direct invocation requires `--bench`; otherwise
Rustybench runs in test mode. Alternate revisions and retain only runs whose
host-readiness checks both pass. Read JSON with `jaq`. Keep profiler runs
separate from timing.
