# Deletion

Interactive deletion and `--rmrf` build a plan from the completed indexed scan
and use the descriptor-relative engine in
[`fdu-delete`](../crates/fdu-delete/src/lib.rs).

## Eligibility and outcomes

A plan may contain several entries, but its roots cannot overlap. A directory
can be selected only when its indexed subtree is complete and has no traversal
exclusions or errors. The shared planner also requires the selected root scan
to be complete, so a complete indexed file or symlink cannot be selected when
another part of the scan is incomplete. Browser deletion calls the shared
planner when Ctrl-R is pressed. `--rmrf` checks for an incomplete root scan
before planning and rejects the same condition.

The engine opens each parent relative to an already-open descriptor. It moves
each planned entry into a private quarantine on the root mount using an
exclusive rename. It validates the captured identity and type before unlinking
the captured name. Discovered symlinks remain leaves. New names are never added
to the plan, and unindexed children prevent directory removal.

If the captured identity or type differs from the index, the engine restores
the entry without overwriting its source name. A restoration conflict retains
both entries and reports the capture location. A directory that gains unindexed
children is restored using the same rule.

Concurrent changes to the source tree are supported. Other processes must
leave fdu's private quarantine directories, their contents, and recovery
records untouched, including through previously opened directory descriptors. Mode `0700` limits
access by other users but cannot enforce this precondition against the owner.
The scan root must be writable to create the quarantine.

Each planned entry gets one outcome: deleted, already absent, changed, failed,
or not attempted. Esc stops scheduling later removals in the browser.
Completed removals cannot be restored. Reported byte totals describe metadata
and do not promise how much storage the filesystem will release.

## Interruption and recovery

Quarantine directories use the prefix `.fdu-quarantine-v1-` beneath the scan
root. A versioned header identifies the root. Each capture has a versioned
intent containing its original name and the identities of its parent chain.
The engine persists the intent before capture. It synchronizes the affected
directories before removing the capture or clearing its intent.

Recovery runs during a confirmed deletion request, before scheduling removals.
Browsing and scanning do not restore entries. Cooperating deletion and recovery
sessions use an exclusive, nonblocking advisory lock on the open root directory.
If recovery finds an interrupted session, it restores captures and stops the
current deletion with a request to rescan.

Recovery never deletes captures. It validates the root and parent identities
and restores only into an absent source name. Conflicts, changed parents,
corrupt records, and unknown versions retain the workspace and report an error.
Inspect the reported location and restore retained entries without overwriting
source names, then rescan. Completed deletions remain permanent.

Directories with the quarantine prefix and a missing or invalid header also
block deletion. Move unrelated lookalikes out of that prefix. A process exit
during initial creation or final cleanup can leave an ambiguous directory that
requires inspection. No capture precedes a durable header, and final header
removal follows capture and intent cleanup.

Process-exit tests cover intent publication, capture, restoration, and removal.
Physical power-loss behavior and native macOS behavior have not been verified.

## `--rmrf`

`--rmrf` selects all immediate children of the explicit root and leaves that
root in place. See the [README](../README.md#empty-a-directory) for the
user-facing root safeguards and confirmation. An incomplete root scan or an
ineligible selected entry rejects the whole operation before deletion starts.

The production engine uses a fixed 16-thread Rayon pool for independent
selected directories. It walks each selected directory depth first and reopens
parents as it ascends, so path depth does not increase the number of open
descriptors.
