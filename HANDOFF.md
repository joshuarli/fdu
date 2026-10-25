# Handoff — scanning performance, Linux follow-up

## Revisions

- fd `a7302bde6dc6636ac9f073ee877bc61c56171c28` + uncommitted `src/walk.rs` size-filter dedup.
- fdu `ea2a827f36926b942e314135c78ed213b25b2dc0c` + uncommitted `crates/fdu-scan/src/macos.rs` summary bulk for subdirs.

After this handoff both are committed (one commit per repo, this file included in fdu).

## What is retained

### fd `src/walk.rs:547-569`

Size filter used `Path::is_file()` (follows, extra stat) then cached `entry.metadata()`
(`symlink_metadata`). Now `entry.file_type()` (`d_type`, free) decides regular files and
dirs without a stat; symlinks/unknown fall back to `is_file()` to preserve follow
semantics. Symlink size still uses lstat length (pre-existing quirk, preserved).

Warm macOS APFS, 192k layout: `--size +1B ''` ~157–165ms → ~146–163ms. Match-all and
no-match neutral. `cargo test --workspace` 116 pass. Clippy fails pre-existing
`build.rs:26` on baseline too.

### fdu `crates/fdu-scan/src/macos.rs`

`read_entries` kept plain for the scan root (root files are dropped, bulk would fetch
unneeded metadata — naive bulk-everywhere was 3x slower on a 20k root-file dir).
`scan_subtree` uses new `read_entries_with_metadata` via existing
`IndexedDirectoryStream` (`getattrlistbulk` + `Dir` fallback); file sizes come from
`BulkMetadata`, dirs still open+fstat+fstatfs for mount/alias checks.

Warm: layout `2.48s → 2.15s`, wide-sub `43ms → 34ms`, deep `623ms → 524ms`,
wide-root neutral. Totals identical incl. `--apparent`. `fdu-scan --lib` 12 pass,
`top_level_sizes` 6 pass, workspace clippy clean.

## Artifacts (local only, git-ignored)

- Baselines/candidates: `.work/tmp/fd-baseline`, `.work/tmp/fd-candidate`,
  `.work/tmp/fdu-baseline`, `.work/tmp/fdu-candidate2`.
- Fixtures: `.work/tmp/fdu-layout-fixture` (`python3 scripts/generate_layout_fixture.py`),
  `wide` / `wide-sub` / `deep` (ad-hoc scripts, see shell history), `ignore-repo`, `symtest`.
- Rebuild on Linux; do not trust macOS binaries there.

## Linux todo

1. Syscall proof: `strace -c` / `perf stat` the paired binaries on the same fixtures.
   fd should show −1 stat per regular file/dir under `--size`. fdu should show fewer
   `newfstatat` on summary; check `openat2 RESOLVE_NO_XDEV` vs mount-ID fallback
   (`linux.rs:817-844`). dtruss/Docker-run hung on macOS, so this was not collected.
2. Revisit prior rejects on current kernel: `perf/captures/uring-worker-limit-2026-10-02.json`,
   inode-ordering, 1M buffer. Use layout + `scripts/generate_inode_fixture.py`.
3. Indexed scheduling: fixed-worker baseline vs storage-wait gating
   (`indexed.rs:452-474,1068-1093`). Vary consumer capacity (8 vs 512) to separate
   publication block from storage wait. Measure startup, small-tree latency, total CPU.
4. Cold-cache: follow `docs/performance.md` capture policy; keep profiler runs separate
   from timing runs; record kernel/fs/fixture with each capture.

## Rejected on macOS (do not repeat without new evidence)

- fd owned-batch publication, worker-local regex clones, full-path scratch: sys-bound
  (`user 0.14s sys 1.62s`), early-exit and `| head` already responsive.
- fd native bulk walker: would duplicate ignore inheritance/pruning.
- fdu summary concurrency beyond bulk: bulk already batches metadata; root-bulk waste
  documented above.
