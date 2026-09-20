"""The runner's side: one courier for the agent, one pump per attempt, counters on the result."""

from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from uuid import UUID

import pytest

from kratos_agent.models import JobAssignment, JobExecutionResult, ObservationBatchResponse
from kratos_agent.observations import Stream
from kratos_agent.runner import AgentRunner, _with_observations
from kratos_agent.state import AgentState

T0 = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)
WORKER_ID = UUID("11111111-1111-4111-8111-111111111111")
ATTEMPT_ID = UUID("33333333-3333-4333-8333-333333333333")
STREAM_ID = UUID("55555555-5555-4555-8555-555555555555")
IMAGE = "example.com/workload@sha256:" + "a" * 64


class Client:
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
        self.batches.append((stream_id, first_sequence, len(records)))
        return ObservationBatchResponse(
            stream_id=stream_id,
            batch_id=batch_id,
            accepted_through_sequence=first_sequence + len(records) - 1,
        )


def state(enrolled: bool = True) -> AgentState:
    return AgentState(
        agent_instance_id=UUID("22222222-2222-4222-8222-222222222222"),
        worker_id=WORKER_ID if enrolled else None,
        worker_credential="kwc_test" if enrolled else None,
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
    assert runner._build_pump(state(), assignment(stream=None)) is None


def test_an_unenrolled_worker_builds_no_pump(runner: AgentRunner) -> None:
    assert runner._build_pump(state(enrolled=False), assignment(stream=STREAM_ID)) is None


def test_the_spool_records_its_stream_so_a_later_process_can_send_it(
    runner: AgentRunner, tmp_path: Path
) -> None:
    """The courier that delivers this may be in a process that never saw the assignment."""
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    stream_file = tmp_path / "observations" / str(ATTEMPT_ID) / "stream.json"
    assert stream_file.exists()


def test_records_reach_the_control_plane_through_the_courier(runner: AgentRunner) -> None:
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, record(record="param", name="model.id", value="smolvla"))
    pump.ingest(Stream.STDOUT, T0, record(record="metric", name="train.loss", value=0.5, step=1))

    courier = runner._courier
    assert courier is not None
    assert courier.deliver_once() == 1

    client: Client = runner._client  # type: ignore[assignment]
    assert client.batches == [(STREAM_ID, 1, 2)]


def test_one_courier_serves_every_attempt(runner: AgentRunner) -> None:
    """Delivery belongs to the agent, so a second assignment does not start a second one."""
    runner._build_pump(state(), assignment(stream=STREAM_ID))
    first = runner._courier
    runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert runner._courier is first


def test_the_counters_reach_the_result(runner: AgentRunner) -> None:
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, "x" * 9000)
    pump.ingest(Stream.STDOUT, T0, "plain output")

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    reported = _with_observations(result, pump)
    assert reported.observation_counters is not None
    assert reported.observation_counters["dropped.oversize"] == 1
    assert reported.observation_counters["not_exported.otlp_unconfigured"] == 1


def test_a_clean_run_reports_no_counters(runner: AgentRunner) -> None:
    """The field is omitted rather than sent empty, so absence means nothing was counted."""
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, record(record="progress", step=1))

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    assert _with_observations(result, pump).observation_counters is None


def test_an_unreachable_endpoint_does_not_delay_or_fail_the_result(runner: AgentRunner) -> None:
    """The job succeeded and its result goes now; the records stay owed and keep being retried."""
    client: Client = runner._client  # type: ignore[assignment]
    client.fail = True
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, record(record="progress", step=1))

    courier = runner._courier
    assert courier is not None
    assert courier.deliver_once() == 0

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    assert _with_observations(result, pump).exit_code == 0
    assert pump.pending() is True

    # And the courier, which is not tied to that attempt, sends them when the link returns.
    client.fail = False
    assert courier.deliver_once() == 1


def test_a_batch_whose_acknowledgement_names_another_stream_is_refused(
    runner: AgentRunner,
) -> None:
    """A mismatched acknowledgement means the control plane answered about something else."""

    class Wrong(Client):
        def submit_observation_batch(self, *args: Any, **kwargs: Any) -> ObservationBatchResponse:
            return ObservationBatchResponse(
                stream_id=UUID("99999999-9999-4999-8999-999999999999"),
                batch_id=args[3],
                accepted_through_sequence=1,
            )

    runner._client = Wrong()  # type: ignore[assignment]
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, record(record="progress", step=1))

    courier = runner._courier
    assert courier is not None
    assert courier.deliver_once() == 0
    assert courier.failures == 1
    assert pump.pending() is True, "an unacknowledged batch is kept"
