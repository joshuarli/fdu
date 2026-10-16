# FDU-1: Concurrent replacement can unlink an unindexed entry

Status: accepted

The [later isolation-boundary decision](FDU-1-deletion-isolation-boundary.md)
supersedes the concurrency boundary recorded here.

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

### Prototype results

A Linux prototype tested atomic `renameat2(RENAME_NOREPLACE)` capture into a
same-filesystem directory with mode `0700`. It checked the captured device,
inode, and entry type before descriptor-relative removal. Tests covered files
and empty directories.

Source replacement before capture produced an identity mismatch. Restoration
with `RENAME_NOREPLACE` restored the captured entry when the source name was
absent. When another entry occupied that name, restoration retained both
entries without overwriting either.

A separate process running as the same user replaced the captured name after
validation and before unlink. Socket handshakes fixed the interleaving without
sleeps. The unlink removed the unindexed replacement, while the indexed entry
survived under another name. An open descriptor to the replacement reported a
zero link count after removal. This happened for both files and empty
directories. Mode `0700` does not isolate a directory from its owner's other
processes, so this quarantine design fails the accepted contract.

Killing the capturing process after its rename left the captured entry intact.
The surviving parent restored it when the source name was absent and retained
both entries on a restoration conflict. These tests used the parent's known
source mapping and identity. They do not establish recovery after all processes
exit, protection of recovery metadata, or power-loss durability.

## Alternatives considered

- Repeat the metadata check immediately before `unlinkat`. The race remains
  between the final check and name resolution by `unlinkat`.
- Open the victim and validate its descriptor before unlinking. The descriptor
  pins the opened object, but `unlinkat` still resolves the name independently.
- Keep the current behavior and require the namespace not to change during
  deletion. This weakens the repository invariant and leaves the replacement
  deletion behavior as an explicit precondition.

## Open questions

- Which removal primitive or isolation mechanism binds removal to the indexed
  identity despite concurrent namespace mutation?
- If a future design captures entries before removal, how does recovery find
  and validate them after all participating processes exit?

## Decision

On 2026-10-16, the owner accepted both recommendations: preserve the indexed-only
deletion contract under arbitrary concurrent namespace mutation, and prototype
quarantine before selecting an implementation.

The prototype rejects mode-`0700` quarantine as sufficient protection. The
accepted contract remains unchanged. No production removal design is selected,
and the existing deletion race remains open.
