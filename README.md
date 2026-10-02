# fdu

A small parallel disk usage scanner for Linux 6.0 and newer. It uses the Linux directory and metadata syscalls directly and has only two direct dependencies: `libc` and Rayon.

```text
fdu [--apparent] [PATH]
```

The default path is the current directory. By default, sizes are allocated bytes (`stx_blocks * 512`); `--apparent` selects logical file lengths. Output is an indented tree with raw byte counts. Tabs, newlines, carriage returns, and backslashes use `\t`, `\n`, `\r`, and `\\`; other control characters use `\u{hex}`, and invalid UTF-8 name bytes use `\xNN`. This keeps each item on one unambiguous line. Symlinks are counted without following them, hardlinked paths are counted separately, and traversal stops when a child directory is on another device.

Entries that cannot be read are omitted. `fdu` reports the number of skipped entries on standard error and marks the totals as potentially incomplete.

The default Rayon pool has at most four workers. Set `RAYON_NUM_THREADS` to choose another count.
