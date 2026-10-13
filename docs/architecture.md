# Architecture

The command line selects the summary scanner or terminal browser. The browser
uses the indexed scanner; `--rmrf` uses the same index and deletion engine
without the terminal.

## Components

- [`fdu-core`](../crates/fdu-core/src/lib.rs) owns compact tree records, node
  IDs, size totals, and scan events. It has no filesystem or terminal I/O.
- [`fdu-scan`](../crates/fdu-scan/src/lib.rs) implements summary and indexed
  scans plus the Linux and macOS backends.
- [`fdu-delete`](../crates/fdu-delete/src/lib.rs) validates and removes
  entries from a completed index.
- [`fdu-tui`](../crates/fdu-tui/src/lib.rs) maps terminal input to intents and
  renders borrowed browser state; it performs no filesystem I/O.
- [`src/main.rs`](../src/main.rs) parses options and chooses a mode.
  [`src/browser.rs`](../src/browser.rs) owns browser state and coordinates
  scans, refreshes, and deletion.

## Browser data flow

The browser owns the mutable tree. The indexed scan sends bounded batches and
completion events to it; the browser appends entries to an arena and keeps
filename bytes in packed storage. A refresh replaces the index.

`NodeStatus` represents states accepted when appending or updating a node.
Removal uses `Tree::tombstone_subtree`, which also subtracts the removed
subtree's byte totals from its ancestors.

The operation phases are scanning, ready, and deleting. Scan and deletion
workers finish and their results are consumed before the browser changes phase.
The indexed scanner is shared across platforms; each backend supplies directory
enumeration, metadata, mount identity, and worker limits. Linux summary scans
use a separate Rayon traversal.

## Event ordering

The browser relies on each directory entry arriving before entries inside that
directory, and on a directory being reported complete only after its subtree.
The scanner publishes entry batches before making discovered directories
available to other workers. Keep that ordering contract intact when changing
the scan protocol.

See [scanning](scanning.md) and [deletion](deletion.md) for subsystem details.
