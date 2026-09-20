"""The pump's contract is mostly negative: it must not raise, and must not block supervision.

So most of these tests break something — the spool, the link, the line — and assert that the job
carries on and the damage arrives as a number instead of an exception.
"""

import json
from datetime import UTC, datetime
from pathlib import Path

import pytest

from kratos_agent.observations import Drop, ObservationCollector, Stream
from kratos_agent.pump import (
    ABANDONED_COUNTER,
    CONTROL_PLANE_SINK,
    FAILURE_COUNTER,
    ObservationPump,
    batch_records,
)
from kratos_agent.spool import Batch, ObservationSpool

AT = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)


class Clock:
    def __init__(self) -> None:
        self.now = 1_000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


class Link:
    """A control plane that can be told to fail, and remembers what it was sent."""

    def __init__(self) -> None:
        self.batches: list[Batch] = []
        self.fail_with: Exception | None = None

    def __call__(self, batch: Batch) -> int:
        if self.fail_with is not None:
            raise self.fail_with
        self.batches.append(batch)
        return batch.last_sequence


@pytest.fixture
def clock() -> Clock:
    return Clock()


@pytest.fixture
def link() -> Link:
    return Link()


def build(tmp_path: Path, clock: Clock, link: Link, **options: object) -> ObservationPump:
    return ObservationPump(
        spool=ObservationSpool(tmp_path / "attempt", (CONTROL_PLANE_SINK,)),
        collector=ObservationCollector(clock=clock),
        send=link,
        clock=clock,
        **options,  # type: ignore[arg-type]
    )


def record(**fields: object) -> str:
    return json.dumps({"schema_version": "1.0", **fields})


# --- What reaches the control plane -----------------------------------------------------------


def test_measurements_are_spooled_and_delivered(tmp_path: Path, clock: Clock, link: Link) -> None:
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, record(record="param", name="model.id", value="smolvla"))
    pump.ingest(Stream.STDOUT, AT, record(record="metric", name="train.loss", value=0.5, step=1))
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1, total_steps=10))

    delivery = pump.deliver(force=True)
    assert delivery.sent and delivery.accepted_through_sequence == 3
    assert len(link.batches) == 1
    # The batch validates into the wire models, which is what stops a bad spool entry becoming a
    # bad request.
    assert len(batch_records(link.batches[0])) == 3


def test_a_result_travels_with_the_job_not_as_an_observation(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, record(record="result", result={"steps": 1}))
    pump.ingest(Stream.STDOUT, AT, record(record="result", result={"steps": 2}))

    assert pump.result == {"steps": 2}, "the last result wins"
    assert pump.deliver(force=True).sent is False, "a result is not spooled"


def test_log_lines_are_counted_when_no_collector_is_configured(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    """Retaining them would hold records behind a cursor that will never advance."""
    pump = build(tmp_path, clock, link, otlp_configured=False)
    for _ in range(3):
        pump.ingest(Stream.STDOUT, AT, "ordinary training output")

    assert pump.counters()[Drop.OTLP_UNCONFIGURED.value] == 3
    assert pump.deliver(force=True).sent is False


def test_log_lines_are_spooled_when_a_collector_is_configured(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    pump = build(tmp_path, clock, link, otlp_configured=True)
    pump.ingest(Stream.STDOUT, AT, "ordinary training output")
    assert Drop.OTLP_UNCONFIGURED.value not in pump.counters()


# --- The interval ---------------------------------------------------------------------------


def test_delivery_waits_for_its_interval(tmp_path: Path, clock: Clock, link: Link) -> None:
    pump = build(tmp_path, clock, link, batch_interval_seconds=2.0)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))
    assert pump.deliver(force=True).sent

    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=2))
    assert pump.deliver().sent is False, "too soon"
    clock.advance(2.0)
    assert pump.deliver().sent is True


# --- Nothing reaches supervision ----------------------------------------------------------------


def test_an_outage_does_not_raise_and_the_batch_is_kept(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))
    link.fail_with = RuntimeError("control plane unreachable")

    delivery = pump.deliver(force=True)
    assert delivery.sent is False and "unreachable" in (delivery.detail or "")

    # The same batch is resent once the link returns, which is safe because the control plane is
    # idempotent on stream and sequence.
    link.fail_with = None
    clock.advance(10)
    assert pump.deliver().sent is True
    assert link.batches[0].first_sequence == 1


def test_a_broken_spool_does_not_raise_into_supervision(
    tmp_path: Path, clock: Clock, link: Link, monkeypatch: pytest.MonkeyPatch
) -> None:
    pump = build(tmp_path, clock, link)

    def explode(*_: object, **__: object) -> None:
        raise OSError("the state volume went away")

    monkeypatch.setattr(ObservationSpool, "append", explode)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))  # must not raise

    monkeypatch.setattr(ObservationSpool, "next_batch", explode)
    assert pump.deliver(force=True).sent is False
    assert pump.counters()[FAILURE_COUNTER] >= 1


def test_a_hostile_line_does_not_raise(tmp_path: Path, clock: Clock, link: Link) -> None:
    pump = build(tmp_path, clock, link)
    for line in ("{" * 500, "\udcff", '{"schema_version":"1.0","record":null}'):
        pump.ingest(Stream.STDOUT, AT, line)


# --- Refusal and draining -------------------------------------------------------------------------


def test_a_refused_batch_is_abandoned_and_reassembled(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    """A 409 means the identifier names different content, so resending it cannot succeed."""
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))
    link.fail_with = RuntimeError("409")
    pump.deliver(force=True)

    pump.refuse_current_batch()
    link.fail_with = None
    clock.advance(10)
    assert pump.deliver().sent is True
    assert link.batches[0].first_sequence == 1


def test_delivery_continues_across_many_batches(tmp_path: Path, clock: Clock, link: Link) -> None:
    pump = build(tmp_path, clock, link)
    for step in range(250):
        # The clock moves as it would in a real run: without it the classifier's rate limiter
        # refuses everything past its hundred-record burst.
        clock.advance(0.1)
        pump.ingest(Stream.STDOUT, AT, record(record="progress", step=step))

    while pump.pending():
        clock.advance(5)
        if not pump.deliver().sent:
            break
    assert len(link.batches) >= 3, "more than one batch is needed for 250 records"
    assert not pump.counters(), "a paced workload should lose nothing"


def test_an_undelivered_spool_does_not_hold_a_result_open(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    """The lifecycle rule: a telemetry outage must never delay a job result.

    The pump reports what it knows at this moment and keeps the rest for an agent-level pump that
    outlives the attempt. The control plane accepts late batches for a stream that already exists.
    """
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))
    link.fail_with = RuntimeError("control plane unreachable")
    pump.deliver(force=True)

    # Still owed, and the caller can see that without being made to wait for it.
    assert pump.pending() is True
    snapshot = pump.counters()
    assert snapshot[FAILURE_COUNTER] == 1
    assert ABANDONED_COUNTER not in snapshot, "a failed attempt is not a discarded record"

    # And it is still deliverable afterwards, which is what makes the snapshot acceptable.
    link.fail_with = None
    clock.advance(10)
    assert pump.deliver().sent is True
    assert pump.pending() is False


# --- Counters -----------------------------------------------------------------------------------


def test_counters_are_empty_when_nothing_was_counted(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    """An empty object is refused by the result model, so the field is omitted instead."""
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))
    assert pump.counters() == {}


def test_counters_carry_the_classifier_and_the_spool(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    pump = build(tmp_path, clock, link)
    pump.ingest(Stream.STDOUT, AT, "x" * 9000)  # oversize
    pump.ingest(Stream.STDOUT, AT, record(record="metric", name="a", value=1))  # malformed

    counters = pump.counters()
    assert counters[Drop.OVERSIZE.value] == 1
    assert counters[Drop.MALFORMED.value] == 1


# --- Concurrency ---------------------------------------------------------------------------------
#
# The executor signals its log reader to stop and does not wait for it, because a result may never
# wait on telemetry. That means the reader can still be calling ingest() while the runner takes the
# counter snapshot and reports the result. The classifier and the spool are single-threaded by
# design, so the pump is what makes that safe.


def test_ingest_and_counters_are_safe_from_two_threads(
    tmp_path: Path, clock: Clock, link: Link
) -> None:
    """Without the pump's lock this races on the counter dictionary and the spool's file."""
    import threading

    pump = build(tmp_path, clock, link)
    stop = threading.Event()
    errors: list[BaseException] = []

    def write() -> None:
        try:
            for step in range(300):
                if stop.is_set():
                    return
                pump.ingest(Stream.STDOUT, AT, record(record="progress", step=step))
                pump.ingest(Stream.STDOUT, AT, "x" * 9000)  # oversize, so a counter moves too
        except BaseException as error:  # pragma: no cover - the failure is the point
            errors.append(error)

    reader = threading.Thread(target=write, daemon=True)
    reader.start()
    try:
        for _ in range(300):
            # Exactly what the runner does at the moment it reports a result.
            snapshot = pump.counters()
            assert all(value >= 0 for value in snapshot.values())
            pump.pending()
    finally:
        stop.set()
        reader.join(timeout=10)

    assert errors == []
