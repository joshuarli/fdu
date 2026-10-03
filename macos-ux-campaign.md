# macOS UX campaign

## Goal

Make the macOS terminal browser comfortable to explore, clear about what is
selected, and difficult to use for an accidental permanent deletion. Keep
browsing inside the directory supplied at startup: allow navigation into
subdirectories, stop parent navigation at that starting directory, and never
follow discovered symlinks. Iterate in the real terminal until the interaction
and layout feel right. After that, solidify the agreed behavior with
`../ptytest`.

`ptytest` will check terminal-visible behavior and lifecycle; it will not
replace trying the interface in the user's actual macOS terminal.

## Visual reference target

Use `dua i` as the visual reference and aim for a close match in layout,
information density, pane focus, selection display, status placement, help, and
deletion review/progress. Match the parts that make the interface feel
cohesive, while keeping fdu's disk browser and safety rules.

Treat this as a layout redesign, not a palette pass: match the title strip,
framed size/bar/path rows, right-hand marked-items pane with count and totals,
and bottom key-hint and status strips. Use Tab for pane focus and `]` to
collapse or restore the right side.

Reference captures from `dua i` v2.45.0 are in
[ux-reference/dua-v2.45.0](ux-reference/dua-v2.45.0/README.md). They use a
synthetic directory tree and include ready, help, marked-items, pane-focus,
and narrow-layout states. Use the same dimensions and equivalent fdu fixture
for comparisons. These PTY captures preserve terminal cells and attributes;
the live macOS UX pass will also check the actual terminal's font and theme.

Capture fdu's own delete review and progress states in the live UX pass because
those operations have different semantics. Match the visual treatment without
copying dua's unprompted delete action.

Do not bring over dua cleanup-specific concepts or status labels such as
`cleanup` and `gitignored` counts. Match the visual treatment with fdu-relevant
information: root and current path, sizes, marks, scan state, exclusions,
read-only state, and deletion results. Keep deletion permanent and retain an
explicit confirmation before work starts.

## Current behavior

- A macOS terminal session opens the interactive browser: a minimal title
  strip, a plainly framed listing (size, share of the directory, bar, name), a
  marked-items pane, and a status strip that leads with the entry count
  (ending in `…` while scanning), then transient messages and key hints for
  the focused pane. `?` opens help.
  The focused pane is drawn at normal weight and the other dimmed; `[x]` marks
  and `[=]` covered rows do not depend on color.
- Navigation can descend into indexed directories and return to the opened
  root, but cannot move above it. Discovered symlinks are selectable leaves and
  are never followed.
- Scanning results appear as they arrive. Browsing and marking remain available
  during scanning; delete and refresh wait for scanning to finish.
- `d` marks; Space does nothing. Marks are a root-wide basket. They survive
  filtering, sorting, and moving between directories, and clear on rescan or
  after a deletion. Overlapping marks are refused with a reason. The pane lists
  marks by root-relative path, including ones a filter hides, and Enter in the
  pane shows a mark in its directory.
- The marked pane opens with the first mark and closes with the last. Tab
  switches panes and `-` toggles side-by-side and stacked layouts. When the
  terminal is too small for both (under 60 columns side by side, under 12 rows
  stacked) only the focused pane is shown.
- Ctrl-R in the marked pane asks for confirmation in the status strip (there
  is no modal); the pane is the review. Enter starts permanent deletion and Esc
  withdraws the question. If any mark is ineligible nothing is deleted or
  narrowed and the status strip names the first reason.
- While deleting, the browser ignores every command except Esc. Esc asks the
  worker to stop scheduling work after the current operation; it cannot restore
  entries already removed. The status strip shows progress, then a summary of
  deleted, already absent, changed, failed, and not attempted entries.
- The interface is keyboard-first, with `?` help, a name filter, `s` to toggle
  size/name order, `a` for allocated/apparent sizes, range marks, and a
  read-only mode.

## UX anchors

### Focusable selection pane

Add a right-hand pane that shows the entries marked for deletion. Tab toggles
focus between the browser list and this pane; `]` hides or restores the right
side. Make the focused pane obvious and show pane-specific controls. The
selection pane should expose marks hidden by the current filter and let the
user review or remove a mark without navigating back through the listing.

Marks should survive navigation among indexed subdirectories and stay
confined to the startup root. Show each selected path relative to that root.
This lets the user explore the tree and collect deletions from multiple
directories before review.

The pane should show the selected count and size totals. Keep directory
deletion scope understandable: a selected directory represents its eligible
indexed subtree, while its displayed size is the current metadata total.
Prevent overlapping roots: a marked directory covers all its descendants, and
an already marked descendant prevents adding its ancestor until that mark is
removed. Show the reason when an entry is already covered or cannot be added.

### Exact review before deletion

Keep an explicit confirmation step after selection review. Show every selected
root, its type, size, and any reason it cannot be deleted. Do not silently
narrow an ineligible selection. Make the permanent nature of deletion and the
limits of the byte totals clear before Enter commits the operation.

### Exclusive deletion state

Treat deletion as a locked interaction state: no pane switching, navigation,
marking, sorting, filtering, refresh, or quit command should take effect while
the worker is running. Keep Esc as the one explicit stop request, and explain
that it stops later work rather than undoing completed removals. Keep progress
visible and finish with a useful summary of deleted, already-absent, changed,
failed, and not-attempted entries.

### Responsive terminal layout

Try the two-pane layout at wide, typical, and narrow terminal sizes. At narrow
widths, keep the focused pane readable and reachable without crushing the main
listing. Resize should preserve focus, selection, and a sensible cursor
position. Verify long names, deep breadcrumbs, empty directories, and tiny
terminal heights.

Use selection and status cues that remain clear without relying on color alone.
Check both light and dark macOS terminal themes, wide and combining Unicode,
and escaped control or invalid filename bytes. Keep sizes and names legible when
the row is clipped.

### Discoverability and state clarity

Keep the root path, current path, allocated/apparent mode, read-only state,
selection count, and operation state easy to distinguish. Update the footer and
help for the focused pane. Make scanning, incomplete data, excluded entries,
deletion progress, and the final result visible without requiring the user to
infer state from color.

## Campaign decisions

- Deletions are always permanent. Keep that explicit in the review and
  confirmation; there is no Finder Trash path.
- The marked-items pane is a root-confined basket spanning subdirectories.
  Navigating above the startup root is unavailable, and discovered symlinks
  remain leaves that are never followed.
- Do not permit overlapping selected roots. A directory selection covers its
  indexed subtree; explain when an existing mark blocks adding an ancestor or
  when an ancestor already covers the current entry.
- Use a review pane before the final confirmation. Keep every marked root
  inspectable, show ineligibility reasons, and never silently narrow the
  selection.
- At narrow widths, collapse the right side and make it available with the
  same `]` control. Tab cycles only through visible panes. Preserve focus when
  resizing.

## Campaign sequence

1. Use the captured `dua i` frames to agree on fdu's layout, row density,
   selection pane, and focus behavior. Capture additional reference states if
   the live visual review needs them.
2. Implement the UX changes in small pieces and try them in the actual terminal
   until the user is happy with the visual hierarchy, key flow, and deletion
   feedback.
3. Once that design is accepted, use `../ptytest` for semantic screen and
   lifecycle coverage. Cover startup and live scan, pane focus with Tab,
   marking and removing selections, filters that hide marks, resize, read-only
   behavior, ineligible selections, confirmation cancellation, deletion lock
   behavior, progress and results, and terminal restoration on exit.
4. Keep the existing macOS-only scope explicit when reporting test results;
   terminal behavior on Linux is not evidence for macOS behavior.

## Inspiration

The visual reference is [dua-cli's interactive mode](https://github.com/Byron/dua-cli).
Its documentation describes Tab navigation between visible panes and a
minimizable right side. Use its frames to guide fdu's visual hierarchy, while
keeping the content and operations specific to fdu.
