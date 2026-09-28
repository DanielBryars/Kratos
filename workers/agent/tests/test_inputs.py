"""Staging dataset inputs: the cache, the digest checks, and the tree the workload sees."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
from uuid import UUID

import httpx
import pytest
from pydantic import ValidationError

from kratos_agent.inputs import (
    CACHE_DIRECTORY,
    INPUT_DIRECTORY,
    DatasetInputError,
    attempt_input_root,
    create_attempt_input_tree,
    discard_attempt_inputs,
    ensure_cached,
    stage_input,
)
from kratos_agent.models import DatasetInputFile, DatasetInputManifest


def digest_of(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def dataset_file(path: str, payload: bytes, *, sha256: str | None = None) -> DatasetInputFile:
    return DatasetInputFile(
        path=path,
        media_type="application/octet-stream",
        byte_length=len(payload),
        sha256=sha256 or digest_of(payload),
        download_url=f"https://storage.example.test/{path}",
    )


def manifest(files: tuple[DatasetInputFile, ...], alias: str = "training") -> DatasetInputManifest:
    return DatasetInputManifest(
        alias=alias,
        dataset_id=UUID("11111111-1111-4111-8111-111111111111"),
        dataset_name="SVLA pick and place",
        dataset_version_id=UUID("22222222-2222-4222-8222-222222222222"),
        version_number=1,
        source_kind="upload",
        manifest_sha256="a" * 64,
        included_episodes=(0, 2),
        files=files,
    )


def serving(payloads: dict[str, bytes], counter: list[str] | None = None) -> httpx.Client:
    """A client that serves declared payloads and records every URL it was asked for."""

    def handler(request: httpx.Request) -> httpx.Response:
        name = request.url.path.lstrip("/")
        if counter is not None:
            counter.append(name)
        if name not in payloads:
            return httpx.Response(404)
        return httpx.Response(200, content=payloads[name])

    return httpx.Client(transport=httpx.MockTransport(handler))


def test_a_file_is_fetched_once_and_served_from_the_cache_after(tmp_path: Path) -> None:
    payload = b"parquet bytes" * 100
    requested: list[str] = []
    client = serving({"data/episode_0.parquet": payload}, requested)
    entry = dataset_file("data/episode_0.parquet", payload)

    first, hit = ensure_cached(tmp_path, client, entry)
    assert hit is False
    assert first.read_bytes() == payload

    second, hit = ensure_cached(tmp_path, client, entry)
    assert hit is True, "the second call must not download"
    assert second == first
    assert requested == ["data/episode_0.parquet"], requested


def test_the_cache_is_keyed_by_digest_not_by_path(tmp_path: Path) -> None:
    # The same bytes under two names cost one download, which is the point of keying by digest.
    payload = b"identical"
    requested: list[str] = []
    client = serving({"a/one": payload, "b/two": payload}, requested)

    ensure_cached(tmp_path, client, dataset_file("a/one", payload))
    _, hit = ensure_cached(tmp_path, client, dataset_file("b/two", payload))
    assert hit is True
    assert requested == ["a/one"], requested


def test_a_digest_mismatch_fails_and_leaves_nothing_behind(tmp_path: Path) -> None:
    payload = b"the bytes that actually arrive"
    client = serving({"data/tampered": payload})
    lying = dataset_file("data/tampered", payload, sha256="b" * 64)

    with pytest.raises(DatasetInputError, match="hashed to"):
        ensure_cached(tmp_path, client, lying)

    # Nothing may be left under the claimed digest: a name in the cache is a claim the bytes were
    # verified, so a failed verification must not create one.
    assert not (tmp_path / CACHE_DIRECTORY / "bb" / ("b" * 64)).exists()
    leftovers = [path for path in (tmp_path / CACHE_DIRECTORY).rglob("*") if path.is_file()]
    assert leftovers == [], leftovers


def test_a_short_or_long_file_is_refused(tmp_path: Path) -> None:
    payload = b"twelve bytes"
    client = serving({"data/short": payload})
    # Declared longer than it is.
    entry = DatasetInputFile(
        path="data/short",
        media_type="application/octet-stream",
        byte_length=len(payload) + 5,
        sha256=digest_of(payload),
        download_url="https://storage.example.test/data/short",
    )
    with pytest.raises(DatasetInputError, match="bytes"):
        ensure_cached(tmp_path, client, entry)

    # Declared shorter than it is: the download stops rather than filling the disk.
    entry = DatasetInputFile(
        path="data/short",
        media_type="application/octet-stream",
        byte_length=4,
        sha256=digest_of(payload[:4]),
        download_url="https://storage.example.test/data/short",
    )
    with pytest.raises(DatasetInputError, match="longer than its declared"):
        ensure_cached(tmp_path, client, entry)


def test_a_corrupted_cache_entry_is_replaced_rather_than_trusted(tmp_path: Path) -> None:
    payload = b"good bytes"
    client = serving({"data/file": payload})
    entry = dataset_file("data/file", payload)
    cached, _ = ensure_cached(tmp_path, client, entry)

    cached.chmod(0o644)
    cached.write_bytes(b"something else entirely")

    replaced, hit = ensure_cached(tmp_path, client, entry)
    assert hit is False, "a cache entry that no longer matches its name must be refetched"
    assert replaced.read_bytes() == payload


def test_storage_refusing_a_read_fails_without_disclosing_the_url(tmp_path: Path) -> None:
    client = serving({})
    with pytest.raises(DatasetInputError) as failure:
        ensure_cached(tmp_path, client, dataset_file("data/missing", b"x"))
    message = str(failure.value)
    assert "404" in message
    # The signed URL is a capability. It must not reach a log through an error message.
    assert "storage.example.test" not in message, message


def test_an_attempt_that_loses_its_lease_stops_downloading(tmp_path: Path) -> None:
    payload = b"z" * (9 * 1024 * 1024)
    client = serving({"data/big": payload})
    with pytest.raises(DatasetInputError, match="no longer this agent"):
        ensure_cached(tmp_path, client, dataset_file("data/big", payload), lambda: False)


def test_staging_builds_the_tree_the_workload_sees(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    attempt_root = tmp_path / "state" / "attempts" / "attempt-1"
    info = b'{"codebase_version":"v2.1"}'
    episode = b"parquet" * 50
    client = serving({"meta/info.json": info, "data/episode_0.parquet": episode})

    staged = stage_input(
        state_root,
        attempt_root,
        client,
        manifest(
            (dataset_file("meta/info.json", info), dataset_file("data/episode_0.parquet", episode))
        ),
    )

    assert staged.alias == "training"
    assert staged.file_count == 2
    assert staged.byte_length == len(info) + len(episode)
    assert staged.cache_hits == 0
    root = attempt_input_root(attempt_root) / "training"
    assert (root / "meta/info.json").read_bytes() == info
    assert (root / "data/episode_0.parquet").read_bytes() == episode


def test_a_second_attempt_costs_no_downloads(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    info = b'{"codebase_version":"v2.1"}'
    requested: list[str] = []
    client = serving({"meta/info.json": info}, requested)
    entries = manifest((dataset_file("meta/info.json", info),))

    stage_input(state_root, state_root / "attempts" / "one", client, entries)
    staged = stage_input(state_root, state_root / "attempts" / "two", client, entries)

    assert staged.cache_hits == 1
    assert requested == ["meta/info.json"], requested
    # Both attempts see the file, and the cache holds one copy of the bytes.
    for attempt in ("one", "two"):
        path = state_root / "attempts" / attempt / INPUT_DIRECTORY / "training" / "meta/info.json"
        assert path.read_bytes() == info


def test_materialising_links_rather_than_copying(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    payload = b"linked bytes"
    client = serving({"meta/info.json": payload})
    stage_input(
        state_root,
        state_root / "attempts" / "one",
        client,
        manifest((dataset_file("meta/info.json", payload),)),
    )
    staged_file = state_root / "attempts" / "one" / INPUT_DIRECTORY / "training" / "meta/info.json"
    cached = next((state_root / CACHE_DIRECTORY).rglob("*"))
    while cached.is_dir():
        cached = next(cached.rglob("*"))
    # One inode, two names: a hundred attempts on one dataset cost one copy of the bytes.
    assert os.stat(staged_file).st_ino == os.stat(cached).st_ino


def test_restaging_replaces_a_partial_tree_and_keeps_the_cache(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    attempt_root = state_root / "attempts" / "one"
    payload = b"bytes"
    client = serving({"meta/info.json": payload})
    entries = manifest((dataset_file("meta/info.json", payload),))
    stage_input(state_root, attempt_root, client, entries)

    stray = attempt_input_root(attempt_root) / "training" / "left-behind"
    stray.write_bytes(b"from a previous attempt")

    staged = stage_input(state_root, attempt_root, client, entries)
    assert not stray.exists(), "a rebuilt tree must not keep what a previous attempt left"
    assert staged.cache_hits == 1, "and it must not refetch what the cache already has"


def test_discarding_inputs_leaves_the_cache_intact(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    attempt_root = state_root / "attempts" / "one"
    payload = b"bytes"
    client = serving({"meta/info.json": payload})
    stage_input(
        state_root, attempt_root, client, manifest((dataset_file("meta/info.json", payload),))
    )

    assert discard_attempt_inputs(attempt_root) is True
    assert not attempt_input_root(attempt_root).exists()
    cached = [path for path in (state_root / CACHE_DIRECTORY).rglob("*") if path.is_file()]
    assert len(cached) == 1, "the cache is what makes the next attempt cheap; it must survive"
    assert discard_attempt_inputs(attempt_root) is True, "discarding twice is not an error"


def test_the_input_tree_exists_before_the_container_would_be_created(tmp_path: Path) -> None:
    # Docker requires a volume subpath to exist before it creates the container, exactly as for
    # outputs. Without this a job with inputs would fail at container creation.
    attempt_root = tmp_path / "attempts" / "one"
    root = create_attempt_input_tree(attempt_root)
    assert root.is_dir()
    assert root == attempt_input_root(attempt_root)


@pytest.mark.parametrize(
    "path",
    [
        "/etc/passwd",
        "../escape",
        "meta/../../escape",
        "meta//info.json",
        "meta/./info.json",
        "a\\b",
    ],
)
def test_a_traversing_path_is_refused_by_the_model(path: str) -> None:
    # Refused before anything reaches the filesystem. The control plane checks this too; this
    # agent is what turns the string into a write, so it checks again.
    with pytest.raises(ValidationError):
        DatasetInputFile(
            path=path,
            media_type="application/octet-stream",
            byte_length=1,
            sha256="a" * 64,
            download_url="https://storage.example.test/x",
        )


def test_a_manifest_cannot_repeat_a_path() -> None:
    payload = b"x"
    with pytest.raises(ValidationError, match="repeats a path"):
        manifest((dataset_file("meta/info.json", payload), dataset_file("meta/info.json", payload)))


def test_the_workload_receives_the_exact_ordered_episode_selection(tmp_path: Path) -> None:
    # Without this the workload gets every file of the version and no way to know which episodes a
    # curated view chose. It would train on the whole dataset while the run's lineage claimed a
    # view: a wrong result that looks like a correct one.
    state_root = tmp_path / "state"
    attempt_root = state_root / "attempts" / "one"
    payload = b'{"codebase_version":"v2.1"}'
    client = serving({"meta/info.json": payload})
    curated = manifest((dataset_file("meta/info.json", payload),)).model_copy(
        update={
            "dataset_view_id": UUID("55555555-5555-4555-8555-555555555555"),
            "dataset_view_name": "First cut",
            "dataset_view_manifest_sha256": "d" * 64,
            "included_episodes": (7, 2, 11),
        }
    )

    staged = stage_input(state_root, attempt_root, client, curated)

    document = json.loads(staged.selection_path.read_text(encoding="utf-8"))
    # Order is preserved exactly: a workload iterating episodes should not have to guess whether
    # the order it was handed means anything.
    assert document["included_episodes"] == [7, 2, 11]
    assert document["dataset_view_id"] == "55555555-5555-4555-8555-555555555555"
    assert document["dataset_view_name"] == "First cut"
    assert document["selects_every_episode"] is False


def test_a_complete_version_says_so_rather_than_listing_nothing(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    payload = b"x"
    client = serving({"meta/info.json": payload})
    whole = manifest((dataset_file("meta/info.json", payload),)).model_copy(
        update={"included_episodes": ()}
    )

    staged = stage_input(state_root, state_root / "attempts" / "one", client, whole)

    document = json.loads(staged.selection_path.read_text(encoding="utf-8"))
    # An empty list and "everything" are not the same instruction, so the manifest says which.
    assert document["selects_every_episode"] is True
    assert document["dataset_view_id"] is None
    assert document["included_episodes"] == []


def test_the_selection_manifest_sits_beside_the_alias_and_cannot_collide(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    attempt_root = state_root / "attempts" / "one"
    payload = b"x"
    # A dataset is free to declare a path called "selection.json"; the manifest must not be it.
    client = serving({"meta/info.json": payload, "selection.json": payload})
    entries = manifest(
        (dataset_file("meta/info.json", payload), dataset_file("selection.json", payload))
    )

    staged = stage_input(state_root, attempt_root, client, entries)

    assert staged.selection_path.parent == attempt_input_root(attempt_root)
    assert staged.selection_path.name == "training.selection.json"
    dataset_copy = attempt_input_root(attempt_root) / "training" / "selection.json"
    assert dataset_copy.read_bytes() == payload
    assert staged.selection_path != dataset_copy


def test_the_selection_manifest_never_carries_a_download_url(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    payload = b"x"
    client = serving({"meta/info.json": payload})

    staged = stage_input(
        state_root,
        state_root / "attempts" / "one",
        client,
        manifest((dataset_file("meta/info.json", payload),)),
    )

    text = staged.selection_path.read_text(encoding="utf-8")
    # A signed URL is a short-lived capability, and a file the workload can read is the last place
    # to leave one.
    assert "storage.example.test" not in text
    assert "download_url" not in text
    document = json.loads(text)
    assert document["files"] == [
        {
            "path": "meta/info.json",
            "media_type": "application/octet-stream",
            "byte_length": len(payload),
            "sha256": digest_of(payload),
        }
    ]


def test_the_selection_manifest_is_not_writable_by_the_workload(tmp_path: Path) -> None:
    state_root = tmp_path / "state"
    payload = b"x"
    client = serving({"meta/info.json": payload})
    staged = stage_input(
        state_root,
        state_root / "attempts" / "one",
        client,
        manifest((dataset_file("meta/info.json", payload),)),
    )
    assert not staged.selection_path.stat().st_mode & 0o222


def test_restaging_replaces_the_selection_manifest(tmp_path: Path) -> None:
    # The manifest is read-only, so rewriting it has to undo that first; otherwise a retry would
    # leave the previous attempt's selection in place.
    state_root = tmp_path / "state"
    attempt_root = state_root / "attempts" / "one"
    payload = b"x"
    client = serving({"meta/info.json": payload})
    entries = manifest((dataset_file("meta/info.json", payload),))
    stage_input(state_root, attempt_root, client, entries)

    curated = entries.model_copy(update={"included_episodes": (4,)})
    staged = stage_input(state_root, attempt_root, client, curated)

    document = json.loads(staged.selection_path.read_text(encoding="utf-8"))
    assert document["included_episodes"] == [4]
