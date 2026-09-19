import base64
import json
import os
import stat
from datetime import UTC, datetime
from pathlib import Path
from uuid import UUID

import httpx
import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

from kratos_agent.executor import DockerExecutor
from kratos_agent.models import (
    GpuHealth,
    GpuHealthStatus,
    JobAssignment,
    JobExecutionResult,
    WorkerCapabilities,
)
from kratos_agent.protocol import ControlPlaneError, WorkerProtocolClient
from kratos_agent.runner import AgentRunner, _confirmation_code
from kratos_agent.state import AgentState, load_state, save_state

WORKER_ID = UUID("11111111-1111-4111-8111-111111111111")
REGISTRATION_ID = UUID("22222222-2222-4222-8222-222222222222")


def capabilities() -> WorkerCapabilities:
    return WorkerCapabilities(
        protocol_version="1.0",
        collected_at=datetime(2026, 9, 19, 12, 0, tzinfo=UTC),
        hostname="gpu-host",
        operating_system="Linux",
        operating_system_version="6.8",
        architecture="x86_64",
        logical_cpu_count=16,
        memory_total_bytes=1024,
        storage_available_bytes=2048,
        python_version="3.12",
        gpus=(),
        gpu_health=GpuHealth(status=GpuHealthStatus.UNAVAILABLE, detail="test"),
    )


def test_enrols_and_sends_scoped_heartbeat_without_logging_secret(tmp_path: Path) -> None:
    enrolment_secret = "ken_identifier_secret-value"
    worker_secret = "kwc_identifier_scoped-secret"
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        assert request.headers["content-type"] == "application/json"
        if request.url.path == "/api/v1/worker-enrolments":
            assert request.headers["authorization"] == f"Bearer {enrolment_secret}"
            return httpx.Response(
                201,
                json={
                    "worker_id": str(WORKER_ID),
                    "worker_credential": worker_secret,
                    "state": "unapproved",
                    "heartbeat_interval_seconds": 30,
                },
            )
        assert request.headers["authorization"] == f"Bearer {worker_secret}"
        payload = json.loads(request.content)
        return httpx.Response(
            200,
            json={
                "worker_id": str(WORKER_ID),
                "state": "unapproved",
                "accepted_sequence": payload["sequence"],
                "next_heartbeat_seconds": 30,
            },
        )

    credential_path = tmp_path / "enrolment"
    credential_path.write_text(enrolment_secret, encoding="utf-8")
    state_path = tmp_path / "state" / "agent.json"
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner = AgentRunner(
            client,
            "GPU host",
            state_path,
            credential_path,
            capability_collector=capabilities,
        )
        enrolled = runner.ensure_enrolled()
        updated = runner.heartbeat_once(enrolled)

    assert len(requests) == 2
    assert updated.worker_id == WORKER_ID
    assert updated.next_sequence == 1
    assert load_state(state_path) == updated
    if os.name == "posix":
        assert stat.S_IMODE(state_path.stat().st_mode) == 0o600
    assert worker_secret not in repr(updated)


def test_structured_error_does_not_include_supplied_credential() -> None:
    supplied = "ken_identifier_do-not-print-this"

    def handler(_: httpx.Request) -> httpx.Response:
        return httpx.Response(
            401,
            json={"code": "unauthorized", "message": "The credential is invalid or unavailable."},
        )

    with (
        WorkerProtocolClient(
            "https://control.example", transport=httpx.MockTransport(handler)
        ) as client,
        pytest.raises(ControlPlaneError) as raised,
    ):
        client.enrol(supplied, UUID(int=1), "GPU host", capabilities())

    assert supplied not in str(raised.value)


def test_radio_in_registration_proves_key_possession(tmp_path: Path) -> None:
    challenge = bytes(range(32))
    challenge_encoded = base64.urlsafe_b64encode(challenge).rstrip(b"=").decode()
    worker_secret = "kwc_identifier_scoped-secret"
    public_key: bytes | None = None

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal public_key
        if request.url.path == "/api/v1/worker-registration-requests":
            payload = json.loads(request.content)
            public_key = base64.urlsafe_b64decode(payload["public_key"] + "=")
            return httpx.Response(
                201,
                json={
                    "registration_id": str(REGISTRATION_ID),
                    "confirmation_code": _confirmation_code(public_key),
                    "expires_at": "2026-09-19T13:00:00Z",
                    "poll_interval_seconds": 1,
                },
            )
        if request.url.path.endswith("/claim"):
            assert public_key is not None
            signature = json.loads(request.content)["signature"]
            signature_bytes = base64.urlsafe_b64decode(signature + "==")
            message = (f"kratos-worker-claim-v1\n{REGISTRATION_ID}\n{challenge_encoded}").encode()
            Ed25519PublicKey.from_public_bytes(public_key).verify(signature_bytes, message)
            return httpx.Response(
                201,
                json={
                    "worker_id": str(WORKER_ID),
                    "worker_credential": worker_secret,
                    "state": "idle",
                    "heartbeat_interval_seconds": 30,
                },
            )
        return httpx.Response(
            200,
            json={
                "registration_id": str(REGISTRATION_ID),
                "state": "approved",
                "claim_challenge": challenge_encoded,
                "poll_interval_seconds": 1,
            },
        )

    state_path = tmp_path / "agent.json"
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        enrolled = AgentRunner(
            client,
            "Rented GPU",
            state_path,
            None,
            capability_collector=capabilities,
        ).ensure_enrolled()

    assert enrolled.worker_id == WORKER_ID
    assert enrolled.worker_credential == worker_secret
    assert enrolled.private_key is not None
    assert enrolled.private_key not in repr(enrolled)


def test_state_is_written_atomically_and_credential_repr_is_redacted(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    state = AgentState(
        agent_instance_id=UUID(int=1),
        worker_id=WORKER_ID,
        worker_credential="kwc_identifier_private",
    )

    save_state(path, state)

    assert load_state(path) == state
    assert "kwc_identifier_private" not in repr(state)
    assert not list(tmp_path.glob(".state-*"))


@pytest.mark.parametrize(
    "url",
    ["http://control.example", "https://user:password@control.example", "control.example"],
)
def test_rejects_unsafe_control_plane_urls(url: str) -> None:
    with pytest.raises(ValueError, match="HTTPS origin"):
        WorkerProtocolClient(url)


class FakeJobExecutor(DockerExecutor):
    def __init__(self) -> None:
        self.executed: JobAssignment | None = None
        self.removed: JobAssignment | None = None

    def run_job(self, assignment: JobAssignment) -> JobExecutionResult:
        self.executed = assignment
        return JobExecutionResult(
            exit_code=0, timed_out=False, stdout="done\n", stderr="", failure_message=None
        )

    def remove_job_container(self, assignment: JobAssignment) -> None:
        self.removed = assignment


def test_assignment_is_executed_reported_and_removed_after_acknowledgement(tmp_path: Path) -> None:
    worker_secret = "kwc_identifier_scoped-secret"
    attempt_id = UUID("33333333-3333-4333-8333-333333333333")
    job_id = UUID("44444444-4444-4444-8444-444444444444")
    reported: dict[str, object] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            payload = json.loads(request.content)
            return httpx.Response(
                200,
                json={
                    "worker_id": str(WORKER_ID),
                    "state": "busy",
                    "accepted_sequence": payload["sequence"],
                    "next_heartbeat_seconds": 30,
                    "assignment": {
                        "attempt_id": str(attempt_id),
                        "job_id": str(job_id),
                        "name": "Matrix check",
                        "image_reference": "example.test/work@sha256:" + ("a" * 64),
                        "gpu_index": 0,
                        "timeout_seconds": 120,
                        "lease_expires_at": "2099-09-19T13:00:00Z",
                    },
                },
            )
        reported.update(json.loads(request.content))
        return httpx.Response(
            200,
            json={"attempt_id": str(attempt_id), "job_id": str(job_id), "status": "succeeded"},
        )

    state_path = tmp_path / "agent.json"
    state = AgentState(
        agent_instance_id=UUID(int=1),
        worker_id=WORKER_ID,
        worker_credential=worker_secret,
    )
    save_state(state_path, state)
    executor = FakeJobExecutor()
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        updated = AgentRunner(
            client,
            "GPU host",
            state_path,
            None,
            capability_collector=capabilities,
            executor=executor,
        ).heartbeat_once(state)

    assert updated.next_sequence == 1
    assert executor.executed is not None and executor.executed.attempt_id == attempt_id
    assert executor.removed is not None and executor.removed.attempt_id == attempt_id
    assert reported["exit_code"] == 0
    assert reported["stdout"] == "done\n"
