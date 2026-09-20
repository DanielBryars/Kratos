"""The pump classifies and stores. It does not send; the courier does.

Its contract is mostly negative: it must not raise, and it must not block supervision. So most of
these break something — the spool, the line — and assert the job carries on with a number to show
for it. Delivery behaviour lives in test_courier.py.
"""

import json
import threading
from datetime import UTC, datetime
from pathlib import Path

import pytest

from kratos_agent.observations import Drop, ObservationCollector, Stream
from kratos_agent.pump import CONTROL_PLANE_SINK, WRITE_FAILURE_COUNTER, ObservationPump
from kratos_agent.spool import ObservationSpool

AT = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)


class Clock:
    def __init__(self) -> None:
        self.now = 1_000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture
def clock() -> Clock:
    return Clock()


def build(
    tmp_path: Path, clock: Clock, **options: object
) -> tuple[ObservationPump, ObservationSpool]:
    spool = ObservationSpool(tmp_path / "attempt", (CONTROL_PLANE_SINK,))
    pump = ObservationPump(
        spool=spool,
        collector=ObservationCollector(clock=clock),
        clock=clock,
        **options,  # type: ignore[arg-type]
    )
    return pump, spool


def record(**fields: object) -> str:
    return json.dumps({"schema_version": "1.0", **fields})


# --- What is kept ------------------------------------------------------------------------------


def test_measurements_are_stored_in_sequence(tmp_path: Path, clock: Clock) -> None:
    pump, spool = build(tmp_path, clock)
    pump.ingest(Stream.STDOUT, AT, record(record="param", name="model.id", value="smolvla"))
    pump.ingest(Stream.STDOUT, AT, record(record="metric", name="train.loss", value=0.5, step=1))
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1, total_steps=10))

    assert spool.last_sequence == 3
    assert pump.pending() is True


def test_a_result_travels_with_the_job_not_as_an_observation(tmp_path: Path, clock: Clock) -> None:
    pump, spool = build(tmp_path, clock)
    pump.ingest(Stream.STDOUT, AT, record(record="result", result={"steps": 1}))
    pump.ingest(Stream.STDOUT, AT, record(record="result", result={"steps": 2}))

    assert pump.result == {"steps": 2}, "the last result wins"
    assert spool.last_sequence == 0, "a result is not spooled"


def test_log_lines_are_counted_when_no_collector_is_configured(
    tmp_path: Path, clock: Clock
) -> None:
    """Retaining them would hold records behind a cursor that will never advance."""
    pump, spool = build(tmp_path, clock, otlp_configured=False)
    for _ in range(3):
        pump.ingest(Stream.STDOUT, AT, "ordinary training output")

    assert pump.counters()[Drop.OTLP_UNCONFIGURED.value] == 3
    assert spool.last_sequence == 0


def test_log_lines_are_spooled_when_a_collector_is_configured(tmp_path: Path, clock: Clock) -> None:
    pump, spool = build(tmp_path, clock, otlp_configured=True)
    pump.ingest(Stream.STDOUT, AT, "ordinary training output")
    assert Drop.OTLP_UNCONFIGURED.value not in pump.counters()
    assert spool.last_sequence == 1


# --- Nothing reaches supervision -----------------------------------------------------------------


def test_a_broken_spool_does_not_raise_and_is_counted_as_a_lost_record(
    tmp_path: Path, clock: Clock, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A line the agent cannot write down is gone, so it is counted in `dropped.`.

    Deliberately not `delivery.failures`, which means a send that will be retried and says nothing
    about whether the record survived. This one did not.
    """
    pump, _ = build(tmp_path, clock)

    def explode(*_: object, **__: object) -> None:
        raise OSError("the state volume went away")

    monkeypatch.setattr(ObservationSpool, "append", explode)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))  # must not raise

    assert pump.counters()[WRITE_FAILURE_COUNTER] == 1


def test_a_hostile_line_does_not_raise(tmp_path: Path, clock: Clock) -> None:
    pump, _ = build(tmp_path, clock)
    for line in ("{" * 500, "\udcff", '{"schema_version":"1.0","record":null}'):
        pump.ingest(Stream.STDOUT, AT, line)


# --- Counters -------------------------------------------------------------------------------------


def test_counters_are_empty_when_nothing_was_counted(tmp_path: Path, clock: Clock) -> None:
    """An empty object is refused by the result model, so the field is omitted instead."""
    pump, _ = build(tmp_path, clock)
    pump.ingest(Stream.STDOUT, AT, record(record="progress", step=1))
    assert pump.counters() == {}


def test_counters_carry_what_the_classifier_refused(tmp_path: Path, clock: Clock) -> None:
    pump, _ = build(tmp_path, clock)
    pump.ingest(Stream.STDOUT, AT, "x" * 9000)  # oversize
    pump.ingest(Stream.STDOUT, AT, record(record="metric", name="a", value=1))  # malformed

    counters = pump.counters()
    assert counters[Drop.OVERSIZE.value] == 1
    assert counters[Drop.MALFORMED.value] == 1


def test_counters_include_records_the_spool_had_to_discard(
    tmp_path: Path, clock: Clock, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("kratos_agent.spool.MAX_SPOOL_BYTES", 400)
    pump, _ = build(tmp_path, clock)
    for step in range(40):
        clock.advance(0.1)
        pump.ingest(Stream.STDOUT, AT, record(record="progress", step=step))

    assert pump.counters()["dropped.delivery_abandoned"] > 0


# --- Concurrency ----------------------------------------------------------------------------------
#
# The executor signals its log reader to stop and does not wait for it, because a result may never
# wait on telemetry. So the reader can still be calling ingest() while the runner takes the counter
# snapshot and reports the result.


def test_ingest_and_counters_are_safe_from_two_threads(tmp_path: Path, clock: Clock) -> None:
    pump, _ = build(tmp_path, clock)
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
            snapshot = pump.counters()
            assert all(value >= 0 for value in snapshot.values())
            pump.pending()
    finally:
        stop.set()
        reader.join(timeout=10)

    assert errors == []
