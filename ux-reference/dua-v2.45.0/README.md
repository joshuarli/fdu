# `dua i` visual references

Captured from `dua i` v2.45.0 on macOS through a PTY, using a synthetic tree
with `Projects`, `Media`, and `Logs` entries. Frames are 120 columns by 36 rows
or 80 columns by 24 rows and include semantic cells plus terminal attributes.
They are terminal-model captures, not pixel screenshots, so they do not capture
the host terminal's font or theme rendering.

- [Ready listing](ready-120x36.ptytest): initial single-pane view.
- [Help pane](help-120x36.ptytest): `?` from the ready listing.
- [Help closed](closed-help-120x36.ptytest): `q` in Help.
- [Marked items](marked-120x36.ptytest): Space on the selected directory.
- [Marked pane focused](tab-120x36.ptytest): Tab after marking.
- [Narrow ready listing](ready-80x24.ptytest): initial single-pane view.
- [Narrow marked items](marked-80x24.ptytest): Space on the selected directory.

The narrow marked-items frame shows the split panes squeezing path names and
the totals. Use it to see the pressure point; fdu should preserve the pane
design while keeping its narrow layout readable.

The reference includes dua-specific cleanup and gitignore controls in its
footer. They are present here only because these are captures of dua; fdu's
footer should describe fdu operations.

The marked-pane frame says Ctrl-R deletes without prompting. Fdu will keep its
explicit confirmation because deletion is permanent.
