Prefix: FDU
Next: 12

### FDU-9 · P3 · Pin the Linux release builder inputs

- **Area:** Build
- **Where:** `Dockerfile:1`, `Dockerfile:10`
- **Problem:** The Linux builder uses the mutable `alpine:3.24.1` tag and installs distribution packages without version pins. Rebuilding a release revision can therefore use a different base or package set, changing the toolchain environment.
- **Recommendation:** Pin the base image by digest and use a versioned package snapshot or pinned package versions.
- **Fix with:** none
- **Effort:** S

### FDU-11 · P2 · Scanner fails the documented warning-denied Clippy check

- **Area:** Build
- **Where:** `crates/fdu-scan/src/linux.rs:84`, `crates/fdu-scan/src/linux.rs:395`
- **Problem:** The pinned toolchain rejects the scanner under `--deny warnings` for `missing_const_for_thread_local` and `unnecessary_sort_by`. The focused command `cargo clippy --offline --locked -p fdu-scan --lib --test indexed_policy -- --deny warnings` fails on the original backlog base as well as the fixed scanner. These errors block the documented Clippy check.
- **Recommendation:** Make the directory-entry pool initializer const and sort summary rows by a reversed size key, preserving descending order.
- **Fix with:** none
- **Effort:** S
