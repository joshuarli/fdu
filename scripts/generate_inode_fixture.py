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
        description="Generate a mixed-depth, inode-heavy tree with tiny files."
    )
    parser.add_argument("root", type=Path, help="new output directory")
    parser.add_argument("--top-level-dirs", type=positive_count, default=256)
    parser.add_argument("--subdirs-per-directory", type=positive_count, default=330)
    parser.add_argument("--files-per-subdirectory", type=positive_count, default=13)
    parser.add_argument("--wide-directory-count", type=int, default=4)
    parser.add_argument("--entries-per-wide-directory", type=positive_count, default=30_000)
    parser.add_argument("--deep-chain-count", type=int, default=8)
    parser.add_argument("--depth-per-chain", type=positive_count, default=128)
    parser.add_argument(
        "--nonempty-every",
        type=int,
        default=16,
        help="write a small payload to every Nth file; use 0 for all-empty files",
    )
    args = parser.parse_args()

    if args.nonempty_every < 0:
        parser.error("--nonempty-every must be zero or greater")
    if args.wide_directory_count < 0:
        parser.error("--wide-directory-count must be zero or greater")
    if args.deep_chain_count < 0:
        parser.error("--deep-chain-count must be zero or greater")
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

    for wide_index in range(args.wide_directory_count):
        wide_directory = args.root / f"wide-{wide_index:04d}"
        wide_directory.mkdir()
        directory_count += 1
        for file_index in range(args.entries_per_wide_directory):
            path = wide_directory / f"entry-{file_index:05d}"
            fd = os.open(path, flags, 0o644)
            try:
                if args.nonempty_every and file_count % args.nonempty_every == 0:
                    payload = SMALL_PAYLOADS[
                        (file_count // args.nonempty_every) % len(SMALL_PAYLOADS)
                    ]
                    os.write(fd, payload)
                    nonempty_count += 1
            finally:
                os.close(fd)
            file_count += 1

    for chain_index in range(args.deep_chain_count):
        current = args.root / f"deep-{chain_index:04d}"
        current.mkdir()
        directory_count += 1
        for depth_index in range(args.depth_per_chain):
            current = current / f"level-{depth_index:04d}"
            current.mkdir()
            directory_count += 1

        path = current / "leaf"
        fd = os.open(path, flags, 0o644)
        try:
            if args.nonempty_every and file_count % args.nonempty_every == 0:
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
        f"wide_directories={args.wide_directory_count} "
        f"entries_per_wide_directory={args.entries_per_wide_directory} "
        f"deep_chains={args.deep_chain_count} depth_per_chain={args.depth_per_chain} "
        f"nonempty_files={nonempty_count} root={args.root}"
    )


if __name__ == "__main__":
    main()
