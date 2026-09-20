"""Turn an attempt's output lines into delivered observations, without ever touching supervision.

This is the join between the classifier, the spool and the control plane. It exists as its own
object, with no Docker and no HTTP client of its own, because the property that matters most is
negative: ADR-015 requires that deadline enforcement, the lease and the result report never wait on
telemetry, and that a telemetry failure never raise into supervision. A component that cannot reach
the supervision path is easier to trust with that than a promise not to.

So every public method here swallows its own failures and records them. The worst an outage, a
malformed line or a broken spool can do is lose diagnostic data and leave a number saying how much.
"""

import threading
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime

from kratos_agent.models import ObservationRecord
from kratos_agent.observations import Drop, Kind, Observation, ObservationCollector, Stream
from kratos_agent.spool import Batch, ObservationSpool

CONTROL_PLANE_SINK = "control_plane"
OTLP_SINK = "otlp"
# Codex's control plane caches a successful credential verification for sixty seconds, so a
# two-second batch no longer pays an Argon2 verification per request.
DEFAULT_BATCH_INTERVAL_SECONDS = 2.0

# What goes to the control plane, and so to MLflow. Log lines belong to the OTLP path.
_DELIVERED_TO_CONTROL_PLANE = (Kind.PARAM, Kind.METRIC, Kind.PROGRESS)

# A send that failed and will be retried. Reported by the courier, which is what sends.
FAILURE_COUNTER = "delivery.failures"
# A line the agent could not write down. Unlike a failed send, this record is gone.
WRITE_FAILURE_COUNTER = "dropped.spool_write_failed"
# Records the agent accepted and then gave up on, counted as records rather than as events.
ABANDONED_COUNTER = "dropped.delivery_abandoned"


@dataclass(frozen=True)
class Delivery:
    """The outcome of one attempt to send, as far as the caller needs to know."""

    sent: bool
    accepted_through_sequence: int | None = None
    detail: str | None = None


class ObservationPump:
    """Classifies an attempt's lines and stores what a sink will want.

    It does not deliver. Sending belongs to the agent-level courier, because a spool outlives the
    attempt that filled it and often the process as well; a pump that also delivered would tie
    delivery to an assignment, which is exactly the fault this design was corrected for.

    One pump belongs to one attempt and is called from more than one thread: the log reader calls
    `ingest` while the runner calls `counters` at the moment it reports a result, which can happen
    while the reader is still handing over its last few lines. The classifier is single-threaded
    by design, so this class holds the lock for it. The spool holds its own.
    """

    def __init__(
        self,
        spool: ObservationSpool,
        collector: ObservationCollector,
        *,
        clock: Callable[[], float],
        otlp_configured: bool = False,
    ) -> None:
        self._spool = spool
        self._collector = collector
        self._clock = clock
        self._otlp_configured = otlp_configured
        self._lock = threading.Lock()
        self._result: dict[str, object] | None = None
        self._failures = 0

    # --- Taking lines in ------------------------------------------------------------------------

    def ingest(self, stream: Stream, at: datetime, line: str) -> None:
        """Classify one line and spool it for whichever sinks want it. Never raises."""
        try:
            with self._lock:
                self._ingest(stream, at, line)
        except Exception:  # noqa: BLE001 - telemetry must never raise into supervision
            self._failures += 1

    def _ingest(self, stream: Stream, at: datetime, line: str) -> None:
        observation = self._collector.observe(stream, at, line)
        if observation is None:
            return
        if observation.kind is Kind.RESULT:
            # The last result wins, and it travels with the job result rather than as an
            # observation: it is the workload's own output, not a measurement of it.
            self._result = observation.result
            return
        if observation.kind is Kind.LOG:
            self._ingest_log()
            return
        if observation.kind in _DELIVERED_TO_CONTROL_PLANE:
            self._spool.append(_wire_record(observation))

    def _ingest_log(self) -> None:
        if not self._otlp_configured:
            # Retaining it would mean holding a record behind a cursor that will never advance.
            self._collector.counters[Drop.OTLP_UNCONFIGURED.value] = (
                self._collector.counters.get(Drop.OTLP_UNCONFIGURED.value, 0) + 1
            )
            return
        self._spool.append({"record": "log", "at": None})

    def pending(self) -> bool:
        """Whether anything is still owed to the control plane.

        Never a reason to delay a result. The courier keeps sending after the attempt is terminal,
        and the control plane accepts late batches for a stream that already exists.
        """
        try:
            return self._spool.pending(CONTROL_PLANE_SINK)
        except Exception:  # noqa: BLE001
            return False

    # --- What it has to report --------------------------------------------------------------------

    @property
    def result(self) -> dict[str, object] | None:
        return self._result

    def counters(self) -> dict[str, int]:
        """A snapshot of what was refused, discarded or failed, as at this moment.

        Taken when the result is reported, which is before delivery has necessarily finished. It
        is a snapshot rather than a final account on purpose: waiting for the spool to drain would
        make a telemetry outage hold a job result open, and ADR-015 forbids exactly that. Delivery
        failures after this point stay in the agent's own logs until a later protocol version can
        update the counters after a result.

        Returned empty when nothing was counted, because the result omits the field rather than
        sending an empty object.
        """
        with self._lock:
            counters = dict(self._collector.counters)
        try:
            # Records the spool accepted and then discarded to stay bounded. Distinct from
            # `dropped.budget`, which is a line refused on the way in: this is a record that was
            # taken, promised to a sink and then given up on, and its value is a record count.
            with self._lock:
                abandoned = sum(gap.count for gap in self._spool.gaps(CONTROL_PLANE_SINK))
        except Exception:  # noqa: BLE001
            abandoned = 0
        if abandoned:
            counters[ABANDONED_COUNTER] = counters.get(ABANDONED_COUNTER, 0) + abandoned
        if self._failures:
            # A line the agent could not store at all, usually because the state volume refused
            # the write. That is a lost record, so it belongs in `dropped.`; it is deliberately
            # not `delivery.failures`, which means a send that failed and will be retried and
            # says nothing about whether the record survived.
            counters[WRITE_FAILURE_COUNTER] = self._failures
        return counters


def _wire_record(observation: Observation) -> dict[str, object]:
    """The record as it will be sent, minus the sequence the spool assigns on append."""
    record: dict[str, object] = {
        "record": observation.kind.value,
        "at": observation.at.isoformat(),
    }
    if observation.name is not None:
        record["name"] = observation.name
    if observation.value is not None:
        record["value"] = observation.value
    if observation.step is not None:
        record["step"] = observation.step
    if observation.total_steps is not None:
        record["total_steps"] = observation.total_steps
    if observation.unit is not None:
        record["unit"] = observation.unit
    return record


def batch_records(batch: Batch) -> tuple[ObservationRecord, ...]:
    """Validate a spooled batch into the models that go on the wire.

    The spool stores plain JSON so that a record written by an older agent still loads. Turning it
    into the wire model here is what stops a malformed spool entry becoming a malformed request.
    """
    return tuple(ObservationRecord.model_validate(record) for record in batch.records)
