import json
import os
import stat
from datetime import UTC, datetime
from pathlib import Path
from uuid import UUID

import httpx
import pytest

from kratos_agent.models import GpuHealth, GpuHealthStatus, WorkerCapabilities
from kratos_agent.protocol import ControlPlaneError, WorkerProtocolClient
from kratos_agent.runner import AgentRunner
from kratos_agent.state import AgentState, load_state, save_state

WORKER_ID = UUID("11111111-1111-4111-8111-111111111111")


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
