"""The runner's side of observation streaming: build a pump, feed it, report what it counted.

The interesting cases are the ones where the pump should not exist at all, and the one where the
control plane is unreachable for telemetry while the job itself finishes perfectly well.
"""

from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from uuid import UUID

import pytest

from kratos_agent.executor import LogObserver
from kratos_agent.models import (
    JobAssignment,
    JobExecutionResult,
    ObservationBatchResponse,
)
from kratos_agent.observations import Stream
from kratos_agent.runner import AgentRunner
from kratos_agent.state import AgentState

T0 = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)
WORKER_ID = UUID("11111111-1111-4111-8111-111111111111")
ATTEMPT_ID = UUID("33333333-3333-4333-8333-333333333333")
STREAM_ID = UUID("55555555-5555-4555-8555-555555555555")
IMAGE = "example.com/workload@sha256:" + "a" * 64


class Client:
    """Records every batch, and can be told the telemetry endpoint is down."""

    def __init__(self) -> None:
        self.batches: list[tuple[UUID, int, int]] = []
        self.fail = False

    def submit_observation_batch(
        self,
        worker_id: UUID,
        credential: str,
        stream_id: UUID,
        batch_id: UUID,
        first_sequence: int,
        records: tuple[Any, ...],
    ) -> ObservationBatchResponse:
        if self.fail:
            raise RuntimeError("observation endpoint is unreachable")
        assert worker_id == WORKER_ID and stream_id == STREAM_ID
        self.batches.append((batch_id, first_sequence, len(records)))
        return ObservationBatchResponse(
            stream_id=stream_id,
            batch_id=batch_id,
            accepted_through_sequence=first_sequence + len(records) - 1,
        )


class Executor:
    """Stands in for the container, and replays lines through whatever observer it is given."""

    def __init__(self, lines: list[tuple[Stream, str]]) -> None:
        self.lines = lines
        self.observed: LogObserver | None = None

    def run_job(
        self,
        assignment: JobAssignment,
        *,
        may_start: bool = True,
        on_tick: Any = None,
        tick_seconds: float = 30,
        observe: LogObserver | None = None,
    ) -> JobExecutionResult:
        self.observed = observe
        if observe is not None:
            for stream, line in self.lines:
                observe(stream, T0, line)
        return JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")


def state() -> AgentState:
    return AgentState(
        agent_instance_id=UUID("22222222-2222-4222-8222-222222222222"),
        worker_id=WORKER_ID,
        worker_credential="kwc_test",
    )


def assignment(*, stream: UUID | None) -> JobAssignment:
    return JobAssignment(
        attempt_id=ATTEMPT_ID,
        job_id=UUID("44444444-4444-4444-8444-444444444444"),
        name="job",
        image_reference=IMAGE,
        gpu_index=0,
        timeout_seconds=60,
        lease_expires_at=T0 + timedelta(minutes=5),
        observation_stream_id=stream,
    )


@pytest.fixture
def runner(tmp_path: Path) -> AgentRunner:
    return AgentRunner(
        client=Client(),  # type: ignore[arg-type]
        display_name="test",
        state_path=tmp_path / "state.json",
        enrolment_credential_path=None,
    )


def record(**fields: object) -> str:
    import json

    return json.dumps({"schema_version": "1.0", **fields})


def test_no_stream_means_no_pump(runner: AgentRunner) -> None:
    """An agent whose control plane allocated no stream must not spool for nowhere."""
    pump, delivery = runner._build_pump(state(), assignment(stream=None))
    assert pump is None and delivery is None


def test_an_unenrolled_worker_builds_no_pump(runner: AgentRunner) -> None:
    pump, _ = runner._build_pump(
        AgentState(agent_instance_id=UUID("22222222-2222-4222-8222-222222222222")),
        assignment(stream=STREAM_ID),
    )
    assert pump is None


def test_records_from_the_container_reach_the_control_plane(runner: AgentRunner) -> None:
    pump, delivery = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None and delivery is not None
    delivery.stop()  # drive delivery by hand so the test does not depend on a thread

    pump.ingest(Stream.STDOUT, T0, record(record="param", name="model.id", value="smolvla"))
    pump.ingest(Stream.STDOUT, T0, record(record="metric", name="train.loss", value=0.5, step=1))
    assert pump.deliver(force=True).sent

    client: Client = runner._client  # type: ignore[assignment]
    assert len(client.batches) == 1
    _, first_sequence, count = client.batches[0]
    assert first_sequence == 1 and count == 2


def test_the_counters_reach_the_result(runner: AgentRunner) -> None:
    executor = Executor([(Stream.STDOUT, "x" * 9000), (Stream.STDOUT, "plain output")])
    runner._executor = executor  # type: ignore[assignment]
    pump, delivery = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None and delivery is not None
    delivery.stop()

    _, result, _ = runner._supervise(
        state(), assignment(stream=STREAM_ID), may_start=True, pump=pump
    )
    from kratos_agent.runner import _with_observations

    reported = _with_observations(result, pump)
    assert reported.observation_counters is not None
    assert reported.observation_counters["dropped.oversize"] == 1
    assert reported.observation_counters["not_exported.otlp_unconfigured"] == 1


def test_a_clean_run_reports_no_counters(runner: AgentRunner) -> None:
    """The field is omitted rather than sent empty, so absence means nothing was counted."""
    from kratos_agent.runner import _with_observations

    pump, delivery = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None and delivery is not None
    delivery.stop()
    pump.ingest(Stream.STDOUT, T0, record(record="progress", step=1))

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    assert _with_observations(result, pump).observation_counters is None


def test_an_unreachable_telemetry_endpoint_does_not_fail_the_job(runner: AgentRunner) -> None:
    """The job succeeded; only its telemetry is stuck, and that must not change the result."""
    from kratos_agent.runner import _with_observations

    client: Client = runner._client  # type: ignore[assignment]
    client.fail = True
    pump, delivery = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None and delivery is not None
    delivery.stop()

    pump.ingest(Stream.STDOUT, T0, record(record="progress", step=1))
    assert pump.deliver(force=True).sent is False

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    reported = _with_observations(result, pump)
    assert reported.exit_code == 0
    assert reported.observation_counters == {"delivery.failures": 1}
    # Still owed, and kept for later rather than lost or waited on.
    assert pump.pending() is True
