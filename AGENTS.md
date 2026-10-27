# fdu

Disk-usage browser and scanner for Linux and macOS. End-user install and usage: [README.md](README.md).

Build: `cargo build --locked`; test: `cargo test --locked --workspace`; check: `cargo clippy --locked --workspace --all-targets --all-features -- --deny warnings`.

Hard invariants:

- Discovered symlinks are leaves and are never followed.
- Scans stay within the selected root mount.
- Deletion attempts only indexed entries, requires a finished root scan, skips entries whose scan is incomplete or excluded, and preserves the selected root. Other processes must leave fdu's private quarantine directories, their contents, and recovery records untouched, including through previously opened descriptors.
- Browser state changes on the UI thread; scan and deletion workers report events.

Topics: [architecture](docs/architecture.md), [scanning](docs/scanning.md), [deletion](docs/deletion.md), [testing](docs/testing.md), [performance](docs/performance.md), [releases](docs/release.md).
