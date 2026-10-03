Implement a fast, lean ncdu-style disk-usage browser in fdu.

If the host is macos, then only finish macos parts. Using docker is out of scope.

GOAL

Keep fdu's performant scanner and add a responsive Ratatui browser for
drilling into directories, marking groups of sibling entries, and deleting
them with confirmation.

This is deliberately NOT a general file manager. Prefer fixed semantics,
a small state machine, and explicit refreshes over optional modes and
background reconciliation.

1. FIXED SEMANTICS

One starting directory per invocation.

Resolve the command-line root normally, including symlinks in its path,
then anchor traversal to the opened directory. Beneath that root:

- Never follow symlinks. They are selectable leaf entries; deletion removes
  the link itself, not its target.
- Never descend into other filesystems or nested mounts.
- Show excluded boundaries explicitly, not as empty directories.
- Do not implement special directory-hardlink/firmlink traversal or
  accounting. Guard against directory-identity cycles; report recognized
  unsupported aliases instead of traversing indefinitely.
- Include hidden files and ignored files. Do not apply .gitignore or
  implicit cache/system-directory exclusions.

Use these traversal rules consistently in indexed and summary modes.
Preserve summary output formatting and size-accounting semantics; document
the intentional new mount-boundary restriction.

On Linux, use appropriate openat2 resolution restrictions for nested
directory opens, including RESOLVE_NO_XDEV. Device equality alone is not
sufficient to exclude bind mounts. Apply restrictions beneath the opened
root, not while locating that root. Do not silently fall back to unrestricted
traversal when the required protection is unavailable.

On macOS, use the backend's native boundary checks. Document their limits.
Do not propagate Linux/ext4-specific assumptions across a boundary.

Count hardlinked entries separately. Each directory entry is independently
selectable; selecting one link never selects other names for the same inode.

Retain both apparent size and metadata-reported allocated size in indexed
mode. Preserve the current exclusion of directory inode storage from totals.
No deduplication, shared/unique-byte attribution, clone detection, snapshot
analysis, or promises of exact reclaimed space. A small hard-link indicator
for multiply linked non-directory entries is sufficient.

Do not add inverse flags for these restrictions.

2. ARCHITECTURE

Create a small workspace with clear, acyclic responsibilities:

- fdu-core: IDs, records, retained tree, accounting, and protocol types.
  No filesystem operations or terminal dependencies.
- fdu-scan: read-only scanning and platform backends.
- fdu-delete: independent deletion planning/execution and outcomes.
- fdu-tui: Ratatui/Crossterm presentation and input.
- fdu binary: CLI and application coordination.

Keep scanning and deletion testable without a terminal and independent of
each other. The TUI emits intents and renders state; rendering performs no
filesystem I/O.

Use ordinary threads, bounded channels, and the existing Rayon machinery.
No Tokio, daemon, database, actor framework, or generic filesystem framework.
Prefer Rustix for useful new descriptor-relative deletion operations, with
small libc fallbacks as needed. Do not mechanically rewrite measured Linux
scanning primitives for wrapper uniformity.

Keep a summary-only build path that excludes terminal dependencies.

3. STRICT OPERATION PHASES

Use this application model:

    Scanning -> Ready -> Deleting -> Ready
                   |
                   +-- explicit full-root rescan -> Scanning

Only one filesystem-operation phase is active at a time.

During Scanning:
- Results stream into the browser.
- Navigation, filtering, sorting, and marking remain available.
- Deletion and another refresh are unavailable.
- Explain this in the UI rather than queueing future operations.

Enter Ready only after scan workers have stopped and all results have been
consumed. A finished scan can contain errors; affected entries remain
explicitly incomplete.

During Deleting:
- Show a responsive progress/cancel dialog.
- Normal navigation can be suspended.
- Do not scan, refresh, or start another deletion.
- Apply all outcomes and finish worker shutdown before returning to Ready.

Refresh rebuilds the entire original root index and clears marks. No
subtree refresh, pause/resume scanning, filesystem watcher, or incremental
matching against the previous index.

Cancellation and quitting must not deadlock on full channels or leave
workers running after their model has been discarded. Make shutdown and
channel ownership explicit. Cooperative cancellation between filesystem
operations is sufficient.

With these phase barriers, do not introduce an elaborate scan-epoch or
concurrent scan/deletion reconciliation protocol.

4. PRESERVE THE FAST SCANNER; ADD AN INDEXED POLICY

Keep two explicit scanning policies:

- Summary: current low-memory reduction and immediate-child-directory output.
- Indexed: retain all entries, including files directly beneath the root.

Select the policy outside the hot loop. Share proven traversal machinery
without forcing summary mode to construct a tree, retain filenames, request
unneeded metadata, or publish per-file messages.

Indexed records need:
- A stable NodeId for this index's lifetime.
- Parent/child relationships and original basename bytes.
- Entry type and filesystem identity: device + inode, not inode alone.
- Link count, apparent bytes, allocated bytes.
- Completeness, exclusion, and error state.

NodeId identifies a directory entry, not an inode. Preserve separate records
for hardlinked names. IDs need not survive rescans.

Use compact arena storage and packed names. Do not give every node a full
PathBuf, Arc, lock, or several independent allocations. Materialize paths
only for visible breadcrumbs, details, confirmation, and diagnostics.

An append-only arena during scanning, followed by tombstones on deletion,
is sufficient. Do not implement live compaction or reused-slot generations.
Reclaim the index on rescan or exit.

Do not retain an open descriptor for every indexed directory.

5. BATCHED UPDATES AND RESPONSIVE DISPLAY

Scanner workers publish owned entry batches. Directory discovery and chunks
of wide directories must become visible before the complete subtree finishes.

Use bounded queues and reusable buffers. Structural records, errors, and
completion events are lossless; redundant progress notifications may be
coalesced. Flush partial batches when work finishes.

No allocation, channel message, or global lock per metadata operation.
Published records must not borrow reused enumeration buffers.

Give the mutable model one owner. Ingest batches with a bounded work budget
so scanning cannot starve input. Define parent-before-child handling and
directory/subtree completion precisely.

Maintain totals incrementally, coalescing changes by directory/batch.
Do not double-count sizes from both entry records and completion events.
Use checked arithmetic and explicit incomplete/error states.

Start with approximately 100 ms between scan-driven redraws, prompt
input-driven updates, and no redraw when nothing visible changed.

The render path must not:
- Clone or recount the tree.
- Format every row in a wide directory.
- Re-sort the current directory every frame.
- Query the filesystem.

Render visible rows and deliberately cache listing order. It is acceptable
to defer automatic reordering during scanning. Preserve the cursor by NodeId,
not row number. Do not reorder underneath an active range selection.

6. USER EXPERIENCE

Use the TUI by default when input and output are terminals. Otherwise retain
summary output. Provide explicit --summary and --interactive modes;
--interactive should fail clearly without a usable terminal.

Provide familiar ncdu-style operations:

- Arrows and j/k for movement; Enter/right/l to descend;
  left/h/Backspace to return; paging and home/end.
- Size-descending initial order, name/size sorting, and an
  apparent/allocated-size toggle without rescanning.
- Size bars, breadcrumb, item counts, scan progress, and error/exclusion
  indicators. Unknown or incomplete sizes must not look like final zeroes.
- A simple current-directory name filter.
- Space to toggle a mark and advance, range marking, mark all currently
  matching entries, and clear marks.
- d to delete marked entries, or the cursor entry when nothing is marked.
- r for full-root rescan, ? for help, q to quit, and --read-only.

Selection is LOCAL TO THE CURRENT DIRECTORY:
- Only immediate children can be marked together.
- Clear marks when changing directories or rescanning.
- Preserve marks through sorting and filtering within that directory.
- Show the total marked count, including marks hidden by a filter.
- Mark-all captures currently known matching entries, not future arrivals.

This deliberately eliminates cross-directory selection, global selection
baskets, and ancestor/descendant overlap handling.

Safely display arbitrary filename bytes and escape terminal-control
characters. Display strings must never be used as filesystem identifiers.

Handle resize, small terminals, empty listings, ordinary errors, and
cancellation. Restore terminal state on normal exits, handled interruption,
and unwinding failures. Worker diagnostics must not corrupt the screen.

7. DELETION

Use this lifecycle:

    selected sibling roots -> confirmation -> worker -> outcome batches
    -> updated model

A selected directory is eligible only when its entire subtree was completely
indexed without errors or traversal exclusions. Excluded mounts, unsupported
aliases, the scan root, and the synthetic parent-navigation entry cannot
be deletion targets.

Individual successfully indexed leaf entries may still be deleted when
unrelated parts of the scan contain errors. Reject ineligible selections
explicitly; do not silently narrow the confirmed operation.

Confirmation:
- Show the selected roots, descendant scope/counts, and counted bytes.
- Make permanent deletion explicit and default to cancellation.
- Explain that actual storage released can differ from counted bytes.
- Freeze the selected roots.
- Do not clone the whole tree or construct full paths for every descendant.

The completed index is the deletion manifest. Use a compact plan or immutable
view of its records. No fresh recursive discovery that silently adds new
files to the confirmed operation.

Execute directories post-order. Only attempt indexed descendants.
A newly created child is left alone; its parent may remain nonempty.
This is expected partial-result behavior, not a reason to widen the plan.

Deletion targets are verified parent-directory descriptors + original
basename bytes + expected filesystem identity/type.

Anchor traversal to the opened root. Validate ancestor resolution and target
identity without following symlinks, and maintain mount-boundary restrictions
during deletion. Reuse parent descriptors for sibling operations rather than
resolving full paths from the root for every file.

Immediately before destructive operations, verify the expected identity/type.
Skip detected replacements and require refresh. Do not hunt for renamed
files by inode, delete every hard link, use displayed paths, invoke shell/rm
commands, or delegate a subtree to a blind remove_dir_all operation.

Do not use size, link count, or mutable timestamps as immutable object IDs:
this plan's own deletions can legitimately change such metadata.

Be honest about the safety model. Descriptor-relative validation followed
by unlinkat is not an atomic compare-and-unlink; inode reuse and concurrent
external mutation remain limitations. Do not claim a snapshot or protection
against every hostile rename race.

Use one deletion worker initially, with no parallel deletion scheduler.
Batch progress/outcomes and keep descriptor use reasonable. Cancellation
stops scheduling new work; it does not undo completed operations.

Distinguish:
- Deleted by this operation.
- Already absent.
- Changed/replaced.
- Failed.
- Cancelled/not attempted.

Remove deleted and already-absent entries from the displayed index, but count
only actual successful removals as this operation's deletions. Apply each
accounting adjustment once; removing an emptied directory must not subtract
its former descendant total again.

Leave failures/replacements visible with useful status. Mark uncertain
totals stale and let explicit full-root rescan reconcile them. Do not
automatically rescan after every successful deletion.

8. VERIFICATION

Prefer focused integration suites over large numbers of trivial unit tests.

All destructive tests and benchmarks must use uniquely owned disposable
fixtures. Never delete from the user's home, checkout, or other real data.
Use independent sentinel targets to verify non-followed symlink behavior.

Cover:
- Summary/indexed accounting consistency under the chosen policy.
- Symlinked command-line roots, discovered symlinks, hard links, sparse
  files, non-UTF-8/control-character names, root files, deep/wide trees,
  metadata errors, and directory-cycle/exclusion handling.
- Directory-local marking through filtering, sorting, and streaming;
  marks clearing on navigation/rescan; range-order stability.
- Phase barriers, repeated refreshes, bounded queues, cancellation with
  full queues, and clean worker shutdown.
- Successful group deletion, already-absent entries, replacement before
  validation, new children after confirmation, partial failure, hard-link
  deletion, and cancellation after some successful removals.
- Refusal to delete incomplete/excluded directories and protected entries.
- Mount boundaries, including Linux bind mounts when a controlled,
  permitted fixture is available. Report skipped privileged coverage.
- Ratatui test-backend behavior plus a focused PTY suite for navigation,
  confirmation/cancel, read-only mode, resize, and terminal restoration.

Use deterministic test seams for mutation/failure scenarios rather than
sleep-based races. These tests verify the stated behavior, not an impossible
claim of atomic inode-conditional deletion.

Run actual Linux and macOS tests where available. Cross-compilation is not
native runtime verification; report unavailable coverage accurately.

9. PERFORMANCE AND DELIVERY

Record a pre-change baseline using the existing fixture/profiling approach.

Compare:
- Original and refactored summary scanning.
- Headless indexed scanning.
- Indexed scanning with the live TUI.

Measure elapsed/CPU time, peak memory, allocation behavior, retained bytes
per entry, queue high-water marks, time to first usable listing, and
input-to-render latency under scanning load. Use syscall profiling where
available. Exercise a very wide directory specifically.

Keep allocation instrumentation optional. Investigate material summary
regressions and distinguish necessary boundary-check costs from architectural
overhead. Report indexed-mode memory costs honestly; a retained browser
cannot have the same memory footprint as a streaming reduction.

Benchmark deletion only on disposable generated fixtures. Do not add
parallel deletion merely to improve a synthetic benchmark.

Do not use flaky absolute timing assertions in CI or declare success from
cargo check and one wall-clock result.

Out of scope:
- Cross-directory selection or overlapping filesystem-operation phases.
- Trash, undo, rollback, shell launching, external file-manager actions.
- Persistent indexes, import/export, watchers, incremental refresh.
- Shared/unique-space accounting or advanced filesystem introspection.
- New io_uring/APFS optimization projects.
- Additional platform support beyond Linux and macOS.

Finish with updated usage and architecture documentation, tests actually
run, measured performance results, and clearly identified remaining gaps.
Keep the result a focused disk-cleanup tool, not a framework.
