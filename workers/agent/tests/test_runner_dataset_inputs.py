"""The runner's staging step: what it fetches, what it refuses, and what it leaves behind."""

from __future__ import annotations

import hashlib
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, cast
from uuid import UUID, uuid4

import httpx
import pytest

from kratos_agent.inputs import attempt_input_root, selection_manifest_path
from kratos_agent.models import (
    DatasetInputAssignment,
    DatasetInputFile,
    DatasetInputManifest,
    JobAssignment,
)
from kratos_agent.protocol import ControlPlaneError

WORKER_ID = UUID("44444444-4444-4444-8444-444444444444")
VERSION_ID = UUID("22222222-2222-4222-8222-222222222222")
PAYLOAD = b'{"codebase_version":"v2.1"}'
DIGEST = hashlib.sha256(PAYLOAD).hexdigest()
MANIFEST_SHA = "a" * 64


def assignment(**overrides: object) -> JobAssignment:
    descriptor = DatasetInputAssignment(
        alias="training",
        dataset_version_id=VERSION_ID,
        manifest_sha256=MANIFEST_SHA,
    )
    base = {
        "attempt_id": uuid4(),
        "job_id": uuid4(),
        "name": "Training run",
        "image_reference": "example.test/work@sha256:" + ("b" * 64),
        "gpu_index": 0,
        "timeout_seconds": 120,
        "lease_expires_at": "2026-09-28T12:00:00Z",
        "dataset_inputs": (descriptor,),
    }
    base.update(overrides)
    return JobAssignment(**base)  # type: ignore[arg-type]


def served_manifest(**overrides: object) -> DatasetInputManifest:
    base = {
        "alias": "training",
        "dataset_id": UUID("11111111-1111-4111-8111-111111111111"),
        "dataset_name": "SVLA",
        "dataset_version_id": VERSION_ID,
        "version_number": 1,
        "source_kind": "upload",
        "manifest_sha256": MANIFEST_SHA,
        "included_episodes": (0,),
        "files": (
            DatasetInputFile(
                path="meta/info.json",
                media_type="application/json",
                byte_length=len(PAYLOAD),
                sha256=DIGEST,
                download_url="https://storage.example.test/meta/info.json",
            ),
        ),
    }
    base.update(overrides)
    return DatasetInputManifest(**base)  # type: ignore[arg-type]


class FakeControlPlane:
    """Serves one manifest, or raises what the real client would raise."""

    def __init__(self, manifest: DatasetInputManifest | Exception) -> None:
        self._manifest = manifest
        self.requested: list[str] = []

    def fetch_dataset_input_manifest(
        self, worker_id: UUID, credential: str, attempt_id: UUID, alias: str
    ) -> DatasetInputManifest:
        self.requested.append(alias)
        if isinstance(self._manifest, Exception):
            raise self._manifest
        return self._manifest


def runner_with(
    tmp_path: Path, control_plane: FakeControlPlane, monkeypatch: pytest.MonkeyPatch
) -> Any:
    from kratos_agent import runner as runner_module

    monkeypatch.setattr(
        runner_module,
        "new_download_client",
        lambda: httpx.Client(
            transport=httpx.MockTransport(lambda request: httpx.Response(200, content=PAYLOAD))
        ),
    )
    instance = object.__new__(runner_module.AgentRunner)
    instance._state_path = tmp_path / "state.json"
    instance._client = cast(Any, control_plane)
    return instance


def test_staging_materialises_the_input_and_reports_success(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    control_plane = FakeControlPlane(served_manifest())
    runner = runner_with(tmp_path, control_plane, monkeypatch)
    job = assignment()

    result = runner._stage_dataset_inputs(job, WORKER_ID, "credential", lambda: True)

    assert result is None, "staging that succeeded must not produce a failure result"
    attempt_root = tmp_path / "attempts" / str(job.attempt_id)
    staged = attempt_input_root(attempt_root) / "training" / "meta/info.json"
    assert staged.read_bytes() == PAYLOAD
    assert control_plane.requested == ["training"]


def test_a_manifest_that_is_not_the_selected_one_fails_the_attempt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The assignment pins a manifest digest. If the server serves a different one the job would
    # train on something other than the version it selected, which is what lineage exists to stop.
    control_plane = FakeControlPlane(served_manifest(manifest_sha256="f" * 64))
    runner = runner_with(tmp_path, control_plane, monkeypatch)
    job = assignment()

    result = runner._stage_dataset_inputs(job, WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert result.exit_code == 125
    assert "different manifest" in (result.failure_message or "")
    # And nothing is left mounted-looking for the workload to pick up.
    attempt_root = tmp_path / "attempts" / str(job.attempt_id)
    assert not (attempt_input_root(attempt_root) / "training").exists()


def test_a_manifest_for_another_version_fails_the_attempt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    control_plane = FakeControlPlane(served_manifest(dataset_version_id=uuid4()))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(assignment(), WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert "different dataset version" in (result.failure_message or "")


def test_a_control_plane_failure_fails_the_attempt_rather_than_raising(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A workload started without its inputs would train on nothing and look like it worked, so
    # staging failure has to end the attempt with a message rather than escape as an exception.
    control_plane = FakeControlPlane(ControlPlaneError(503, "unavailable", "no manifest today"))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(assignment(), WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert result.exit_code == 125
    assert "dataset inputs could not be staged" in (result.failure_message or "")


def test_a_failure_message_never_carries_a_download_url(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    control_plane = FakeControlPlane(served_manifest(manifest_sha256="f" * 64))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(assignment(), WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert "storage.example.test" not in (result.failure_message or "")


def descriptor_with(**overrides: object) -> DatasetInputAssignment:
    base = {
        "alias": "training",
        "dataset_version_id": VERSION_ID,
        "manifest_sha256": MANIFEST_SHA,
    }
    base.update(overrides)
    return DatasetInputAssignment(**base)  # type: ignore[arg-type]


VIEW_ID = UUID("55555555-5555-4555-8555-555555555555")


def test_a_full_version_answer_to_a_curated_request_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The dangerous case Codex named: right version, right digest, no view. The episodes would be
    # the whole dataset while the run's lineage claimed a curated view.
    job = assignment(dataset_inputs=(descriptor_with(dataset_view_id=VIEW_ID),))
    control_plane = FakeControlPlane(served_manifest(dataset_view_id=None))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(job, WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert "different curated view" in (result.failure_message or "")


def test_a_different_view_of_the_right_version_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    job = assignment(dataset_inputs=(descriptor_with(dataset_view_id=VIEW_ID),))
    control_plane = FakeControlPlane(served_manifest(dataset_view_id=uuid4()))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(job, WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert "different curated view" in (result.failure_message or "")


def test_a_curated_view_answer_to_a_full_version_request_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # And the other direction: a null view means the complete version, so a view is wrong too.
    control_plane = FakeControlPlane(served_manifest(dataset_view_id=VIEW_ID))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(assignment(), WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert "different curated view" in (result.failure_message or "")


def test_the_matching_view_is_accepted_and_recorded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    job = assignment(dataset_inputs=(descriptor_with(dataset_view_id=VIEW_ID),))
    control_plane = FakeControlPlane(
        served_manifest(dataset_view_id=VIEW_ID, included_episodes=(3, 1))
    )
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    assert runner._stage_dataset_inputs(job, WORKER_ID, "credential", lambda: True) is None

    attempt_root = tmp_path / "attempts" / str(job.attempt_id)
    document = json.loads(
        selection_manifest_path(attempt_root, "training").read_text(encoding="utf-8")
    )
    assert document["dataset_view_id"] == str(VIEW_ID)
    assert document["included_episodes"] == [3, 1]


def test_a_manifest_for_another_alias_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Otherwise another input's contents would be staged under this input's name.
    control_plane = FakeControlPlane(served_manifest(alias="evaluation"))
    runner = runner_with(tmp_path, control_plane, monkeypatch)

    result = runner._stage_dataset_inputs(assignment(), WORKER_ID, "credential", lambda: True)

    assert result is not None
    assert "manifest for 'evaluation'" in (result.failure_message or "")


def test_a_withdrawn_assignment_stops_a_multi_chunk_download_before_the_workload_starts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The case the previous revision could not detect, because it was handed `lambda: True`.

    Authority is withdrawn part-way through a download that spans several chunks. Staging must
    stop, leave no staged tree, and return a failure so the workload is never started with inputs
    that were only partly fetched.
    """
    from kratos_agent import runner as runner_module

    big = b"y" * (20 * 1024 * 1024)
    digest = hashlib.sha256(big).hexdigest()
    control_plane = FakeControlPlane(
        served_manifest(
            files=(
                DatasetInputFile(
                    path="data/episode_0.parquet",
                    media_type="application/vnd.apache.parquet",
                    byte_length=len(big),
                    sha256=digest,
                    download_url="https://storage.example.test/data/episode_0.parquet",
                ),
            )
        )
    )
    runner = runner_with(tmp_path, control_plane, monkeypatch)
    # After runner_with, which installs its own small-payload client: this file has to span
    # several chunks for a mid-download withdrawal to be observable at all.
    monkeypatch.setattr(
        runner_module,
        "new_download_client",
        lambda: httpx.Client(
            transport=httpx.MockTransport(lambda request: httpx.Response(200, content=big))
        ),
    )
    job = assignment()

    # Authorised for the first chunk, withdrawn from then on.
    calls = {"n": 0}

    def withdrawn_after_first_chunk() -> bool:
        calls["n"] += 1
        return calls["n"] <= 1

    result = runner._stage_dataset_inputs(job, WORKER_ID, "credential", withdrawn_after_first_chunk)

    assert result is not None, "a withdrawn assignment must not produce a runnable staging"
    assert result.exit_code == 125
    assert "no longer this agent" in (result.failure_message or "")
    assert calls["n"] > 1, "the download must actually have been checked more than once"
    attempt_root = tmp_path / "attempts" / str(job.attempt_id)
    assert not (attempt_input_root(attempt_root) / "training").exists()
    # And nothing half-fetched may be left under a digest's name in the shared cache.
    cache = tmp_path / "dataset-cache"
    assert [path for path in cache.rglob("*") if path.is_file()] == []


def test_the_staging_tick_refuses_once_the_execution_lease_has_passed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Staging runs under the execution lease and nothing extends it.

    A download still running after the lease expires is work for an attempt this agent no longer
    owns, so the deadline is a second condition beside "the server still holds it".
    """
    from kratos_agent import runner as runner_module

    now = [datetime(2026, 9, 28, 12, 0, tzinfo=UTC)]
    runner = object.__new__(runner_module.AgentRunner)
    monkeypatch.setattr(runner, "_clock", lambda: now[0], raising=False)
    # Held by the server throughout, so the only thing that can stop it is the deadline.
    monkeypatch.setattr(runner, "_delivery_tick", lambda holder, job: lambda: True, raising=False)

    job = assignment(lease_expires_at=now[0] + timedelta(minutes=5))
    tick = runner._staging_tick([cast(Any, object())], job)

    assert tick() is True
    now[0] = job.lease_expires_at - timedelta(seconds=1)
    assert tick() is True
    now[0] = job.lease_expires_at
    assert tick() is False, "at the deadline the attempt is no longer this agent's"
    now[0] = job.lease_expires_at + timedelta(minutes=1)
    assert tick() is False


def test_the_staging_tick_refuses_when_the_server_no_longer_holds_the_attempt(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from kratos_agent import runner as runner_module

    now = datetime(2026, 9, 28, 12, 0, tzinfo=UTC)
    runner = object.__new__(runner_module.AgentRunner)
    monkeypatch.setattr(runner, "_clock", lambda: now, raising=False)
    monkeypatch.setattr(runner, "_delivery_tick", lambda holder, job: lambda: False, raising=False)

    job = assignment(lease_expires_at=now + timedelta(minutes=5))
    tick = runner._staging_tick([cast(Any, object())], job)

    assert tick() is False, "a withdrawn assignment stops staging even inside the lease"


def test_staging_is_wired_to_a_real_tick_rather_than_a_constant() -> None:
    """Guard against the defect the first revision shipped.

    The plumbing was there and was handed `lambda: True`, so the chunk-level check could never
    fire however large the download. This asserts the call site passes the heartbeat-aware tick.
    """
    import inspect

    from kratos_agent import runner as runner_module

    source = inspect.getsource(runner_module.AgentRunner._run_assignment)
    staging = source.split("_stage_dataset_inputs(", 1)[1]
    assert "self._staging_tick(" in staging, staging[:400]
    assert "lambda: True" not in staging.split(")", 1)[0]
