#!/usr/bin/env python3

import argparse
import os
from pathlib import Path


SMALL_PAYLOADS = (b"x", b"tiny", b"12345678")


def positive_count(value: str) -> int:
    try:
        count = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be an integer") from error
    if count < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return count


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Generate a shallow, inode-heavy directory tree with tiny files."
    )
    parser.add_argument("root", type=Path, help="new output directory")
    parser.add_argument("--top-level-dirs", type=positive_count, default=256)
    parser.add_argument("--subdirs-per-directory", type=positive_count, default=330)
    parser.add_argument("--files-per-subdirectory", type=positive_count, default=13)
    parser.add_argument(
        "--nonempty-every",
        type=int,
        default=16,
        help="write a small payload to every Nth file; use 0 for all-empty files",
    )
    args = parser.parse_args()

    if args.nonempty_every < 0:
        parser.error("--nonempty-every must be zero or greater")
    if args.root.exists():
        parser.error(f"output path already exists: {args.root}")

    args.root.mkdir(parents=True)
    file_count = 0
    nonempty_count = 0
    directory_count = 0
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    for top_index in range(args.top_level_dirs):
        top = args.root / f"top-{top_index:04d}"
        top.mkdir()
        directory_count += 1
        for sub_index in range(args.subdirs_per_directory):
            subdirectory = top / f"sub-{sub_index:04d}"
            subdirectory.mkdir()
            directory_count += 1
            for file_index in range(args.files_per_subdirectory):
                path = subdirectory / f"file-{file_index:06d}"
                fd = os.open(path, flags, 0o644)
                try:
                    if (
                        args.nonempty_every
                        and file_count % args.nonempty_every == 0
                    ):
                        payload = SMALL_PAYLOADS[
                            (file_count // args.nonempty_every) % len(SMALL_PAYLOADS)
                        ]
                        os.write(fd, payload)
                        nonempty_count += 1
                finally:
                    os.close(fd)
                file_count += 1

    print(
        f"directories={directory_count} files={file_count} entries={directory_count + file_count} "
        f"nonempty_files={nonempty_count} root={args.root}"
    )


if __name__ == "__main__":
    main()
