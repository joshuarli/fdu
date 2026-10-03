#!/usr/bin/env python3

"""Generate a synthetic filesystem tree from an anonymized layout profile."""

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import random
import shutil


DEFAULT_PROFILE = (
    Path(__file__).resolve().parents[1]
    / "perf"
    / "fixture_profiles"
    / "workspace-layout.json"
)
SMALL_PAYLOADS = (b"x", b"tiny", b"12345678")


@dataclass(frozen=True)
class DirectoryLayout:
    child_directories: int
    regular_files: int
    symbolic_links: int
    special_entries: int


@dataclass
class GeneratedCounts:
    directories: int = 1
    regular_files: int = 0
    symbolic_links: int = 0
    special_entries: int = 0
    nonempty_files: int = 0


def nonnegative_count(value: str) -> int:
    try:
        count = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be an integer") from error
    if count < 0:
        raise argparse.ArgumentTypeError("must be zero or greater")
    return count


def profile_count(value: object, field: str, *, positive: bool = False) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"profile field {field!r} must be an integer")
    if value < (1 if positive else 0):
        qualifier = "positive" if positive else "nonnegative"
        raise ValueError(f"profile field {field!r} must be {qualifier}")
    return value


def read_profile(path: Path) -> tuple[list[list[DirectoryLayout]], list[int]]:
    try:
        profile = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read layout profile: {error}") from error
    if not isinstance(profile, dict) or profile.get("schema_version") != 1:
        raise ValueError("layout profile has an unsupported schema version")

    directory_counts = profile.get("directory_counts_by_depth")
    raw_layers = profile.get("layouts_by_depth")
    if not isinstance(directory_counts, list) or not isinstance(raw_layers, list):
        raise ValueError("layout profile is missing its depth tables")
    if not directory_counts or len(directory_counts) != len(raw_layers):
        raise ValueError("layout profile depth tables do not match")

    counts = [
        profile_count(count, f"directory_counts_by_depth[{depth}]")
        for depth, count in enumerate(directory_counts)
    ]
    layouts_by_depth: list[list[DirectoryLayout]] = []
    for depth, raw_layer in enumerate(raw_layers):
        if not isinstance(raw_layer, list):
            raise ValueError(f"layout profile depth {depth} is not a list")
        layouts: list[DirectoryLayout] = []
        for row in raw_layer:
            if not isinstance(row, dict):
                raise ValueError(f"layout profile depth {depth} has a malformed row")
            layout = DirectoryLayout(
                child_directories=profile_count(
                    row.get("child_directories"), "child_directories"
                ),
                regular_files=profile_count(row.get("regular_files"), "regular_files"),
                symbolic_links=profile_count(
                    row.get("symbolic_links"), "symbolic_links"
                ),
                special_entries=profile_count(
                    row.get("special_entries"), "special_entries"
                ),
            )
            frequency = profile_count(row.get("frequency"), "frequency", positive=True)
            layouts.extend([layout] * frequency)
        if len(layouts) != counts[depth]:
            raise ValueError(f"layout profile directory count is inconsistent at depth {depth}")
        layouts_by_depth.append(layouts)

    if counts[0] != 1:
        raise ValueError("layout profile must contain exactly one root directory")
    for depth, layouts in enumerate(layouts_by_depth):
        expected_children = counts[depth + 1] if depth + 1 < len(counts) else 0
        if sum(layout.child_directories for layout in layouts) != expected_children:
            raise ValueError(f"layout profile child count is inconsistent at depth {depth}")
    return layouts_by_depth, counts


def create_regular_files(
    directory: Path,
    count: int,
    first_file_index: int,
    nonempty_every: int,
    counts: GeneratedCounts,
) -> int:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    for index in range(count):
        path = directory / f"file-{index:06d}"
        descriptor = os.open(path, flags, 0o644)
        try:
            file_index = first_file_index + index
            if nonempty_every and file_index % nonempty_every == 0:
                payload = SMALL_PAYLOADS[
                    (file_index // nonempty_every) % len(SMALL_PAYLOADS)
                ]
                os.write(descriptor, payload)
                counts.nonempty_files += 1
        finally:
            os.close(descriptor)
    counts.regular_files += count
    return first_file_index + count


def populate_directory(
    directory: Path,
    layout: DirectoryLayout,
    first_file_index: int,
    nonempty_every: int,
    counts: GeneratedCounts,
) -> tuple[list[Path], int]:
    first_file_index = create_regular_files(
        directory,
        layout.regular_files,
        first_file_index,
        nonempty_every,
        counts,
    )
    for index in range(layout.symbolic_links):
        os.symlink("missing-target", directory / f"link-{index:06d}")
    counts.symbolic_links += layout.symbolic_links

    for index in range(layout.special_entries):
        os.mkfifo(directory / f"special-{index:06d}")
    counts.special_entries += layout.special_entries

    child_directories = []
    for index in range(layout.child_directories):
        child = directory / f"directory-{index:06d}"
        child.mkdir()
        child_directories.append(child)
    counts.directories += layout.child_directories
    return child_directories, first_file_index


def generate_fixture(
    root: Path,
    layouts_by_depth: list[list[DirectoryLayout]],
    nonempty_every: int,
    seed: int,
) -> GeneratedCounts:
    root.mkdir(parents=True)
    counts = GeneratedCounts()
    randomizer = random.Random(seed)
    # The profile keeps per-depth distributions without retaining parent links.
    for layouts in layouts_by_depth:
        randomizer.shuffle(layouts)

    directories = [root]
    file_index = 0
    try:
        for layouts in layouts_by_depth:
            if len(directories) != len(layouts):
                raise RuntimeError("layout profile directory count changed during generation")
            next_directories = []
            for directory, layout in zip(directories, layouts):
                children, file_index = populate_directory(
                    directory,
                    layout,
                    file_index,
                    nonempty_every,
                    counts,
                )
                next_directories.extend(children)
            directories = next_directories
        if directories:
            raise RuntimeError("layout profile ended before all directories were generated")
    except BaseException:
        shutil.rmtree(root)
        raise
    return counts


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Generate synthetic names and tiny files from an anonymized tree profile."
    )
    parser.add_argument("root", type=Path, help="new output directory")
    parser.add_argument("--profile", type=Path, default=DEFAULT_PROFILE)
    parser.add_argument("--nonempty-every", type=nonnegative_count, default=16)
    parser.add_argument("--seed", type=int, default=0)
    args = parser.parse_args()

    if args.root.exists():
        parser.error(f"output path already exists: {args.root}")
    try:
        layouts_by_depth, directory_counts = read_profile(args.profile)
    except ValueError as error:
        parser.error(str(error))

    counts = generate_fixture(
        args.root, layouts_by_depth, args.nonempty_every, args.seed
    )
    expected_files = sum(
        layout.regular_files for layer in layouts_by_depth for layout in layer
    )
    expected_links = sum(
        layout.symbolic_links for layer in layouts_by_depth for layout in layer
    )
    expected_special = sum(
        layout.special_entries for layer in layouts_by_depth for layout in layer
    )
    if (
        counts.directories != sum(directory_counts)
        or counts.regular_files != expected_files
        or counts.symbolic_links != expected_links
        or counts.special_entries != expected_special
    ):
        raise RuntimeError("generated fixture counts do not match the layout profile")

    entries = (
        counts.directories
        - 1
        + counts.regular_files
        + counts.symbolic_links
        + counts.special_entries
    )
    print(
        f"directories_including_root={counts.directories} regular_files={counts.regular_files} "
        f"symbolic_links={counts.symbolic_links} special_entries={counts.special_entries} "
        f"entries={entries} max_depth={len(directory_counts) - 1} "
        f"nonempty_files={counts.nonempty_files} root={args.root}"
    )


if __name__ == "__main__":
    main()
