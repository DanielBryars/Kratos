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


def test_a_restarted_agent_delivers_a_pending_spool_with_no_new_assignment(
    tmp_path: Path,
) -> None:
    """The recovery case the whole design exists for, proved through the agent's own startup.

    The courier used to be built only by `_build_pump`, which needs a 1.2 assignment. A worker
    that restarted holding undelivered records and was then never given another job would have
    kept them for ever. Constructing a courier directly in a test does not prove this; entering
    `run()` does.
    """
    from kratos_agent.pump import CONTROL_PLANE_SINK as SINK
    from kratos_agent.spool import ObservationSpool

    # A previous process left records behind, bound to their stream.
    directory = tmp_path / "observations" / str(ATTEMPT_ID)
    orphan = ObservationSpool(directory, (SINK,))
    orphan.record_stream(STREAM_ID)
    orphan.append({"record": "progress", "step": 1, "at": "2026-09-20T12:00:00Z"})

    client = Client()
    runner = AgentRunner(
        client=client,  # type: ignore[arg-type]
        display_name="test",
        state_path=tmp_path / "state.json",
        enrolment_credential_path=None,
    )

    class Stop(RuntimeError):
        pass

    runner.ensure_enrolled = lambda: state()  # type: ignore[method-assign]

    def no_heartbeat(_: AgentState) -> AgentState:
        # The courier must already exist by the time the heartbeat loop is entered.
        raise Stop

    runner.step = no_heartbeat  # type: ignore[assignment,method-assign]

    with pytest.raises(Stop):
        runner.run()

    courier = runner._courier
    assert courier is not None, "the courier is built at startup, not when work arrives"

    # The courier's own thread is already running and may have sent this before the
    # assertion, so stop it and deliver explicitly. Whichever got there first, exactly
    # one batch is sent: the second attempt finds the cursor past everything.
    courier.stop()
    courier.deliver_once()
    assert client.batches, "the pending spool was delivered after a restart"
    # The thread and this call can both send before either acknowledges. That is a replay
    # of the same persisted batch, which the control plane answers idempotently on stream
    # and sequence, so the test asserts what was sent rather than how many times.
    assert set(client.batches) == {(STREAM_ID, 1, 1)}
    assert courier.pending_paths() == []


def test_the_workloads_own_result_is_carried_through(runner: AgentRunner) -> None:
    """Opaque to the agent and to the control plane: the workload's output, not a measurement."""
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    summary = {"model": {"id": "lerobot/smolvla_base"}, "steps": 2000}
    pump.ingest(Stream.STDOUT, T0, record(record="result", result=summary))

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    assert _with_observations(result, pump).structured_result == summary


def test_a_workload_that_emits_no_result_record_sends_no_field(runner: AgentRunner) -> None:
    """Every image predating the contract is in this case, so it must stay a 1.1-shaped result."""
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, '{"status": "ok"}')  # bare JSON, not a Kratos record

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    reported = _with_observations(result, pump)
    assert reported.structured_result is None
    assert "structured_result" not in reported.model_dump(exclude_none=True)


def test_an_oversized_workload_result_is_omitted_and_the_job_result_survives(
    runner: AgentRunner,
) -> None:
    """model_copy(update=...) does not validate, so the size rule was skipped on the one path
    that matters. The control plane refuses a whole result submission whose structured result is
    too large, so a workload printing an enormous final record would have cost its own job's
    outcome: evidence lost to the workload's commentary about itself.
    """
    from kratos_agent.models import MAX_STRUCTURED_RESULT_BYTES

    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(Stream.STDOUT, T0, "x" * 9000)  # oversize line, so a counter is also present

    # Past the classifier, which bounds a record line, straight onto the pump.
    pump._result = {"huge": "x" * (MAX_STRUCTURED_RESULT_BYTES + 1)}

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    reported = _with_observations(result, pump)

    assert reported.structured_result is None, "the payload is dropped"
    assert reported.exit_code == 0, "the job result is not"
    assert reported.observation_counters is not None, "and neither are the counters"
    assert reported.observation_counters["dropped.oversize"] == 1


def test_an_unserialisable_workload_result_is_omitted(runner: AgentRunner) -> None:
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump._result = {"not json": {1, 2, 3}}

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    reported = _with_observations(result, pump)
    assert reported.structured_result is None
    assert reported.exit_code == 0


def test_a_result_that_fits_is_still_carried(runner: AgentRunner) -> None:
    """The guard must not be so eager that it drops what it was added to protect."""
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    summary = {"steps": 2000, "checkpoint": "smolvla-checkpoint.tar"}
    pump.ingest(Stream.STDOUT, T0, record(record="result", result=summary))

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    assert _with_observations(result, pump).structured_result == summary


@pytest.mark.parametrize("hostile", [float("nan"), float("inf"), float("-inf")])
def test_a_non_finite_workload_result_is_omitted_not_sent(
    runner: AgentRunner, hostile: float
) -> None:
    """Python's json accepts NaN and Infinity on the way in and emits them on the way out.

    Neither is JSON, and the HTTP client refuses to encode them, so a workload result carrying
    one would have aborted the whole result request. The classifier rejects non-finite *metric*
    values, but a result payload is opaque and passes straight through `json.loads`, which
    accepts those literals quite happily.
    """
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump._result = {"loss": hostile}

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    reported = _with_observations(result, pump)

    assert reported.structured_result is None
    assert reported.exit_code == 0
    # And what is reported can actually be encoded, which is the property that was missing.
    import json as _json

    _json.dumps(reported.model_dump(mode="json"), allow_nan=False)


def test_a_result_record_carrying_nan_reaches_the_pump_and_is_still_refused(
    runner: AgentRunner,
) -> None:
    """The full path: a workload prints NaN inside its result record and the job still reports."""
    pump = runner._build_pump(state(), assignment(stream=STREAM_ID))
    assert pump is not None
    pump.ingest(
        Stream.STDOUT,
        T0,
        '{"schema_version":"1.0","record":"result","result":{"final_loss":NaN}}',
    )
    assert pump.result is not None, "json.loads accepted the NaN literal quite happily"
    assert "final_loss" in pump.result

    result = JobExecutionResult(exit_code=0, timed_out=False, stdout="", stderr="")
    assert _with_observations(result, pump).structured_result is None
