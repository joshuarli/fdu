# FDU-1: Choose the deletion isolation boundary

Status: accepted

## Summary

The previously accepted contract covers arbitrary concurrent namespace mutation, including
changes made by another process running as the same user. The investigated
Linux and macOS APIs do not provide the identity-bound removal that contract
requires for files, directories, and symlinks.

Decision: permit concurrent changes to the scanned tree, but require
other processes to leave fdu's private quarantine directories, their contents,
and recovery records untouched. The owner approved this narrower contract on 2026-10-16.
It does not provide protection against a same-user process that interferes with
the quarantine, including through previously opened directory descriptors.

## Motivation

Linux `unlinkat` resolves the current name under a directory descriptor. It
accepts `AT_REMOVEDIR`, but neither an expected inode nor `AT_EMPTY_PATH`.
The [Linux implementation](https://github.com/torvalds/linux/blob/master/fs/namei.c)
has no identity condition supplied by the caller. The
[UAPI feature proposal](https://uapi-group.org/kernel-features/#unlinking-via-two-file-descriptors)
describes the missing conditional operation as `unlinkat2(dir_fd, name, inode_fd)`.

The [macOS unlink implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_syscalls.c)
also resolves a pathname. Its public interface has no expected identity.
Guarded descriptors and exclusive renames do not establish the required
exclusion for all supported entry types. No suitable supported primitive was
found in the investigated APIs. This is not a proof about every possible
filesystem-specific or undocumented interface.

The quarantine prototype demonstrated the remaining race with two processes
running as the same user. After capture and validation, the second process
replaced the captured name. The first process then unlinked that unindexed
replacement. Mode `0700` and descriptor-relative operations did not prevent it.

## Design

The implementation follows these rules:

1. Create a private quarantine on the source mount. Record recovery intent
   before moving an entry out of its original location.
2. Atomically capture the current source name without overwriting another
   capture. Validate the captured entry's indexed identity and type before
   removing it. Keep discovered symlinks as leaves.
3. On a mismatch, restore the captured entry without overwriting a concurrent
   source replacement. If restoration conflicts, retain both entries and report
   the retained capture's location.
4. Remove only indexed children before capturing a directory itself. Unindexed
   children prevent directory removal and must remain available for recovery.
5. Recover interrupted captures without deleting them automatically. Validate
   the recorded parent identity and restore only into an absent original name.
   Retain captures on identity changes, malformed records, or name conflicts.
6. Test interruption between intent persistence, capture, validation, removal,
   and cleanup. Define directory synchronization before claiming power-loss
   recovery. A surviving parent's memory is insufficient for restart recovery.

The source tree may change concurrently before capture. Throughout deletion
and recovery, other processes must not rename or replace the quarantine directory.
They must leave its contents and recovery records untouched. The deletion
documentation and hard invariant state this precondition. Permissions limit access by other users but do not
enforce the precondition against processes running as the owner.

The private quarantine uses a versioned durable header and per-capture intents.
Exclusive root-directory locks coordinate fdu sessions. Recovery runs only after
a deletion request is confirmed, restores captures, and rejects the stale plan.
Ambiguous directories with the quarantine prefix remain untouched and block
deletion until inspected. See [deletion recovery](../deletion.md#interruption-and-recovery).

## Alternatives considered

- Preserve the accepted contract with a privileged helper that owns a
  same-mount quarantine inaccessible to peer processes. This adds a security
  boundary, installation requirements, and a recovery service. It requires a
  separate design and authorization for privileged setup.
- Preserve the accepted contract and require a kernel or filesystem primitive
  that compares the expected identity while removing the entry. No suitable
  supported cross-platform primitive was found. The existing deployment
  requirements therefore cannot currently support this implementation path.
- Repeat metadata checks, use advisory locks, or choose unpredictable quarantine
  names. These do not close the demonstrated check-to-unlink interval under
  arbitrary same-user interference.
- Disable deletion. This avoids removing replacements by removing the feature,
  and does not satisfy the requested fix.

## Decision

The owner approved the quarantine precondition on 2026-10-16. This decision
supersedes the unrestricted concurrency boundary in the
[earlier accepted RFC](FDU-1-concurrent-replacement-deletion.md).
Concurrent mutation of source names remains supported. Modification of private
quarantine directories, their contents, or recovery records is outside the
supported contract.

The implementation adds no dependencies or privileged helper. Deterministic
file and directory replacement regressions cover the original race.
Process-exit recovery tests exercise the durable records without a surviving
in-memory map. Native macOS and physical power-loss behavior remain unverified.
