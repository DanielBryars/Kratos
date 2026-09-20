import base64
import json
import os
import stat
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path
from uuid import UUID

import httpx
import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
from test_executor import T0, FakeClock, JobClient, JobContainer, job_assignment

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


ATTEMPT_ID = UUID("33333333-3333-4333-8333-333333333333")
JOB_ID = UUID("44444444-4444-4444-8444-444444444444")
WORKER_SECRET = "kwc_identifier_scoped-secret"


class FakeJobExecutor(DockerExecutor):
    def __init__(self) -> None:
        self.prepared: list[UUID] = []
        self.runs: list[tuple[UUID, bool]] = []
        self.removed: list[UUID] = []
        self.stopped_unbounded: list[UUID] = []
        self.ticks = 0
        self.authorised: list[bool] = []
        self.tick_seconds: float | None = None

    def prepare_job(self, assignment: JobAssignment) -> JobExecutionResult | None:
        self.prepared.append(assignment.attempt_id)
        return None

    def run_job(
        self,
        assignment: JobAssignment,
        *,
        may_start: bool = True,
        on_tick: Callable[[], bool] | None = None,
        tick_seconds: float = 30,
    ) -> JobExecutionResult:
        self.runs.append((assignment.attempt_id, may_start))
        self.tick_seconds = tick_seconds
        if on_tick is not None:
            self.authorised = [on_tick() for _ in range(self.ticks)]
        return JobExecutionResult(
            exit_code=0, timed_out=False, stdout="done\n", stderr="", failure_message=None
        )

    def remove_job_container(self, attempt_id: UUID) -> None:
        self.removed.append(attempt_id)

    def stop_unbounded_attempt(self, attempt_id: UUID) -> None:
        self.stopped_unbounded.append(attempt_id)


def heartbeat_response(request: httpx.Request, *, assigned: bool) -> httpx.Response:
    body: dict[str, object] = {
        "worker_id": str(WORKER_ID),
        "state": "busy" if assigned else "idle",
        "accepted_sequence": json.loads(request.content)["sequence"],
        "next_heartbeat_seconds": 30,
    }
    if assigned:
        body["assignment"] = {
            "attempt_id": str(ATTEMPT_ID),
            "job_id": str(JOB_ID),
            "name": "Matrix check",
            "image_reference": "example.test/work@sha256:" + ("a" * 64),
            "gpu_index": 0,
            "timeout_seconds": 120,
            "lease_expires_at": "2099-09-19T13:00:00Z",
        }
    return httpx.Response(200, json=body)


def result_acknowledgement() -> httpx.Response:
    return httpx.Response(
        200,
        json={"attempt_id": str(ATTEMPT_ID), "job_id": str(JOB_ID), "status": "succeeded"},
    )


def enrolled_state(
    state_path: Path,
    *,
    started_attempt_id: UUID | None = None,
    started_assignment: JobAssignment | None = None,
) -> AgentState:
    state = AgentState(
        agent_instance_id=UUID(int=1),
        worker_id=WORKER_ID,
        worker_credential=WORKER_SECRET,
        started_attempt_id=started_attempt_id,
        started_assignment=started_assignment,
    )
    save_state(state_path, state)
    return state


def job_runner(
    client: WorkerProtocolClient, state_path: Path, executor: DockerExecutor
) -> AgentRunner:
    return AgentRunner(
        client, "GPU host", state_path, None, capability_collector=capabilities, executor=executor
    )


def test_assignment_is_executed_reported_and_removed_after_acknowledgement(tmp_path: Path) -> None:
    reported: dict[str, object] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            return heartbeat_response(request, assigned=True)
        reported.update(json.loads(request.content))
        return result_acknowledgement()

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    executor = FakeJobExecutor()
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        updated = job_runner(client, state_path, executor).heartbeat_once(state)

    assert updated.next_sequence == 1
    assert updated.started_attempt_id is None
    assert load_state(state_path) == updated
    assert executor.prepared == [ATTEMPT_ID]
    assert executor.runs == [(ATTEMPT_ID, True)]
    assert executor.removed == [ATTEMPT_ID]
    assert reported["exit_code"] == 0
    assert reported["stdout"] == "done\n"


def test_result_lost_to_a_network_outage_is_replayed_without_a_second_start(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    link_up = True
    delivered: list[dict[str, object]] = []

    class LinkDropsDuringJob(FakeJobExecutor):
        def run_job(
            self,
            assignment: JobAssignment,
            *,
            may_start: bool = True,
            on_tick: Callable[[], bool] | None = None,
            tick_seconds: float = 30,
        ) -> JobExecutionResult:
            nonlocal link_up
            if may_start:
                link_up = False
            return super().run_job(
                assignment, may_start=may_start, on_tick=on_tick, tick_seconds=tick_seconds
            )

    def handler(request: httpx.Request) -> httpx.Response:
        if not link_up:
            raise httpx.ConnectError("network is unreachable", request=request)
        if request.url.path.endswith("/heartbeat"):
            return heartbeat_response(request, assigned=True)
        delivered.append(json.loads(request.content))
        return result_acknowledgement()

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    executor = LinkDropsDuringJob()
    executor.ticks = 2
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        runner = job_runner(client, state_path, executor)

        # The assignment arrives, the link drops while it runs and the result cannot be delivered.
        state = runner.step(state)
        assert state.started_attempt_id == ATTEMPT_ID
        assert load_state(state_path) == state
        assert executor.removed == []

        # Heartbeats fail for the rest of the outage without ending the agent.
        state = runner.step(state)
        assert delivered == []

        link_up = True
        state = runner.step(state)

    assert executor.prepared == [ATTEMPT_ID]
    # The container is created once. Every later pass only resumes it: once per step during
    # the outage, then before and after the heartbeat that finally gets through.
    assert executor.runs[0] == (ATTEMPT_ID, True)
    assert executor.runs[1:] == [(ATTEMPT_ID, False)] * 3
    assert executor.authorised == [True, True]
    assert len(delivered) == 1
    assert executor.removed == [ATTEMPT_ID]
    assert state.started_attempt_id is None
    assert WORKER_SECRET not in capsys.readouterr().out


def test_attempt_no_longer_held_by_the_control_plane_is_removed(tmp_path: Path) -> None:
    # The result was recorded but its acknowledgement was lost, so the next heartbeat carries
    # no assignment and the retained container has no remaining purpose or authority.
    def handler(request: httpx.Request) -> httpx.Response:
        return heartbeat_response(request, assigned=False)

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path, started_attempt_id=ATTEMPT_ID)
    executor = FakeJobExecutor()
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        updated = job_runner(client, state_path, executor).step(state)

    assert executor.removed == [ATTEMPT_ID]
    assert executor.runs == []
    assert updated.started_attempt_id is None
    assert load_state(state_path) == updated


@pytest.mark.parametrize("status_code", [429, 500, 503])
def test_temporary_control_plane_failure_does_not_end_the_agent(
    tmp_path: Path, status_code: int
) -> None:
    def handler(_: httpx.Request) -> httpx.Response:
        return httpx.Response(status_code, json={"code": "unavailable", "message": "try later"})

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        assert job_runner(client, state_path, FakeJobExecutor()).step(state) == state


def test_rejected_credential_still_ends_the_agent(tmp_path: Path) -> None:
    def handler(_: httpx.Request) -> httpx.Response:
        return httpx.Response(401, json={"code": "unauthorized", "message": "rejected"})

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    with (
        WorkerProtocolClient(
            "https://control.example", transport=httpx.MockTransport(handler)
        ) as client,
        pytest.raises(ControlPlaneError),
    ):
        job_runner(client, state_path, FakeJobExecutor()).step(state)


def test_state_without_an_attempt_journal_remains_readable(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "agent_instance_id": str(UUID(int=1)),
                "worker_id": str(WORKER_ID),
                "worker_credential": WORKER_SECRET,
                "next_sequence": 7,
                "heartbeat_interval_seconds": 30,
            }
        ),
        encoding="utf-8",
    )

    state = load_state(path)

    assert state is not None
    assert state.next_sequence == 7
    assert state.started_attempt_id is None


def test_worker_keeps_heartbeating_while_its_job_runs(tmp_path: Path) -> None:
    sequences: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            sequences.append(json.loads(request.content)["sequence"])
            return heartbeat_response(request, assigned=True)
        return result_acknowledgement()

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    executor = FakeJobExecutor()
    executor.ticks = 2
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        updated = job_runner(client, state_path, executor).step(state)

    assert sequences == [0, 1, 2]
    assert executor.authorised == [True, True]
    assert executor.tick_seconds == 30
    assert executor.runs == [(ATTEMPT_ID, True)]
    assert updated.next_sequence == 3
    assert updated.started_attempt_id is None
    assert load_state(state_path) == updated


def test_attempt_closed_by_the_control_plane_withdraws_authority(tmp_path: Path) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            first = json.loads(request.content)["sequence"] == 0
            return heartbeat_response(request, assigned=first)
        return result_acknowledgement()

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    executor = FakeJobExecutor()
    executor.ticks = 1
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        job_runner(client, state_path, executor).step(state)

    assert executor.authorised == [False]


@pytest.mark.parametrize(("status_code", "authorised"), [(401, False), (409, False), (503, True)])
def test_only_an_explicit_rejection_withdraws_authority_mid_job(
    tmp_path: Path, status_code: int, authorised: bool
) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if not request.url.path.endswith("/heartbeat"):
            return result_acknowledgement()
        if json.loads(request.content)["sequence"] == 0:
            return heartbeat_response(request, assigned=True)
        return httpx.Response(status_code, json={"code": "refused", "message": "refused"})

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    executor = FakeJobExecutor()
    executor.ticks = 1
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        updated = job_runner(client, state_path, executor).step(state)

    assert executor.authorised == [authorised]
    assert updated.next_sequence == 1


def unreachable(request: httpx.Request) -> httpx.Response:
    raise httpx.ConnectError("network is unreachable", request=request)


def test_restart_during_an_outage_still_enforces_the_runtime_bound(tmp_path: Path) -> None:
    # The agent restarts with no route to the control plane while its container, started 100
    # seconds earlier with a 120-second bound, is still running.
    clock = FakeClock()
    container = JobContainer(clock, started_at=T0 - timedelta(seconds=100))
    executor = DockerExecutor(JobClient(clock, container), clock=clock, sleep=clock.sleep)
    assignment = job_assignment()
    state_path = tmp_path / "agent.json"
    enrolled_state(
        state_path, started_attempt_id=assignment.attempt_id, started_assignment=assignment
    )
    restarted = load_state(state_path)
    assert restarted is not None

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(unreachable)
    ) as client:
        after = job_runner(client, state_path, executor).step(restarted)

    assert container.killed is True
    assert container.exits_at == T0 + timedelta(seconds=20)
    # The result could not be delivered, so the attempt stays recorded for replay.
    assert after.started_assignment == assignment
    assert load_state(state_path) == after


def test_recorded_attempt_is_supervised_before_any_heartbeat(tmp_path: Path) -> None:
    events: list[str] = []

    class RecordingExecutor(FakeJobExecutor):
        def run_job(
            self,
            assignment: JobAssignment,
            *,
            may_start: bool = True,
            on_tick: Callable[[], bool] | None = None,
            tick_seconds: float = 30,
        ) -> JobExecutionResult:
            events.append(f"supervise may_start={may_start}")
            return super().run_job(assignment, may_start=may_start)

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            events.append("heartbeat")
            return heartbeat_response(request, assigned=True)
        events.append("result")
        return result_acknowledgement()

    assignment = JobAssignment.model_validate(
        heartbeat_response(
            httpx.Request("PUT", "https://control.example/heartbeat", json={"sequence": 0}),
            assigned=True,
        ).json()["assignment"]
    )
    state_path = tmp_path / "agent.json"
    state = enrolled_state(
        state_path, started_attempt_id=assignment.attempt_id, started_assignment=assignment
    )
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        after = job_runner(client, state_path, RecordingExecutor()).step(state)

    assert events == [
        "supervise may_start=False",
        "heartbeat",
        "supervise may_start=False",
        "result",
    ]
    assert after.started_assignment is None
    assert after.started_attempt_id is None


def test_capability_failure_during_a_job_does_not_abandon_supervision(tmp_path: Path) -> None:
    collections = 0

    def flaky_capabilities() -> WorkerCapabilities:
        nonlocal collections
        collections += 1
        if collections == 2:
            raise RuntimeError("nvidia-smi did not respond")
        return capabilities()

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/heartbeat"):
            return heartbeat_response(request, assigned=True)
        return result_acknowledgement()

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    executor = FakeJobExecutor()
    executor.ticks = 2
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        after = AgentRunner(
            client,
            "GPU host",
            state_path,
            None,
            capability_collector=flaky_capabilities,
            executor=executor,
        ).step(state)

    assert executor.authorised == [True, True]
    assert after.next_sequence == 2
    assert after.started_assignment is None


def test_capability_failure_between_jobs_does_not_end_the_agent(tmp_path: Path) -> None:
    def failing_capabilities() -> WorkerCapabilities:
        raise RuntimeError("nvidia-smi did not respond")

    state_path = tmp_path / "agent.json"
    state = enrolled_state(state_path)
    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(unreachable)
    ) as client:
        after = AgentRunner(
            client,
            "GPU host",
            state_path,
            None,
            capability_collector=failing_capabilities,
            executor=FakeJobExecutor(),
        ).step(state)

    assert after == state


def test_recorded_assignment_survives_a_state_round_trip(tmp_path: Path) -> None:
    assignment = job_assignment()
    state_path = tmp_path / "agent.json"
    state = enrolled_state(
        state_path, started_attempt_id=assignment.attempt_id, started_assignment=assignment
    )

    assert load_state(state_path) == state


def test_state_written_before_bounds_were_recorded_still_stops_its_container(
    tmp_path: Path,
) -> None:
    # An agent upgraded in place during an outage: its state file names an attempt but predates
    # the recorded bounds, so it cannot show the container is still authorised.
    clock = FakeClock()
    container = JobContainer(clock, started_at=T0 - timedelta(seconds=10))
    executor = DockerExecutor(JobClient(clock, container), clock=clock, sleep=clock.sleep)
    attempt_id = job_assignment().attempt_id
    state_path = tmp_path / "agent.json"
    state_path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "agent_instance_id": str(UUID(int=1)),
                "worker_id": str(WORKER_ID),
                "worker_credential": WORKER_SECRET,
                "next_sequence": 4,
                "heartbeat_interval_seconds": 30,
                "started_attempt_id": str(attempt_id),
            }
        ),
        encoding="utf-8",
    )
    legacy = load_state(state_path)
    assert legacy is not None and legacy.started_assignment is None

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(unreachable)
    ) as client:
        job_runner(client, state_path, executor).step(legacy)

    assert container.killed is True
    assert container.status == "exited"


def test_unstoppable_container_blocks_any_result_report(tmp_path: Path) -> None:
    requests: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request.url.path)
        return heartbeat_response(request, assigned=True)

    clock = FakeClock()
    container = JobContainer(clock, started_at=T0 - timedelta(seconds=100))
    container.unkillable = True
    executor = DockerExecutor(JobClient(clock, container), clock=clock, sleep=clock.sleep)
    assignment = job_assignment()
    state_path = tmp_path / "agent.json"
    state = enrolled_state(
        state_path, started_attempt_id=assignment.attempt_id, started_assignment=assignment
    )

    with WorkerProtocolClient(
        "https://control.example", transport=httpx.MockTransport(handler)
    ) as client:
        after = job_runner(client, state_path, executor).step(state)

    assert not any(path.endswith("/result") for path in requests)
    assert after.started_assignment == assignment
    assert container.status == "running"
