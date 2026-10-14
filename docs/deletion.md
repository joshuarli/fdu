# Deletion

Interactive deletion and `--rmrf` build a plan from the completed indexed scan
and use the descriptor-relative engine in
[`fdu-delete`](../crates/fdu-delete/src/lib.rs).

## Eligibility and outcomes

A plan may contain several entries, but its roots cannot overlap. A directory
can be selected only when its indexed subtree is complete and has no traversal
exclusions or errors. The browser currently allows a complete indexed file or
symlink to be selected even when an unrelated scan error makes the root
incomplete. `--rmrf` separately rejects an incomplete root scan.

The engine opens each parent directory relative to an already-open descriptor
and checks an entry's expected identity and type before unlinking it. New names
that were not indexed are not added to the plan. Each planned entry gets one
outcome: deleted, already absent, changed, failed, or not attempted. In the
browser, Esc stops scheduling later removals; completed removals cannot be
restored.

The identity check and `unlinkat` are separate operations. If another process
replaces a planned name between them, `unlinkat` can remove the replacement and
report the indexed entry as deleted. This can happen without inode reuse.
Reported byte totals describe metadata and do not promise how much storage the
filesystem will release.

## `--rmrf`

`--rmrf` selects all immediate children of the explicit root and leaves that
root in place. See the [README](../README.md#empty-a-directory) for the
user-facing root safeguards and confirmation. An incomplete root scan or an
ineligible selected entry rejects the whole operation before deletion starts.

The engine currently uses a fixed 16-thread Rayon pool. Directory removal often
waits on filesystem work, so deletion can benefit from more workers than the
number of CPU cores.
