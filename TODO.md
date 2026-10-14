Prefix: FDU
Next: 11

### FDU-1 · P1 · Concurrent replacement can unlink an unindexed entry

- **Area:** Data
- **Where:** `crates/fdu-delete/src/lib.rs:669`, `src/rmrf.rs:138`, `AGENTS.md:11`
- **Problem:** Browser deletion and `--rmrf` check the indexed identity, then call name-based `unlinkat`. If another process replaces the name between those calls, fdu removes the replacement and reports the indexed entry as deleted. A deterministic interposition experiment against the real deletion engine reproduced this on macOS: after pausing before `unlinkat`, replacing the indexed file caused fdu to report `Deleted` and remove the replacement. The same check-then-remove sequence is used for directories. This violates the invariant that deletion attempts only indexed entries.
- **Recommendation:** Resolve the deletion contract and implement removal so the entry captured for removal is identity-checked before it can be unlinked; cover the interleaving with a deterministic regression.
- **Fix with:** `os-boundaries`, `lifecycle-concurrency`, `root-cause`
- **Effort:** M
- **Design:** [FDU-1 RFC](docs/rfc/FDU-1-concurrent-replacement-deletion.md)

### FDU-2 · P1 · Linux summary reports direct mount boundaries as zero bytes

- **Area:** Data
- **Where:** `crates/fdu-scan/src/linux.rs:438`, `src/main.rs:126`, `crates/fdu-scan/src/macos.rs:301`, `crates/fdu-scan/tests/indexed_policy.rs:591`
- **Problem:** When an immediate child is itself a mount boundary, Linux increments the boundary count and returns size zero, then constructs the summary row with no exclusion reason. The CLI therefore prints `0B` instead of the mount-boundary label that the shared writer emits for macOS. The privileged mount test checks the boundary count but not the summary row. Users can mistake an excluded filesystem for an empty directory.
- **Recommendation:** Preserve the mount-boundary classification in Linux summary results and assert the emitted row in the mount-boundary test.
- **Fix with:** `os-boundaries`, `root-cause`
- **Effort:** S

### FDU-3 · P2 · Reject browser deletion when the root scan is incomplete

- **Area:** Data
- **Where:** `crates/fdu-delete/src/lib.rs:83`, `src/browser.rs:968`, `src/browser.rs:2510`, `AGENTS.md:11`
- **Problem:** The shared planner checks the selected entry but not root state. The focused existing browser test proves that a complete file can be planned for deletion while its root is `Incomplete` because an unrelated directory scan failed. `--rmrf` has a separate root-completeness check, but browser deletion does not. This violates the invariant that deletion requires a complete root scan.
- **Recommendation:** Enforce root completeness at the shared planning boundary and update the browser test to expect rejection.
- **Fix with:** `api-surface-and-module-boundaries`
- **Effort:** M

### FDU-4 · P2 · Bound aggregate directory descriptors across Linux scan workers

- **Area:** Concurrency
- **Where:** `crates/fdu-scan/src/linux/platform.rs:58`, `crates/fdu-scan/src/indexed.rs:895`, `crates/fdu-scan/src/indexed.rs:1003`, `crates/fdu-scan/src/fsutil.rs:146`
- **Problem:** Linux sizes the worker and queue limits from a process descriptor budget, but each worker-local outbox can independently retain up to the 64-directory flush threshold. With a 1024 hard descriptor limit, the configured budget permits 32 workers and 224 queued directories; the root scan and active workers can accumulate far more opened child directories than the budget accounts for. **Unproven runtime peak:** the exact overlap depends on scheduling and tree shape. When the hard limit is reached, descriptor retry cannot raise it further and child opens become incomplete scan entries.
- **Recommendation:** Include worker-local held directories in one aggregate descriptor limit and add a low-`RLIMIT_NOFILE` wide-tree regression.
- **Fix with:** `lifecycle-concurrency`
- **Effort:** M

### FDU-5 · P2 · Restore terminal state on termination signals

- **Area:** Operability
- **Where:** `crates/fdu-tui/src/terminal.rs:42`, `crates/fdu-tui/src/terminal.rs:63`, `crates/fdu-tui/src/terminal.rs:148`, `tests/interactive_pty.rs:463`
- **Problem:** The terminal enters raw mode and the alternate screen, but restoration runs only through explicit cleanup, `Drop`, or the panic hook. Default `SIGTERM` and `SIGHUP` termination bypass those paths; the checked-in PTY test covers Ctrl-C as an input key rather than an OS signal. A terminated interactive run can leave the invoking terminal in raw mode or the alternate screen.
- **Recommendation:** Route supported termination signals through orderly UI shutdown and verify terminal restoration in a PTY test.
- **Fix with:** `lifecycle-concurrency`
- **Effort:** M

### FDU-6 · P3 · Report normal terminal restoration failures

- **Area:** Errors
- **Where:** `crates/fdu-tui/src/terminal.rs:63`, `crates/fdu-tui/src/terminal.rs:154`, `src/browser.rs:1879`
- **Problem:** Explicit browser shutdown calls a restoration method that discards `tcsetattr`, stdout write, and flush errors, then returns success. A user can lose the cleanup failure and receive a successful exit status even though the terminal was not restored.
- **Recommendation:** Return restoration errors from explicit shutdown while keeping `Drop` and panic cleanup best-effort; preserve an earlier operation error if cleanup also fails.
- **Fix with:** `error-model`
- **Effort:** M

### FDU-7 · P3 · Match help output to summary-only builds

- **Area:** Operability
- **Where:** `src/main.rs:331`, `src/main.rs:356`
- **Problem:** Help always advertises `--interactive` and `--rmrf`, but a build without the `interactive` feature returns `Unsupported` for both modes. Users of summary-only binaries receive commands that the binary cannot run.
- **Recommendation:** Generate feature-aware help text for each build configuration.
- **Fix with:** `api-surface-and-module-boundaries`
- **Effort:** S

### FDU-8 · P3 · Run repository-owned checks on pull requests

- **Area:** Build
- **Where:** `.github/workflows/release.yml:4`
- **Problem:** The only checked-in workflow is triggered by `workflow_dispatch`; its build and test jobs do not run automatically for pull requests or pushes. External repository settings may provide checks, but no workflow in this repository verifies proposed changes before merge.
- **Recommendation:** Add a least-privilege pull-request workflow for the locked builds and focused test suites, separate from release publishing.
- **Fix with:** none
- **Effort:** M

### FDU-9 · P3 · Pin the Linux release builder inputs

- **Area:** Build
- **Where:** `Dockerfile:1`, `Dockerfile:10`
- **Problem:** The Linux builder uses the mutable `alpine:3.24.1` tag and installs distribution packages without version pins. Rebuilding a release revision can therefore use a different base or package set, changing the toolchain environment.
- **Recommendation:** Pin the base image by digest and use a versioned package snapshot or pinned package versions.
- **Fix with:** none
- **Effort:** S

### FDU-10 · P3 · Verify scan-time navigation latency on a quiet host

- **Area:** Performance
- **Where:** `crates/fdu-scan/src/macos.rs:25`, `src/browser.rs:1725`, `tests/interactive_pty.rs:654`
- **Problem:** Five alternating release PTY runs indexed all 180,001 entries. The perf gate was skipped at the user's request, and the host was busy. Median navigation p95 was 2.585 ms before the changes and 2.605 ms after; the ranges overlapped. The candidate had lower median p50 and scan-ready time, but these runs do not establish a p95 improvement.
- **Recommendation:** Build release binaries for the parent and candidate commits, then run five alternating pairs with `FDU_BIN=<binary> cargo test --release --locked --test interactive_pty navigation_latency_during_indexed_scan -- --ignored --exact --nocapture` on a quiet macOS host. Keep or retune worker headroom and the 4 ms scan-drain budget only if p95 improves beyond run-to-run spread. Confirm all 180,001 entries on each run.
- **Fix with:** `make-it-fast`, `benchmark-checklist`
- **Effort:** S
