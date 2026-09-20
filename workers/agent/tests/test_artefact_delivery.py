"""The agent's side of the durable output flow, from a finished container to a verified artefact."""

import hashlib
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from uuid import UUID

import httpx
import pytest
from test_protocol import WORKER_ID, WORKER_SECRET, FakeJobExecutor, capabilities

from kratos_agent.models import (
    JobAssignment,
    JobExecutionResult,
)
from kratos_agent.outputs import DESCRIPTOR_WALK_SUPPORTED
from kratos_agent.protocol import WorkerProtocolClient
from kratos_agent.runner import AgentRunner
from kratos_agent.state import AgentState, save_state

ATTEMPT_ID = UUID("33333333-3333-4333-8333-333333333333")
JOB_ID = UUID("44444444-4444-4444-8444-444444444444")
ARTIFACT_ID = UUID("55555555-5555-4555-8555-555555555555")
SESSION = "https://storage.googleapis.test/upload?upload_id=secret-session-value"

needs_posix = pytest.mark.skipif(
    not DESCRIPTOR_WALK_SUPPORTED, reason="output collection requires a POSIX host"
)


def requirement(mandatory: bool = True) -> dict[str, object]:
    return {
        "logical_path": "model.pt",
        "role": "model",
        "media_type": "application/octet-stream",
        "mandatory": mandatory,
        "max_bytes": 1024,
    }


def assignment_body(requirements: list[dict[str, object]]) -> dict[str, object]:
    return {
        "attempt_id": str(ATTEMPT_ID),
        "job_id": str(JOB_ID),
        "name": "Training",
        "image_reference": "example.test/work@sha256:" + ("a" * 64),
        "gpu_index": 0,
        "timeout_seconds": 120,
        "lease_expires_at": (datetime.now(UTC) + timedelta(hours=1)).isoformat(),
        "output_requirements": requirements,
    }


def artifact_body(status: str = "declared") -> dict[str, object]:
    return {
        "artifact_id": str(ARTIFACT_ID),
        "logical_path": "model.pt",
        "role": "model",
        "media_type": "application/octet-stream",
        "mandatory": True,
        "byte_length": 9,
        "sha256": "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225",
        "crc32c": "4waSgw==",
        "object_key": "v1/projects/p/jobs/j/attempts/a/artifacts/x",
        "status": status,
        "storage_generation": None,
        "upload_started_at": None,
        "upload_completed_at": None,
        "verification_pending": status == "uploading",
    }


class Recorder:
    """A control plane and a Cloud Storage session, recording the order of every call."""

    def __init__(self, *, requirements: list[dict[str, object]] | None = None) -> None:
        self.requirements = requirements if requirements is not None else [requirement()]
        self.calls: list[str] = []
        self.uploaded = bytearray()
        self.completed: dict[str, Any] = {}
        self.manifest_ids: list[str] = []
        self.assigned = True

    def __call__(self, request: httpx.Request) -> httpx.Response:
        path = request.url.path
        if str(request.url).startswith(SESSION.split("?")[0]):
            return self._storage(request)
        if path.endswith("/heartbeat"):
            self.calls.append("heartbeat")
            body: dict[str, object] = {
                "worker_id": str(WORKER_ID),
                "state": "busy" if self.assigned else "idle",
                "accepted_sequence": json.loads(request.content)["sequence"],
                "next_heartbeat_seconds": 30,
            }
            if self.assigned:
                body["assignment"] = assignment_body(self.requirements)
            return httpx.Response(200, json=body)
        if path.endswith("/artifact-manifest"):
            self.calls.append("manifest")
            payload = json.loads(request.content)
            self.manifest_ids.append(payload["manifest_id"])
            assert payload["protocol_version"] == "1.1"
            return httpx.Response(
                200,
                json={
                    "manifest_id": payload["manifest_id"],
                    "attempt_id": str(ATTEMPT_ID),
                    "artifacts": [artifact_body()],
                },
            )
        if path.endswith("/upload"):
            self.calls.append("begin-upload")
            return httpx.Response(
                200,
                json={
                    "artifact": artifact_body("uploading"),
                    "session": {
                        "uri": SESSION,
                        "method": "PUT",
                        "expires_at": (datetime.now(UTC) + timedelta(minutes=10)).isoformat(),
                    },
                },
            )
        if path.endswith("/complete-upload"):
            self.calls.append("complete-upload")
            self.completed = json.loads(request.content)
            return httpx.Response(200, json={"status": "verified"})
        self.calls.append("result")
        return httpx.Response(
            200, json={"attempt_id": str(ATTEMPT_ID), "job_id": str(JOB_ID), "status": "succeeded"}
        )

    def _storage(self, request: httpx.Request) -> httpx.Response:
        sent = request.headers["Content-Range"]
        if sent.startswith("bytes */"):
            self.calls.append("probe")
            return httpx.Response(308)
        self.calls.append("send")
        self.uploaded.extend(request.content)
        return httpx.Response(200, headers={"x-goog-generation": "41"})


def enrolled(state_path: Path) -> AgentState:
    state = AgentState(
        agent_instance_id=UUID(int=1), worker_id=WORKER_ID, worker_credential=WORKER_SECRET
    )
    save_state(state_path, state)
    return state


def outputs_directory(state_path: Path) -> Path:
    return state_path.parent / "attempts" / str(ATTEMPT_ID) / "outputs"


def runner(
    client: WorkerProtocolClient, state_path: Path, executor: FakeJobExecutor
) -> AgentRunner:
    return AgentRunner(
        client, "GPU host", state_path, None, capability_collector=capabilities, executor=executor
    )


class ProducingExecutor(FakeJobExecutor):
    """An executor that writes what the job would have written into the attempt directory."""

    def __init__(self, state_path: Path, content: bytes = b"123456789") -> None:
        super().__init__()
        self.state_path = state_path
        self.content = content

    def run_job(self, assignment: JobAssignment, **kwargs: Any) -> JobExecutionResult:
        directory = outputs_directory(self.state_path)
        directory.mkdir(parents=True, exist_ok=True)
        if self.content:
            (directory / "model.pt").write_bytes(self.content)
        return super().run_job(assignment, **kwargs)


@needs_posix
def test_outputs_are_declared_uploaded_and_completed_before_the_result(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(recorder)
    ) as client:
        runner(client, state_path, ProducingExecutor(state_path)).heartbeat_once(state)

    assert recorder.calls == [
        "heartbeat",
        "manifest",
        "begin-upload",
        "probe",
        "send",
        "complete-upload",
        "result",
    ]
    assert bytes(recorder.uploaded) == b"123456789"
    assert recorder.completed["storage_generation"] == 41
    assert recorder.completed["sha256"] == artifact_body()["sha256"]
    assert recorder.completed["crc32c"] == "4waSgw=="


@needs_posix
def test_the_attempt_directory_is_discarded_once_the_result_is_acknowledged(
    tmp_path: Path,
) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(Recorder())
    ) as client:
        runner(client, state_path, ProducingExecutor(state_path)).heartbeat_once(state)

    assert not outputs_directory(state_path).parent.exists()


@needs_posix
def test_a_replayed_attempt_declares_the_same_manifest(tmp_path: Path) -> None:
    # A lost manifest response must not produce a second, conflicting manifest.
    state_path = tmp_path / "agent.json"
    recorder = Recorder()

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(recorder)
    ) as client:
        for _ in range(2):
            state = enrolled(state_path)
            runner(client, state_path, ProducingExecutor(state_path)).heartbeat_once(state)

    assert len(recorder.manifest_ids) == 2
    assert len(set(recorder.manifest_ids)) == 1


@needs_posix
def test_a_job_that_produced_no_mandatory_output_is_reported_as_failed(tmp_path: Path) -> None:
    # Reporting success would be refused by the control plane, leaving the attempt stuck.
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()
    reported: dict[str, Any] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/result"):
            reported.update(json.loads(request.content))
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner(client, state_path, ProducingExecutor(state_path, content=b"")).heartbeat_once(state)

    assert "manifest" not in recorder.calls
    assert reported["exit_code"] == 125
    assert "outputs could not be collected" in reported["failure_message"]


@needs_posix
def test_a_job_without_declared_outputs_uploads_nothing(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder(requirements=[])

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(recorder)
    ) as client:
        runner(client, state_path, FakeJobExecutor()).heartbeat_once(state)

    assert recorder.calls == ["heartbeat", "result"]


@needs_posix
def test_an_artefact_already_verified_is_not_uploaded_again(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/artifact-manifest"):
            recorder.calls.append("manifest")
            payload = json.loads(request.content)
            return httpx.Response(
                200,
                json={
                    "manifest_id": payload["manifest_id"],
                    "attempt_id": str(ATTEMPT_ID),
                    "artifacts": [artifact_body("verified")],
                },
            )
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner(client, state_path, ProducingExecutor(state_path)).heartbeat_once(state)

    assert recorder.calls == ["heartbeat", "manifest", "result"]


@needs_posix
def test_an_outage_during_upload_keeps_the_attempt_for_the_next_pass(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()

    def handler(request: httpx.Request) -> httpx.Response:
        if str(request.url).startswith(SESSION.split("?")[0]):
            raise httpx.ConnectError("network is unreachable", request=request)
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        after = runner(client, state_path, ProducingExecutor(state_path)).step(state)

    assert "result" not in recorder.calls
    assert after.started_attempt_id == ATTEMPT_ID
    assert outputs_directory(state_path).exists()
    printed = capsys.readouterr().out
    assert "secret-session-value" not in printed
    assert "upload_id" not in printed


class WithdrawingExecutor(ProducingExecutor):
    """The control plane stops holding the attempt while the container runs."""

    def run_job(self, assignment: JobAssignment, **kwargs: Any) -> JobExecutionResult:
        result = super().run_job(assignment, **kwargs)
        on_tick = kwargs.get("on_tick")
        if on_tick is not None:
            on_tick()
        return result


@needs_posix
def test_lost_authority_skips_delivery_and_still_acknowledges(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            # The first heartbeat assigns; the in-job tick no longer carries the attempt.
            recorder.assigned = not recorder.calls
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        after = runner(client, state_path, WithdrawingExecutor(state_path)).heartbeat_once(state)

    assert "manifest" not in recorder.calls
    assert "begin-upload" not in recorder.calls
    assert "result" in recorder.calls
    assert after.started_attempt_id is None
    assert not outputs_directory(state_path).parent.exists()


@needs_posix
def test_a_failed_workload_is_reported_without_waiting_for_storage(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()
    reported: dict[str, Any] = {}

    class FailingExecutor(ProducingExecutor):
        def run_job(self, assignment: JobAssignment, **kwargs: Any) -> JobExecutionResult:
            super().run_job(assignment, **kwargs)
            return JobExecutionResult(
                exit_code=1,
                timed_out=False,
                stdout="",
                stderr="boom",
                failure_message="container exited with code 1",
            )

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/artifact-manifest"):
            raise httpx.ConnectError("storage is unreachable", request=request)
        if request.url.path.endswith("/result"):
            reported.update(json.loads(request.content))
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner(client, state_path, FailingExecutor(state_path)).heartbeat_once(state)

    # The workload's own failure reaches the control plane even though delivery would have failed.
    assert reported["exit_code"] == 1
    assert reported["failure_message"] == "container exited with code 1"


@needs_posix
def test_a_lost_completion_response_is_replayed_without_uploading_again(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/artifact-manifest"):
            recorder.calls.append("manifest")
            payload = json.loads(request.content)
            # The server already recorded the generation; only the acknowledgement was lost.
            artifact = artifact_body("uploading") | {"storage_generation": 41}
            return httpx.Response(
                200,
                json={
                    "manifest_id": payload["manifest_id"],
                    "attempt_id": str(ATTEMPT_ID),
                    "artifacts": [artifact],
                },
            )
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner(client, state_path, ProducingExecutor(state_path)).heartbeat_once(state)

    assert recorder.calls == ["heartbeat", "manifest", "complete-upload", "result"]
    assert recorder.completed["storage_generation"] == 41


@needs_posix
def test_a_dead_session_is_abandoned_and_replaced(tmp_path: Path) -> None:
    state_path = tmp_path / "agent.json"
    state = enrolled(state_path)
    recorder = Recorder()
    fingerprints: list[str] = []
    refusals = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal refusals
        if str(request.url).startswith(SESSION.split("?")[0]):
            if refusals == 0:
                refusals += 1
                return httpx.Response(410)
            return recorder(request)
        if request.url.path.endswith("/abandon-upload"):
            recorder.calls.append("abandon")
            payload = json.loads(request.content)
            fingerprints.append(payload["session_uri_sha256"])
            assert "storage.googleapis" not in request.content.decode()
            return httpx.Response(204)
        return recorder(request)

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner(client, state_path, ProducingExecutor(state_path)).heartbeat_once(state)

    assert recorder.calls.count("begin-upload") == 2
    assert "abandon" in recorder.calls
    assert fingerprints == [hashlib.sha256(SESSION.encode()).hexdigest()]
    assert recorder.completed["storage_generation"] == 41
