# fdu

`fdu` is a disk-usage browser and summary scanner for Linux 6.0+ and macOS 26+.
macOS releases earlier than 26 are outside the support guarantee.
On macOS, terminal sessions open the interactive browser by default. Piped and
redirected runs use the summary scanner. Linux currently provides summary mode;
indexed browsing and deletion are implemented for macOS.

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
descriptor mount-ID fallback.

## Modes

- `--summary` prints one row for each immediate child directory, sorted by
  allocated or apparent bytes in descending order. Each row is a byte count, a tab, and a quoted
  name with Rust debug escaping. Root-level files do not appear in this output.
  Summary formatting and accounting follow the original scanner; nested mount
  contents are now intentionally excluded.
- Interactive mode on macOS retains an index for browsing and deletion. `--read-only`
  disables deletion. `--interactive` requires a usable terminal and reports an
  error when this build or platform has no interactive implementation.
- A summary-only binary omits terminal dependencies:

  ```sh
  cargo build --release --no-default-features
  target/release/fdu --summary PATH
  ```

## Browser controls

Use arrows or `j`/`k` to move, Enter/right/`l` to open, and left/`h`/Backspace
to return. Page Up/Down and Home/End move through a listing. Space toggles a
mark; `v` starts or ends a stable range; `*` marks the currently matching
entries; `c` clears marks. Marks apply only to immediate children in the
current directory and remain through filtering and sorting. They clear when
you navigate or rescan. The marked count includes filtered-out entries.

Press `d` to review and permanently delete the marked siblings, or the current
entry if none are marked. The confirmation waits for an explicit Enter; Esc
cancels. Directories are eligible only when their indexed subtrees were
complete and had no traversal exclusions or errors. Scanning progress is shown
as results arrive. Navigation, filtering, sorting, and marking remain available
while scanning; deletion and refresh wait until workers stop. Press `r` to
rebuild the full root index, `/` to filter names in the current directory, `n`
or `s` to sort, `a` to switch between allocated and apparent sizes, `?` for
help, and `q` or Ctrl-C to quit.

The browser shows a breadcrumb, entry counts, size bars, scan progress, and
error or exclusion markers. Unknown and incomplete sizes are displayed as
incomplete. `*` marks only entries already known to match while a scan is in
progress. Individually indexed files and symlinks remain selectable when an
unrelated entry has an error; directories with incomplete or excluded
subtrees are rejected. Failed metadata entries are omitted from totals and
reported as incomplete, and root-level open or enumeration errors stop the
scan.

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
- `fdu-delete` plans and executes descriptor-relative deletion independently
  of scanning and the terminal.
- `fdu-tui` maps input to intents and renders borrowed model state; it performs
  no filesystem I/O.
- `src/browser.rs` coordinates scan, ready, and delete phases on the macOS
  host build. `src/main.rs` parses options and preserves the summary CLI.

Workers use ordinary threads and bounded channels. The browser owns mutable
state, streams batches into an append-only arena, and retains packed filename
bytes instead of one allocation or path per indexed entry. A rescan discards
that index and builds a new one. The operation phases are `Scanning → Ready →
Deleting → Ready`; a full-root rescan returns to `Scanning`. Scan and deletion
workers stop and their results are consumed before a phase transition. The
Linux scanner remains summary-only in this port; its four-worker Rayon
traversal is kept separate from indexed browser state.

## Build and check

```sh
cargo build --locked --release
cargo test --locked --workspace
cargo test --locked -p fdu --no-default-features
```

Linux uses four Rayon workers by default; `RAYON_NUM_THREADS` selects another
count. The macOS summary scanner is sequential. The directory tree is live,
not an atomic snapshot, so concurrent filesystem changes may be observed at
different times. See [PERF.md](PERF.md) for platform details, profiling
methods, measured results, and coverage gaps.

To generate the Linux inode-heavy scanner fixture:

```sh
python3 scripts/generate_inode_fixture.py perf/fixture
target/release/fdu --summary perf/fixture
```

The generated fixture is ignored by Git. Check free inode capacity before
choosing large custom counts; see the generator's `--help` for scaling options.

To generate the anonymized workspace-layout fixture used for macOS indexing
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
