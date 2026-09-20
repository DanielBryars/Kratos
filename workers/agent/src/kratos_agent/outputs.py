"""Verified manifests for the output tree of a finished job attempt."""

import base64
import hashlib
import os
import stat
from collections.abc import Iterator, Sequence
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, cast

import google_crc32c

from kratos_agent.models import (
    MAX_OUTPUT_FILE_BYTES,
    MAX_OUTPUT_TOTAL_BYTES,
    ArtifactManifestFile,
    JobOutputRequirement,
    valid_logical_path,
)

MAX_INSPECTED_ENTRIES = 10_000
READ_CHUNK_BYTES = 1024 * 1024
SEALED_FILE_MODE = 0o400
SEALED_DIRECTORY_MODE = 0o500
DESCRIPTOR_WALK_SUPPORTED = os.open in os.supports_dir_fd and os.scandir in os.supports_fd


class OutputError(RuntimeError):
    """The output tree cannot be declared as trustworthy artefacts."""


@dataclass(frozen=True)
class OutputIdentity:
    """What must still be true of a file for its recorded checksums to describe it."""

    device: int
    inode: int
    byte_length: int
    link_count: int
    modified_ns: int
    changed_ns: int

    @classmethod
    def of(cls, status: os.stat_result) -> "OutputIdentity":
        return cls(
            device=status.st_dev,
            inode=status.st_ino,
            byte_length=status.st_size,
            link_count=status.st_nlink,
            modified_ns=status.st_mtime_ns,
            changed_ns=status.st_ctime_ns,
        )


@dataclass(frozen=True)
class VerifiedOutput:
    """A manifest entry and the identity an uploader SHALL re-check on the descriptor it sends."""

    file: ArtifactManifestFile
    identity: OutputIdentity


class _Checksum(Protocol):
    def update(self, chunk: bytes) -> None: ...
    def digest(self) -> bytes: ...


def _new_crc32c() -> _Checksum:
    # google-crc32c ships no type information; this is the only untyped call.
    return cast(_Checksum, google_crc32c.Checksum())  # type: ignore[no-untyped-call]


def build_manifest(
    outputs_dir: Path, requirements: Sequence[JobOutputRequirement]
) -> tuple[VerifiedOutput, ...]:
    """Describe every declared output beneath ``outputs_dir``.

    The caller SHALL invoke this only after the job container has stopped. The tree is walked
    through directory descriptors opened without following symbolic links, so replacing an
    inspected directory or file cannot redirect the walk outside the tree. Each visited directory
    and file is made read-only, and a file is rejected if anything about it changes while it is
    read. Anything other than a singly linked regular file at a declared path, within its declared
    size, makes the whole tree untrustworthy.
    """
    if not DESCRIPTOR_WALK_SUPPORTED:
        raise OutputError("output collection requires descriptor-relative file access")
    declared = {requirement.logical_path: requirement for requirement in requirements}
    outputs: list[VerifiedOutput] = []
    total_bytes = 0
    with _open_directory(str(outputs_dir), None, "the output directory") as root:
        for logical_path, name, directory in _regular_files(root):
            requirement = declared.get(logical_path)
            if requirement is None:
                raise OutputError(f"output {_shown(logical_path)} was not declared by the job")
            output = _describe(name, directory, logical_path, requirement.max_bytes)
            total_bytes += output.file.byte_length
            if total_bytes > MAX_OUTPUT_TOTAL_BYTES:
                raise OutputError("outputs exceed the total size limit")
            outputs.append(output)

    produced = {output.file.logical_path for output in outputs}
    missing = sorted(
        requirement.logical_path
        for requirement in requirements
        if requirement.mandatory and requirement.logical_path not in produced
    )
    if missing:
        raise OutputError(f"mandatory output {_shown(missing[0])} was not produced")
    return tuple(sorted(outputs, key=lambda output: output.file.logical_path))


@contextmanager
def _open_directory(name: str, parent: int | None, shown: str) -> Iterator[int]:
    try:
        descriptor = os.open(
            name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent
        )
    except OSError as error:
        raise OutputError(f"{shown} could not be opened as a directory") from error
    try:
        os.fchmod(descriptor, SEALED_DIRECTORY_MODE)
        yield descriptor
    finally:
        os.close(descriptor)


def _regular_files(root: int) -> Iterator[tuple[str, str, int]]:
    """Yield each regular file's logical path, its name and its still-open parent directory."""
    inspected = 0

    def walk(directory: int, prefix: str) -> Iterator[tuple[str, str, int]]:
        nonlocal inspected
        children: list[tuple[str, int]] = []
        with os.scandir(directory) as entries:
            for entry in entries:
                inspected += 1
                if inspected > MAX_INSPECTED_ENTRIES:
                    raise OutputError("output tree is too large to inspect")
                children.append((entry.name, entry.stat(follow_symlinks=False).st_mode))
        for name, mode in sorted(children):
            logical_path = f"{prefix}{name}"
            if not valid_logical_path(logical_path):
                raise OutputError(f"output path {_shown(logical_path)} is not permitted")
            if stat.S_ISDIR(mode):
                shown = f"output directory {_shown(logical_path)}"
                with _open_directory(name, directory, shown) as child:
                    yield from walk(child, f"{logical_path}/")
            elif stat.S_ISREG(mode):
                yield logical_path, name, directory
            else:
                raise OutputError(f"output {_shown(logical_path)} is not a regular file")

    yield from walk(root, "")


def _describe(name: str, directory: int, logical_path: str, max_bytes: int) -> VerifiedOutput:
    try:
        # O_NONBLOCK keeps a file that was swapped for a named pipe from blocking the open.
        descriptor = os.open(
            name,
            os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
            dir_fd=directory,
        )
    except OSError as error:
        raise OutputError(f"output {_shown(logical_path)} could not be opened") from error
    with os.fdopen(descriptor, "rb") as stream:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise OutputError(f"output {_shown(logical_path)} is not a regular file")
        os.fchmod(descriptor, SEALED_FILE_MODE)
        identity = OutputIdentity.of(os.fstat(descriptor))
        if identity.link_count > 1:
            raise OutputError(f"output {_shown(logical_path)} has more than one hard link")
        if identity.byte_length > min(max_bytes, MAX_OUTPUT_FILE_BYTES):
            raise OutputError(f"output {_shown(logical_path)} exceeds its size limit")
        sha256 = hashlib.sha256()
        crc32c = _new_crc32c()
        byte_length = 0
        while byte_length <= identity.byte_length and (chunk := stream.read(READ_CHUNK_BYTES)):
            byte_length += len(chunk)
            sha256.update(chunk)
            crc32c.update(chunk)
        unchanged = OutputIdentity.of(os.fstat(descriptor)) == identity
        if byte_length != identity.byte_length or not unchanged:
            raise OutputError(f"output {_shown(logical_path)} changed while it was being read")
    return VerifiedOutput(
        file=ArtifactManifestFile(
            logical_path=logical_path,
            byte_length=byte_length,
            sha256=sha256.hexdigest(),
            crc32c=base64.b64encode(crc32c.digest()).decode("ascii"),
        ),
        identity=identity,
    )


def _shown(logical_path: str) -> str:
    return repr(logical_path[:80])
