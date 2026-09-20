"""Verified manifests for the output tree of a finished job attempt."""

import base64
import contextlib
import hashlib
import os
import shutil
import stat
from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Protocol, cast

import google_crc32c

from kratos_agent.models import (
    MAX_OUTPUT_FILE_BYTES,
    MAX_OUTPUT_TOTAL_BYTES,
    ArtifactManifestFile,
    JobOutputRequirement,
    valid_logical_path,
)

# Output collection depends on descriptor-relative access that Windows does not provide. The agent
# always runs in the Linux execution environment (ADR-007), so these are resolved once here rather
# than typed for a platform where the collector refuses to run.
_O_DIRECTORY: int = getattr(os, "O_DIRECTORY", 0)
_O_NOFOLLOW: int = getattr(os, "O_NOFOLLOW", 0)
_O_NONBLOCK: int = getattr(os, "O_NONBLOCK", 0)
_O_CLOEXEC: int = getattr(os, "O_CLOEXEC", 0)
_fchmod: Callable[[int, int], None] | None = getattr(os, "fchmod", None)

MAX_INSPECTED_ENTRIES = 10_000
READ_CHUNK_BYTES = 1024 * 1024
SEALED_FILE_MODE = 0o400
SEALED_DIRECTORY_MODE = 0o500
UNSEALED_FILE_MODE = 0o600
UNSEALED_DIRECTORY_MODE = 0o700
# A job image runs as whatever user it declares, so the leaf it writes into must be
# writable by that unknown user. Only the leaf is opened up; the attempt directory
# above it stays private to the agent, so nothing can reach a sibling attempt.
OUTPUT_LEAF_MODE = 0o777
ATTEMPT_DIRECTORY_MODE = 0o700
DESCRIPTOR_WALK_SUPPORTED = (
    os.open in os.supports_dir_fd
    and os.scandir in os.supports_fd
    and bool(_O_DIRECTORY and _O_NOFOLLOW)
)


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

    The caller SHALL invoke this only after the job container has stopped, which is what makes
    the tree quiescent; the agent may not own what a job image wrote. The tree is walked
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
            name, os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC, dir_fd=parent
        )
    except OSError as error:
        raise OutputError(f"{shown} could not be opened as a directory") from error
    try:
        _seal(descriptor, SEALED_DIRECTORY_MODE)
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
            os.O_RDONLY | _O_NOFOLLOW | _O_NONBLOCK | _O_CLOEXEC,
            dir_fd=directory,
        )
    except OSError as error:
        raise OutputError(f"output {_shown(logical_path)} could not be opened") from error
    with os.fdopen(descriptor, "rb") as stream:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise OutputError(f"output {_shown(logical_path)} is not a regular file")
        _seal(descriptor, SEALED_FILE_MODE)
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


@contextmanager
def open_verified(source: Path, identity: OutputIdentity) -> Iterator[BinaryIO]:
    """Open an output for transfer, refusing it unless it is still the file that was hashed.

    This closes the window between hashing and uploading: a file replaced, relinked or rewritten
    in between would otherwise be sent under the manifest's checksums.
    """
    try:
        descriptor = os.open(str(source), os.O_RDONLY | _O_NOFOLLOW | _O_NONBLOCK | _O_CLOEXEC)
    except OSError as error:
        raise OutputError(f"output {_shown(source.name)} could not be reopened") from error
    stream = os.fdopen(descriptor, "rb")
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise OutputError(f"output {_shown(source.name)} is not a regular file")
        if OutputIdentity.of(os.fstat(descriptor)) != identity:
            raise OutputError(f"output {_shown(source.name)} changed after it was recorded")
        yield stream
    finally:
        stream.close()


def create_attempt_tree(attempt_root: Path) -> Path:
    """Create the outputs directory a job will write into, and return it.

    Docker requires a volume subpath to exist before the container is created. The leaf is made
    world-writable because the workload's user is not known to the agent; its parent is not, so an
    attempt cannot see or reach another attempt's outputs.
    """
    outputs = attempt_root / "outputs"
    outputs.mkdir(parents=True, exist_ok=True)
    attempt_root.chmod(ATTEMPT_DIRECTORY_MODE)
    outputs.chmod(OUTPUT_LEAF_MODE)
    return outputs


def discard_tree(root: Path) -> None:
    """Remove an attempt's retained outputs, undoing the read-only sealing first.

    Collection seals the tree, so a non-root agent cannot delete what it just sealed until the
    write bit is restored. Anything it does not own is left behind rather than forced.
    """
    if not root.exists():
        return
    for directory, _, files in os.walk(root, topdown=False):
        for name in (*files, ""):
            target = Path(directory) / name if name else Path(directory)
            with contextlib.suppress(OSError):
                target.chmod(UNSEALED_DIRECTORY_MODE if not name else UNSEALED_FILE_MODE)
    shutil.rmtree(root, ignore_errors=True)


def _seal(descriptor: int, mode: int) -> None:
    """Make an output read-only while it is inspected, where this agent is allowed to.

    A job image may write its outputs as any user, so the agent often does not own them and cannot
    change their mode. Sealing is therefore defence in depth, not the guarantee: correctness rests
    on the container being stopped before collection and on each file's recorded identity being
    re-checked after it has been read.
    """
    if _fchmod is None:
        return
    try:
        _fchmod(descriptor, mode)
    except OSError:
        return


def _shown(logical_path: str) -> str:
    return repr(logical_path[:80])
