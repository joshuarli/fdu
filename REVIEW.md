# Review: fdu

Scope: repository source, tests, documentation, workspace configuration, and release workflow. Verification: `cargo check --locked --workspace --all-targets` (pass); `cargo test --locked -p fdu --bin fdu` (35 passed); `cargo test --locked -p fdu-scan` (22 passed, mount test visibly ignored); repeated-anchor scanner regression and core tombstone accounting tests (pass); privileged mount test in a `SYS_ADMIN` Docker container (pass); focused Clippy checks (pass). The full workspace Clippy command still reports existing warnings in unchanged code: scanner thread-local initialization and sorting, `io::Error` construction in scanner examples/tests, and casts in TUI tests. The release Docker image build and macOS runtime were not run locally.

## Summary

All five reviewed findings are addressed. `--rmrf` captures and revalidates the selected root identity around scanning and confirmation; each indexed scan opens an independent root stream; release validation explicitly runs the mount test with mount capability; the Rust bootstrap is versioned and checksum-verified; and only `Tree::tombstone_subtree` can create tombstones.

## Findings

No unresolved findings.

## Resolved findings

- R1: confirmation uses a canonical path captured against the opened root identity and aborts if the selected path changes target before confirmation.
- R2: every indexed scan opens `.` relative to its root anchor, with a repeated-anchor regression test.
- R3: the mount-boundary test is visibly ignored in the general suite, fails on mount errors when selected, and runs in release validation with `CAP_SYS_ADMIN`.
- R4: both rustup installer downloads use version 1.29.0 URLs and verify published SHA-256 checksums.
- R5: `ScanEntry`, `Tree::append`, and `Tree::set_state` use `NodeStatus`, which excludes tombstones; subtree removal remains the only tombstone path.

## Strengths to keep

- The scanner and deletion engine use anchored directory descriptors, and deletion verifies indexed entry identity and mount membership before unlinking.
- Deletion planning rejects incomplete roots and selected subtrees before starting removal.
- Browser state stays on the UI thread while scan and deletion workers communicate through bounded event channels.
