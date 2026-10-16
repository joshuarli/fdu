# Performance

## Benchmarks

The `fixture` benchmark compares summary scan, indexed scan, index building,
and deletion of a fresh fixture clone. It uses
[`benches/fixture.rs`](../benches/fixture.rs) and rustybench.
The deletion benchmark requires GNU `cp` with `--reflink=auto`, so run it on
Linux.

Create a workspace-local scratch directory, then run:

```sh
mkdir -p .work/tmp
FDU_BENCH_FIXTURE=.work/tmp/layout FDU_BENCH_SCRATCH=.work/tmp cargo bench --locked --bench fixture
FDU_BENCH_FIXTURE=.work/tmp/layout FDU_BENCH_SCRATCH=.work/tmp cargo bench --locked --bench fixture -- scan
FDU_BENCH_FIXTURE=.work/tmp/layout FDU_BENCH_SCRATCH=.work/tmp cargo bench --locked --bench fixture -- delete --sample-count 3
```

The first run creates the anonymized layout fixture with
[`generate_layout_fixture.py`](../scripts/generate_layout_fixture.py). Its
[`workspace-layout.json`](../perf/fixture_profiles/workspace-layout.json)
profile stores only per-directory entry-type counts by depth; the generator
uses synthetic names and does not read source-tree files. `FDU_BENCH_FIXTURE`
chooses the fixture path (default `.work/tmp/fdu-layout-fixture`), and
`FDU_BENCH_SCRATCH` holds temporary deletion clones (default `.work/tmp`).

To create the separate inode-heavy Linux fixture, run
[`generate_inode_fixture.py`](../scripts/generate_inode_fixture.py) with a
new output path. Check free inode capacity first; its `--help` lists scaling
options.

Set `FDU_PROFILE=1` to have the interactive browser report scan readiness,
index memory, and main-thread timing when it exits.

## Captures

Keep selected human-readable captures in [`perf/captures/`](../perf/captures/README.md).
Each capture should record its command, source revision, kernel, filesystem,
and fixture parameters. Preserve `perf stat` output and a
`perf report --stdio` export; include raw `perf.data` only when it adds useful
detail. Do not store generated fixture trees or full scan output there.

## Open experiments

- `NodeId` is a dense index into the tree arena; deletion tombstones records
  rather than removing them. Profile vectors or bitsets for `NodeId` maps and
  mark sets before changing hashers. For `FileIdentity`, compare hashbrown's
  current foldhash with `FxBuildHasher` on a large real tree: inode values may
  be aligned or strided, and the non-cryptographic hasher is suitable only
  because these keys come from local filesystem metadata. Adopt it only if
  end-to-end time improves.
- Consider batched Linux deletion only if profiles show syscall overhead is
  material. Stop if scan time or filesystem journal and directory locks
  dominate, and preserve capture validation before each unlink. macOS has no
  equivalent batch interface.
- Revisit adaptive deletion workers only with repeatable measurements on quiet
  and busy hosts. The current pool has 16 threads. If implemented, tune against
  sustained throughput, grow slowly, shrink when throughput falls, and ignore
  short jobs that finish before measurements settle. Keep the controller out
  of the capture-validation and unlink path.

