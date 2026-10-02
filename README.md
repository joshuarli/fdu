# fdu

A Linux 6.0+ disk usage counter for directory trees. It uses only libc and Rayon as direct dependencies.

    fdu [--apparent] [PATH]

The default path is the current directory. FDU prints one row for each immediate child directory, sorted by size descending. Each row is a raw byte count, a tab, and the quoted directory name with standard Rust debug escaping. Files directly under PATH are omitted from the output. Each directory's count sums the non-directory entries below it; directory inode metadata is not added.

By default, sizes are allocated bytes (stx_blocks * 512); --apparent selects logical file lengths. Symlinks are counted without following them, and hardlinked paths are counted separately. The scan assumes a single filesystem and does not check mount boundaries.

Entries that cannot be read or whose byte count cannot fit in u64 are omitted from their containing total. FDU reports skipped entries on standard error and marks totals as potentially incomplete. Root-level open and enumeration failures abort the scan.

Interrupted filesystem calls are retried. The default Rayon pool has four workers; set RAYON_NUM_THREADS to choose another count.

The scanner reads a live directory tree, not an atomic filesystem snapshot. Changes made during traversal may be observed at different points in time. Metadata queries do not force synchronization with remote filesystems, so remote results may reflect cached metadata.

To create a repeatable, inode-heavy ext4 workload, use the generator:

    python3 scripts/generate_inode_fixture.py perf/fixture
    cargo build --locked --release
    target/release/fdu perf/fixture

The default fixture contains 256 top-level directories, 330 nested directories per top-level directory, and 13 tiny files per nested directory. It also creates four top-level directories with 30,000 files each and eight deep chains with 128 nested levels apiece. In total, that is 85,772 directories and 1,218,248 files, or 1,304,020 entries. About one in sixteen files contains one, four, or eight bytes; the rest are empty. The wide directories exercise large getdents batches, and the deep chains cross the scanner's recursive-to-iterative boundary. Scale the counts when the filesystem has room for more inodes. The generated perf/fixture directory is gitignored.

For a larger run with about two million files:

    python3 scripts/generate_inode_fixture.py perf/fixture --top-level-dirs 128 --subdirs-per-directory 32 --files-per-subdirectory 512
