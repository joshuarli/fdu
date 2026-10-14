# FDU-1: Concurrent replacement can unlink an unindexed entry

Status: draft

## Summary

Deletion validates a directory entry's indexed identity and then removes its
current name in a separate operation. Another process can replace that name in
the gap, causing fdu to remove the replacement. The observed behavior conflicts
with the repository invariant that deletion attempts only indexed entries.

## Motivation

Both browser deletion and `--rmrf` use the same name-based unlink path after an
identity check. A deterministic experiment against the real engine paused at
the unlink boundary, replaced the indexed file, and resumed the operation. Fdu
reported the indexed entry as deleted and removed the replacement. Existing
tests replace entries before execution and do not exercise this interleaving.

## Design

Any implementation that preserves the current invariant must bind the object
being removed to the indexed identity across the removal operation. A second
metadata check, opening the victim before unlink, or an internal lock does not
bind a later name-based unlink to that object.

One design to investigate is same-filesystem quarantine: atomically move the
current name into a protected quarantine namespace, validate the captured
entry, and remove it only if it matches the index. A mismatch must be restored
without overwriting a concurrent replacement. The design must define how the
quarantine namespace is protected and how mismatches or process crashes are
recovered. No portable unconditional compare-inode-and-unlink operation has
been established.

## Alternatives considered

- Repeat the metadata check immediately before `unlinkat`. The race remains
  between the final check and name resolution by `unlinkat`.
- Open the victim and validate its descriptor before unlinking. The descriptor
  pins the opened object, but `unlinkat` still resolves the name independently.
- Keep the current behavior and require the namespace not to change during
  deletion. This weakens the repository invariant and leaves the replacement
  deletion behavior as an explicit precondition.

## Open questions

- Should the hard contract cover arbitrary concurrent namespace mutation, as
  the current indexed-entry invariant implies? Recommendation: yes.
- Is the visible rename and conflict-safe crash recovery required by a
  quarantine design acceptable? Recommendation: prototype it before selecting
  it; do not claim the race is closed until mismatch restoration and recovery
  are defined.

## Decision

Pending owner decision on the concurrency contract and an implementation design
that satisfies it.
