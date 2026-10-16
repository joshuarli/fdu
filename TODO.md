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

### FDU-9 · P3 · Pin the Linux release builder inputs

- **Area:** Build
- **Where:** `Dockerfile:1`, `Dockerfile:10`
- **Problem:** The Linux builder uses the mutable `alpine:3.24.1` tag and installs distribution packages without version pins. Rebuilding a release revision can therefore use a different base or package set, changing the toolchain environment.
- **Recommendation:** Pin the base image by digest and use a versioned package snapshot or pinned package versions.
- **Fix with:** none
- **Effort:** S
