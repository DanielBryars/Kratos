"""Verified manifests for the output tree of a finished job attempt."""

import base64
import hashlib
import os
import stat
from collections.abc import Iterator, Sequence
from pathlib import Path

import google_crc32c

from kratos_agent.models import ArtifactManifestFile, JobOutputRequirement

MAX_FILE_BYTES = 5 * 1024**3
MAX_TOTAL_BYTES = 10 * 1024**3
MAX_LOGICAL_PATH_BYTES = 240
MAX_INSPECTED_ENTRIES = 10_000
READ_CHUNK_BYTES = 1024 * 1024


class OutputError(RuntimeError):
    """The output tree cannot be declared as trustworthy artefacts."""


def valid_logical_path(path: str) -> bool:
    """Apply the control plane's logical-path rules, so it never refuses a manifest's paths."""
    try:
        encoded = path.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return (
        0 < len(encoded) <= MAX_LOGICAL_PATH_BYTES
        and not path.startswith("/")
        and "\\" not in path
        and not any(_is_control(character) for character in path)
        and all(segment not in ("", ".", "..") for segment in path.split("/"))
    )


def build_manifest(
    outputs_dir: Path, requirements: Sequence[JobOutputRequirement]
) -> tuple[ArtifactManifestFile, ...]:
    """Describe every declared output beneath ``outputs_dir``.

    The caller SHALL invoke this only after the job container has stopped, so nothing can write
    to the tree. Symbolic links are never followed. Anything that is not a singly linked regular
    file at a declared path, within its declared size, makes the whole tree untrustworthy.
    """
    declared = {requirement.logical_path: requirement for requirement in requirements}
    files: list[ArtifactManifestFile] = []
    total_bytes = 0
    for logical_path, path in _regular_files(outputs_dir):
        requirement = declared.get(logical_path)
        if requirement is None:
            raise OutputError(f"output {_shown(logical_path)} was not declared by the job")
        file = _describe(path, logical_path, requirement.max_bytes)
        total_bytes += file.byte_length
        if total_bytes > MAX_TOTAL_BYTES:
            raise OutputError("outputs exceed the total size limit")
        files.append(file)

    produced = {file.logical_path for file in files}
    missing = sorted(
        requirement.logical_path
        for requirement in requirements
        if requirement.mandatory and requirement.logical_path not in produced
    )
    if missing:
        raise OutputError(f"mandatory output {_shown(missing[0])} was not produced")
    return tuple(sorted(files, key=lambda file: file.logical_path))


def _regular_files(root: Path) -> Iterator[tuple[str, Path]]:
    inspected = 0
    pending = [(root, "")]
    while pending:
        directory, prefix = pending.pop()
        with os.scandir(directory) as entries:
            for entry in entries:
                inspected += 1
                if inspected > MAX_INSPECTED_ENTRIES:
                    raise OutputError("output tree is too large to inspect")
                logical_path = f"{prefix}{entry.name}"
                if not valid_logical_path(logical_path):
                    raise OutputError(f"output path {_shown(logical_path)} is not permitted")
                mode = entry.stat(follow_symlinks=False).st_mode
                if stat.S_ISDIR(mode):
                    pending.append((Path(entry.path), f"{logical_path}/"))
                elif stat.S_ISREG(mode):
                    yield logical_path, Path(entry.path)
                else:
                    raise OutputError(f"output {_shown(logical_path)} is not a regular file")


def _describe(path: Path, logical_path: str, max_bytes: int) -> ArtifactManifestFile:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as stream:
        status = os.fstat(stream.fileno())
        if not stat.S_ISREG(status.st_mode):
            raise OutputError(f"output {_shown(logical_path)} is not a regular file")
        if status.st_nlink > 1:
            raise OutputError(f"output {_shown(logical_path)} has more than one hard link")
        if status.st_size > min(max_bytes, MAX_FILE_BYTES):
            raise OutputError(f"output {_shown(logical_path)} exceeds its size limit")
        sha256 = hashlib.sha256()
        crc32c = google_crc32c.Checksum()
        byte_length = 0
        while chunk := stream.read(READ_CHUNK_BYTES):
            byte_length += len(chunk)
            if byte_length > status.st_size:
                break
            sha256.update(chunk)
            crc32c.update(chunk)
        if byte_length != status.st_size:
            raise OutputError(f"output {_shown(logical_path)} changed while it was being read")
    return ArtifactManifestFile(
        logical_path=logical_path,
        byte_length=byte_length,
        sha256=sha256.hexdigest(),
        crc32c=base64.b64encode(crc32c.digest()).decode("ascii"),
    )


def _is_control(character: str) -> bool:
    # Matches Rust's char::is_control, the rule the control plane applies.
    return character < " " or "\x7f" <= character <= "\x9f"


def _shown(logical_path: str) -> str:
    return repr(logical_path[:80])
