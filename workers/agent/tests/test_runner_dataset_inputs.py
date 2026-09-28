"""The runner's staging step: what it fetches, what it refuses, and what it leaves behind."""

from __future__ import annotations

import hashlib
from pathlib import Path
from uuid import UUID, uuid4

import httpx
import pytest

from kratos_agent.inputs import attempt_input_root
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


def runner_with(tmp_path: Path, control_plane: FakeControlPlane, monkeypatch: pytest.MonkeyPatch):
    from kratos_agent import runner as runner_module

    monkeypatch.setattr(
        runner_module,
        "new_download_client",
        lambda: httpx.Client(
            transport=httpx.MockTransport(lambda request: httpx.Response(200, content=PAYLOAD))
        ),
    )
    instance = object.__new__(runner_module.AgentRunner)
    instance._state_path = tmp_path / "state.json"  # type: ignore[attr-defined]
    instance._client = control_plane  # type: ignore[attr-defined]
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
