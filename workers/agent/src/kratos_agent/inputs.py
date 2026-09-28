"""Staging dataset inputs for a job attempt.

Three separate jobs, kept separate on purpose:

*Fetching* turns a short-lived signed URL into bytes on disk. *Caching* keys those bytes by their
verified SHA-256, so the same file is downloaded once however many attempts use it. *Materialising*
builds the tree the workload actually sees, at ``/kratos/inputs/<alias>``, out of links into the
cache.

The cache is what makes a retry cheap and a second job on the same dataset nearly free, so it
deliberately outlives an attempt. Nothing in it is trusted on the strength of having been there
before: a cache entry is only ever created after its digest has been verified, and its name *is*
that digest, so a file that fails verification can never be mistaken for one that passed.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import shutil
import stat
import tempfile
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

import httpx

from kratos_agent.models import DatasetInputFile, DatasetInputManifest

# Where the workload sees its inputs. One mount for every alias, each in its own subdirectory.
INPUT_MOUNT_TARGET = "/kratos/inputs"
# Inside the attempt directory, beside "outputs".
INPUT_DIRECTORY = "inputs"
# Beside "attempts" rather than inside one, because it is shared across attempts.
CACHE_DIRECTORY = "dataset-cache"
# The selection manifest sits beside the alias directory rather than inside it, so it can never
# collide with a path the dataset itself declares. An alias cannot contain a dot, so this name
# cannot collide with another alias either.
SELECTION_SUFFIX = ".selection.json"

# Read-only to the workload's user, and the agent owns the directories. The workload runs as an
# arbitrary user, so the tree has to be readable by it; nothing in it needs to be writable.
CACHE_FILE_MODE = 0o444
INPUT_DIRECTORY_MODE = 0o755
ATTEMPT_INPUT_ROOT_MODE = 0o755

DOWNLOAD_CHUNK_BYTES = 8 * 1024 * 1024
# A dataset file is fetched from object storage with a signed URL. Generous, because a LeRobot
# video is large, and bounded, because an attempt holds a lease.
DOWNLOAD_TIMEOUT_SECONDS = 600.0


class DatasetInputError(RuntimeError):
    """Staging failed in a way that should fail the attempt rather than be retried silently."""


@dataclass(frozen=True)
class StagedInput:
    """One alias, ready for the workload."""

    alias: str
    root: Path
    file_count: int
    byte_length: int
    cache_hits: int
    selection_path: Path


def cache_root(state_root: Path) -> Path:
    return state_root / CACHE_DIRECTORY


def attempt_input_root(attempt_root: Path) -> Path:
    return attempt_root / INPUT_DIRECTORY


def _cache_path(state_root: Path, digest: str) -> Path:
    """Shard by the first two hex characters, so one directory never holds every file."""
    return cache_root(state_root) / digest[:2] / digest


def _verified_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(DOWNLOAD_CHUNK_BYTES), b""):
            digest.update(chunk)
    return digest.hexdigest()


@contextmanager
def _temporary_file(directory: Path) -> Iterator[Path]:
    """A scratch file in the cache's own directory, so the move into place is atomic.

    Same filesystem, therefore ``os.replace`` cannot fail halfway and leave a partial file under a
    digest's name -- which is the one corruption this design must not allow, because a name in the
    cache is a claim that the bytes were verified.
    """
    directory.mkdir(parents=True, exist_ok=True)
    handle, raw = tempfile.mkstemp(dir=directory, prefix=".partial-")
    os.close(handle)
    scratch = Path(raw)
    try:
        yield scratch
    finally:
        scratch.unlink(missing_ok=True)


def _download(
    client: httpx.Client,
    dataset_file: DatasetInputFile,
    destination: Path,
    still_authorised: Callable[[], bool] | None,
) -> None:
    written = 0
    with client.stream("GET", dataset_file.download_url) as response:
        if response.status_code != httpx.codes.OK:
            # The URL is never included: it is a capability, and this message is logged.
            raise DatasetInputError(
                f"storage refused {dataset_file.path} with {response.status_code}"
            )
        with destination.open("wb") as handle:
            for chunk in response.iter_bytes(DOWNLOAD_CHUNK_BYTES):
                if still_authorised is not None and not still_authorised():
                    raise DatasetInputError("the attempt is no longer this agent's to run")
                written += len(chunk)
                if written > dataset_file.byte_length:
                    # Stop at the declared length rather than filling the disk with whatever the
                    # URL happens to serve.
                    raise DatasetInputError(
                        f"{dataset_file.path} is longer than its declared "
                        f"{dataset_file.byte_length} bytes"
                    )
                handle.write(chunk)
    if written != dataset_file.byte_length:
        raise DatasetInputError(
            f"{dataset_file.path} is {written} bytes, not the declared {dataset_file.byte_length}"
        )


def ensure_cached(
    state_root: Path,
    client: httpx.Client,
    dataset_file: DatasetInputFile,
    still_authorised: Callable[[], bool] | None = None,
) -> tuple[Path, bool]:
    """Return the cache path for a file's bytes, fetching them if they are not already there.

    The second element says whether the cache already had it, which is the only thing worth
    reporting about a download that did not happen.

    A cached entry is re-verified rather than trusted. It is cheap next to a download, and the
    alternative is trusting that nothing has touched the file since -- on a disk shared with every
    workload this agent has ever run.
    """
    target = _cache_path(state_root, dataset_file.sha256)
    if target.exists():
        if _verified_digest(target) == dataset_file.sha256:
            return target, True
        # Something changed it. Treat it as absent rather than as an error: the file is about to be
        # replaced by bytes that are verified before they take the name.
        target.unlink(missing_ok=True)

    with _temporary_file(target.parent) as scratch:
        _download(client, dataset_file, scratch, still_authorised)
        actual = _verified_digest(scratch)
        if actual != dataset_file.sha256:
            raise DatasetInputError(
                f"{dataset_file.path} hashed to {actual}, not the declared {dataset_file.sha256}"
            )
        scratch.chmod(CACHE_FILE_MODE)
        # Atomic, and last: until this line the digest names nothing.
        os.replace(scratch, target)
    return target, False


def _force_remove(path: Path) -> None:
    """Remove a file that may be read-only.

    Cache entries and their hard links are mode 0444 so a workload cannot alter them. On Linux the
    directory's write bit is what permits unlinking and the file's own mode is irrelevant, but on
    Windows a read-only file cannot be unlinked at all. Making the file writable first keeps
    restaging and cleanup working on both, rather than leaving the test suite platform-dependent.
    """
    with contextlib.suppress(OSError):
        path.chmod(stat.S_IWRITE | stat.S_IREAD)
    path.unlink(missing_ok=True)


def _remove_tree(root: Path) -> None:
    """Remove a staged tree whose files are deliberately read-only."""

    def clear(function: object, path: str, error: BaseException) -> None:
        _force_remove(Path(path))

    shutil.rmtree(root, onexc=clear)


def _link_into_place(cached: Path, destination: Path) -> None:
    """Hard link the cache entry into the attempt tree, copying only if that is impossible.

    A link means a hundred attempts on one dataset cost one copy of the bytes. The cache and the
    attempt tree are on the same volume, so this normally succeeds; the copy is there because a
    filesystem that refuses links should degrade rather than fail the job.
    """
    destination.parent.mkdir(parents=True, exist_ok=True)
    _force_remove(destination)
    try:
        os.link(cached, destination)
    except OSError:
        shutil.copyfile(cached, destination)
        destination.chmod(CACHE_FILE_MODE)


def stage_input(
    state_root: Path,
    attempt_root: Path,
    client: httpx.Client,
    manifest: DatasetInputManifest,
    still_authorised: Callable[[], bool] | None = None,
) -> StagedInput:
    """Materialise one alias under the attempt, fetching what the cache does not already hold."""
    alias_root = attempt_input_root(attempt_root) / manifest.alias
    # A previous attempt of this job may have left a partial tree. The cache is what is worth
    # keeping; this tree is rebuilt from it.
    if alias_root.exists():
        _remove_tree(alias_root)
    alias_root.mkdir(parents=True, exist_ok=True)

    cache_hits = 0
    byte_length = 0
    for dataset_file in manifest.files:
        destination = alias_root / dataset_file.path
        resolved = destination.resolve()
        # Belt and braces over the model's own check: whatever the manifest said, nothing is
        # written outside the alias directory.
        if not resolved.is_relative_to(alias_root.resolve()):
            raise DatasetInputError(f"{dataset_file.path} resolves outside its input directory")
        cached, hit = ensure_cached(state_root, client, dataset_file, still_authorised)
        _link_into_place(cached, destination)
        cache_hits += 1 if hit else 0
        byte_length += dataset_file.byte_length

    for directory in (alias_root, attempt_input_root(attempt_root)):
        directory.chmod(INPUT_DIRECTORY_MODE)
    for parent in sorted(
        {path.parent for path in alias_root.rglob("*") if path.is_file()},
        key=lambda path: len(path.parts),
    ):
        parent.chmod(INPUT_DIRECTORY_MODE)

    write_selection_manifest(attempt_root, manifest)

    return StagedInput(
        alias=manifest.alias,
        root=alias_root,
        file_count=len(manifest.files),
        byte_length=byte_length,
        cache_hits=cache_hits,
        selection_path=selection_manifest_path(attempt_root, manifest.alias),
    )


def selection_manifest_path(attempt_root: Path, alias: str) -> Path:
    return attempt_input_root(attempt_root) / f"{alias}{SELECTION_SUFFIX}"


def write_selection_manifest(attempt_root: Path, manifest: DatasetInputManifest) -> Path:
    """Record what this input *is*, and which episodes the job may train on.

    Without this the workload receives every file of the version and no way to tell which episodes
    a curated view selected. It would then train on the whole dataset while the run's lineage
    claimed a view -- a wrong result that looks like a correct one, which is the failure this whole
    seam exists to prevent.

    Immutable once written, and deliberately free of `download_url`: those are short-lived
    capabilities, and a file the workload can read is the last place to put one.
    """
    destination = selection_manifest_path(attempt_root, manifest.alias)
    document = {
        "schema_version": "1.0",
        "alias": manifest.alias,
        "dataset_id": str(manifest.dataset_id),
        "dataset_name": manifest.dataset_name,
        "dataset_version_id": str(manifest.dataset_version_id),
        "version_number": manifest.version_number,
        "source_kind": manifest.source_kind,
        "source_repository": manifest.source_repository,
        "resolved_revision": manifest.resolved_revision,
        "manifest_sha256": manifest.manifest_sha256,
        "dataset_view_id": (
            str(manifest.dataset_view_id) if manifest.dataset_view_id is not None else None
        ),
        "dataset_view_name": manifest.dataset_view_name,
        "dataset_view_manifest_sha256": manifest.dataset_view_manifest_sha256,
        # Ordered, because a workload iterating episodes should not have to guess whether the
        # order it was given means anything. A null view is the complete version, which is
        # recorded as the empty selection being absent rather than as an empty list.
        "included_episodes": list(manifest.included_episodes),
        "selects_every_episode": manifest.dataset_view_id is None,
        "files": [
            {
                "path": dataset_file.path,
                "media_type": dataset_file.media_type,
                "byte_length": dataset_file.byte_length,
                "sha256": dataset_file.sha256,
            }
            for dataset_file in manifest.files
        ],
    }
    destination.parent.mkdir(parents=True, exist_ok=True)
    _force_remove(destination)
    destination.write_text(json.dumps(document, indent=2, sort_keys=True), encoding="utf-8")
    destination.chmod(CACHE_FILE_MODE)
    return destination


def create_attempt_input_tree(attempt_root: Path) -> Path:
    """Create the inputs directory before the container is created.

    Docker requires a volume subpath to exist first, exactly as it does for outputs.
    """
    root = attempt_input_root(attempt_root)
    root.mkdir(parents=True, exist_ok=True)
    root.chmod(ATTEMPT_INPUT_ROOT_MODE)
    return root


def discard_attempt_inputs(attempt_root: Path) -> bool:
    """Remove an attempt's materialised inputs, leaving the shared cache alone.

    The tree is links and directories the agent owns, so unlike outputs it can always be removed;
    and removing it must never touch the cache, which is the point of staging through links.
    """
    root = attempt_input_root(attempt_root)
    if not root.exists():
        return True
    _remove_tree(root)
    return not root.exists()


def new_download_client() -> httpx.Client:
    """A client for signed storage reads, which carry their own authorisation in the URL.

    Deliberately separate from the control-plane client: this one must never send the worker
    credential, and it talks to object storage rather than to Kratos.
    """
    return httpx.Client(timeout=DOWNLOAD_TIMEOUT_SECONDS, follow_redirects=True)
