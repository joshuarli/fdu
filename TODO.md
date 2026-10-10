# TODO

## Try `rustc-hash` against hashbrown's default hasher

All hash tables use `hashbrown` with its default hasher (foldhash). Keys are trusted local filesystem data, so a faster non-cryptographic hasher is acceptable.

- Benchmark `hashbrown::HashSet<FileIdentity, rustc_hash::FxBuildHasher>` (dev/ino keys) in the indexed scan's seen-directory shards in `crates/fdu-scan/src/indexed.rs`. This is the hottest table.
- Then try it for the `NodeId` sets and maps in `fdu-delete`, `fdu-tui` and `src/browser.rs`.
- Fx can cluster badly on aligned or strided IDs (inode numbers may be), so measure on a large real tree, not a synthetic loop.
- Needs a quiet host. Run it when timings are stable.
- Adopt it only if it wins on wall-clock time. Otherwise keep foldhash and add no dependency.

## Check whether `NodeId` is dense

If `NodeId`s are dense indexes into the tree, hashing may be unnecessary:

- `status_by_node: HashMap<NodeId, String>` in `src/browser.rs` could be a `Vec<Option<String>>` indexed by `NodeId`.
- The marks sets (`HashSet<NodeId>` in `src/browser.rs`, `fdu-tui` and `fdu-delete`) could be a bitset or `Vec<bool>`.

First confirm how `NodeId`s are allocated in `fdu-core` and whether they stay dense after deletions. Try this before the Fx experiment, since it could remove the hashing entirely for these collections.
