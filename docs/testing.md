# Testing

Run the workspace tests and the summary-only build's tests with:

```sh
cargo test --locked --workspace
cargo test --locked -p fdu --no-default-features
```

The terminal tests use the `laputa-ptytest` registry dependency. UI snapshot
tests generate and reuse a 192,238-entry fixture at
`.work/tmp/fdu-layout-fixture`; Python 3 and enough space for the fixture are
required. The fixture is created automatically on the first snapshot test run.

[`tests/interactive_pty.rs`](../tests/interactive_pty.rs) covers browser
behavior on small trees. [`tests/ui_snapshots.rs`](../tests/ui_snapshots.rs)
stores complete terminal frames in `tests/snapshots/`. After an intended UI
change, update and review snapshots with:

```sh
PTYTEST_UPDATE_SNAPSHOTS=1 cargo test --locked --test ui_snapshots
```

Stress coverage runs in the normal suite across deep chains, wide directories,
and directory fan-outs. Set `FDU_STRESS_SCANS` or `FDU_STRESS_DELETES` to
increase its repeated scan or deletion rounds.
[`indexed_policy.rs`](../crates/fdu-scan/tests/indexed_policy.rs) contains
the scan stress test.

The Linux mount-boundary test is ignored in the general suite because it needs
`CAP_SYS_ADMIN`. Run it in a Linux container with that capability using
`make test-mounts TARGET=x86_64-unknown-linux-musl` (or the matching target for
the host architecture). The release workflow runs this test explicitly.

The whole-fixture interactive deletion test is ignored by default because it
removes about 190,000 entries from a private fixture clone. Run it with:

```sh
cargo test --locked --release --test delete_performance -- --ignored --nocapture
```
