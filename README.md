# fdu

`fdu` is a disk-usage browser and summary scanner for Linux 6.0+ and macOS 26+.
macOS releases earlier than 26 are outside the support guarantee.
On both platforms, terminal sessions open the interactive browser by default.
Piped and redirected runs use the summary scanner.

```text
fdu [--apparent] [--summary | --interactive] [--read-only] [PATH]
```

`PATH` defaults to the current directory. The command line resolves symlinks
in the supplied root path before traversal anchors to the opened directory.
Discovered symlinks are selectable leaf entries and are never followed.

By default, sizes use filesystem-allocated bytes (native metadata blocks
multiplied by 512); `--apparent` uses logical file lengths. Hardlinked paths
count separately. Directory inode storage is excluded from totals. Hidden and
ignored names are included; FDU does not read
`.gitignore` or apply implicit cache exclusions. Files and directories on
another filesystem or nested mount are excluded and reported as a boundary,
not shown as empty directories. Recognized unsupported directory aliases are
reported and skipped to prevent cycles; directory hardlinks and firmlink
traversal are not specially implemented.
The macOS backend checks device, filesystem ID, and mount point from opened
directory descriptors. Those checks are not atomic against concurrent mount
or path changes, and some filesystems may not distinguish every same-filesystem
remount. The Linux summary backend uses `openat2` with `RESOLVE_NO_XDEV` and a
descriptor mount-ID fallback. The Linux indexed backend and deletion compare the
device and the kernel mount ID from `statx`, so bind mounts and btrfs subvolumes
count as boundaries.

## Modes

- `--summary` prints one row for each immediate child directory, sorted by
  allocated or apparent bytes in descending order. Each row is a byte count, a tab, and a quoted
  name with Rust debug escaping. Root-level files do not appear in this output.
  Summary formatting and accounting follow the original scanner; nested mount
  contents are now intentionally excluded.
- Interactive mode retains an index for browsing and deletion. `--read-only`
  disables deletion. `--interactive` requires a usable terminal and reports an
  error when this is a summary-only build.
- A summary-only binary omits terminal dependencies:

  ```sh
  cargo build --release --no-default-features
  target/release/fdu --summary PATH
  ```

## Browser controls

The screen has a title strip, a framed listing, and a status strip. Marking
opens the marked-items pane beside the listing. The title strip shows only
non-default state (`read only`, `apparent sizes`, an active filter). The status
strip leads with the total entry count, ending in `…` while the scan is still
adding to it, followed by transient messages and key hints for the focused
pane; deletion prompts and progress appear there too. The listing
shows each entry's size, share of the directory, and a bar. `[x]` marks an
entry, `[=]` shows an entry already covered by a marked parent directory, and
`!`, `~`, `M`, and `A` flag incomplete, stale, mount-boundary, and
unsupported-alias entries. Directories end in `/` and symlinks in `@`;
symlinks are never followed. Directory and symlink colors follow the standard
`LS_COLORS` environment variable (`di`, `ln`, `fi`, and `*` patterns);
marked entries and the marked-items pane stay red as the deletion review,
and deletion prompts are red too. `?` opens help.

Use arrows or `j`/`k` to move, Enter/right/`l` to open, and left/`h`/Backspace
to return; navigation stops at the directory fdu was started in. Page Up/Down
and Home/End move through a listing. `d` toggles a mark and moves down; `v`
starts or ends a stable range; `*` marks the currently matching entries; `c`
clears marks. `s` toggles the order between size and name, and `a` switches
between allocated and apparent sizes.

Marks form a deletion basket for the whole root. They survive filtering,
sorting, and moving between directories, and clear only on rescan or after a
deletion. The marked-items pane lists each mark by its path relative to the
root, including marks the current filter hides, with a count and size total.
Tab moves focus between the listing and the pane; the other pane is dimmed.
`-` switches between side-by-side and stacked panes. In the pane, `d` removes
the highlighted mark, Enter shows it in the listing, and `c` clears all marks.
The pane closes when the last mark is removed. When the terminal is too small
for both panes (under 60 columns side by side, under 12 rows stacked), only the
focused pane is shown and Tab swaps them.

Marks never overlap. A marked directory covers its whole indexed subtree, so
entries inside it cannot be marked, and a directory that contains a marked entry
cannot be marked until that mark is removed. The status strip says which mark
is in the way. `*` and range marking skip such entries and report how many.

Deletion starts from the marked-items pane, which is the review of what will be
removed: press Ctrl-R there. The status strip then asks for confirmation;
Enter deletes permanently and Esc cancels, and other keys do nothing. There is
no Trash. If any marked entry is ineligible nothing is deleted or narrowed and
the status strip names the first reason. Directories are eligible only when
their indexed subtrees were complete and had no traversal exclusions or errors.
While deleting, the status strip shows progress and every command except Esc is
ignored. Esc stops scheduling later removals; it cannot restore entries already
removed. The status strip then reports how many entries were deleted, already
absent, changed since scanning, failed, or not attempted.

Scanning progress is shown as results arrive. Navigation, filtering, sorting,
and marking remain available while scanning; deletion and refresh wait until
workers stop. Press `r` to rebuild the full root index, `/` to filter names in
the current directory, `?` for help, and `q` or Ctrl-C to quit.

Unknown and incomplete sizes are displayed as incomplete. `*` marks only
entries already known to match while a scan is in progress. Individually
indexed files and symlinks remain selectable when an unrelated entry has an
error; directories with incomplete or excluded subtrees are rejected. Failed
metadata entries are omitted from totals and reported as incomplete, and
root-level open or enumeration errors stop the scan.

Deletion uses the completed index as its manifest and attempts only indexed
entries. A child created after confirmation is left in place. Each target is
checked against its expected identity and type immediately before unlinking.
Descriptor-relative validation followed by `unlinkat` cannot atomically
compare and remove an inode; concurrent external mutation and inode reuse
remain limitations. Counted bytes are metadata totals and do not promise the
amount of storage the filesystem releases.

## Architecture

- `fdu-core` owns compact indexed records, IDs, totals, and scanner protocol
  types; it has no filesystem or terminal operations.
- `fdu-scan` owns summary and indexed read-only scanning and platform backends.
  The indexed scan's worker queue and completion tracking are shared; each
  platform supplies directory enumeration, metadata, and mount identity (macOS
  `getattrlistbulk`, Linux `getdents64` plus `fstatat`).
- `fdu-delete` plans and executes descriptor-relative deletion independently
  of scanning and the terminal. A plan may span directories but never contains
  overlapping roots.
- `fdu-tui` maps input to intents and renders borrowed model state; it performs
  no filesystem I/O. Terminal settings and input readiness use rustix; a local
  cell renderer writes ANSI updates, with unicode-width for name alignment.
- `src/browser.rs` coordinates scan, ready, and delete phases.
  `src/main.rs` parses options and preserves the summary CLI.

Workers use ordinary threads and bounded channels. The browser owns mutable
state, streams batches into an append-only arena, and retains packed filename
bytes instead of one allocation or path per indexed entry. A rescan discards
that index and builds a new one. The operation phases are `Scanning → Ready →
Deleting → Ready`; a full-root rescan returns to `Scanning`. Scan and deletion
workers stop and their results are consumed before a phase transition. The
Linux summary scanner keeps its four-worker Rayon traversal separate from
indexed browser state.

The indexed scanner (`crates/fdu-scan/src/indexed.rs`, shared by both platforms)
is a pool of workers with work-stealing deques. Each worker keeps the
directories it finds and scans them depth-first, and idle workers steal the
oldest ones. A worker gathers the entries of consecutive directories into one
batch (each entry names its parent), so the browser receives a few large
messages rather than one per directory. Ordering is part of the contract with
the browser: a directory's own entry is sent before anything found inside it,
and a directory is reported finished only after the entries of everything below
it. A worker therefore sends its entries first, then makes the directories it
found available to others, and only then releases the directories it finished to
their parents. On Linux the pool is sized for a cold cache (many threads wait on
storage) but only twice the cores start; the rest are released if sampling shows
scan time going to blocked I/O, since on a warm cache they only compete. Queued
directories hold descriptors, so the pool is also bounded by the descriptor
limit, which is raised toward the hard limit at start.

Deletion (`crates/fdu-delete`) runs on a Rayon pool, one task per directory. A
task opens its directory from its parent's descriptor, checks it against the
index, removes its files in inode order, and spawns tasks for its
subdirectories. The last task to finish a directory removes it and releases its
parent. Every planned entry gets exactly one outcome, and entries not attempted
after Esc are reported as not attempted.

## Performance

Benchmarks use [rustybench](https://github.com/joshuarli/rustybench).

```sh
cargo bench --bench fixture                      # all benchmarks
cargo bench --bench fixture -- scan              # summary and indexed scans
cargo bench --bench fixture -- delete --sample-count 3
```

`benches/fixture.rs` times the summary scan, the indexed scan with its events
dropped, the indexed scan building the tree the way the browser does, and
deleting a fresh clone of the whole layout fixture (the clone is made outside the
timed region and needs room for a copy under `FDU_BENCH_SCRATCH`, default the
system temporary directory). `FDU_PROFILE=1` makes the interface report, on exit,
the time to ready, when the first entries arrived, how much memory the index uses
per entry, and how the main thread spent its time. Wall-clock times on a small VM
vary by tens of milliseconds, so changes were judged by instruction counts
(`valgrind --tool=callgrind`), syscall counts (`strace -f -c`), and CPU time as
well as by the clock.

On the 192 thousand entry layout fixture (4 vCPUs, ext4 with `discard`), compared with
the first Linux port:

| | before | now |
|---|---|---|
| time to ready, warm cache | 565 ms | about 195 ms |
| time to ready, cold cache | about 1.4 s | about 0.8 s |
| indexed scan alone (in process) | 270 ms | about 142 ms |
| delete everything (in process) | 7.0 s | about 2.3 s |
| delete everything (through the interface) | 5.6 s | about 2.1 s |
| index memory per entry | 138 bytes | 84 bytes |

The summary scan, which does nothing but read the metadata, takes about 105 ms and
uses about 0.4 CPU-seconds in the kernel, so the indexed scan is within about 35 ms of
the floor for this approach. Deleting is limited by the filesystem: removing a
directory takes about 100 microseconds of waiting, against 5 for a file, which is why
the deletion pool is much larger than the number of cores.

## Build and check

```sh
cargo build --locked --release
cargo test --locked --workspace
cargo test --locked -p fdu --no-default-features
```

The terminal tests use the sibling `../ptytest` crate, so that checkout must be
present (Cargo needs it to load the workspace even for other builds). They run
on macOS and Linux. `tests/interactive_pty.rs` checks
behavior on small trees. `tests/ui_snapshots.rs` freezes complete screens,
including cell attributes, in `tests/snapshots/`; it opens the generated
layout fixture at `/tmp/fdu-layout-fixture` (built once with a fixed seed by
`scripts/generate_layout_fixture.py`, about a minute) in read-only apparent
mode so the frames do not depend on the machine. Re-record after an intended
UI change with `PTYTEST_UPDATE_SNAPSHOTS=1 cargo test --test ui_snapshots` and
review the diff.
`tests/delete_performance.rs` deletes a copy-on-write clone of that fixture
through the interface and reports how long it took; run it with
`cargo test --release --test delete_performance -- --ignored --nocapture`.

Linux uses four Rayon workers by default; `RAYON_NUM_THREADS` selects another
count. The macOS summary scanner is sequential. The directory tree is live,
not an atomic snapshot, so concurrent filesystem changes may be observed at
different times. See [PERF.md](PERF.md) for platform details, profiling
methods, measured results, and coverage gaps.

Two stress tests run many scans, or many deletions, at once over trees that split
into tasks of very different sizes (deep chains, fan-outs of small directories, wide
directories). They are part of the normal test run; raise `FDU_STRESS_SCANS` or
`FDU_STRESS_DELETES` to soak, for example
`FDU_STRESS_SCANS=300 cargo test --release -p fdu-scan --test indexed_policy concurrent_scans`.
They exist because the pools coordinate through counters and wake-ups, and a missed
wake-up shows up only as an occasional hang; the scan test caught one such bug in
development.

To generate the Linux inode-heavy scanner fixture:

```sh
python3 scripts/generate_inode_fixture.py perf/fixture
target/release/fdu --summary perf/fixture
```

The generated fixture is ignored by Git. Check free inode capacity before
choosing large custom counts; see the generator's `--help` for scaling options.

To generate the anonymized workspace-layout fixture used for indexing
benchmarks:

```sh
python3 scripts/generate_layout_fixture.py /tmp/fdu-workspace-layout
```

The committed profile in `perf/fixture_profiles/workspace-layout.json` is an
anonymized snapshot of the parent tree used during profiling. It stores only
per-directory entry-type counts by depth. The generator uses synthetic names,
writes 1-, 4-, and 8-byte payloads to every sixteenth regular file, and
represents symbolic links and special entries as dangling links and FIFOs. It
reads no source tree or file contents.
