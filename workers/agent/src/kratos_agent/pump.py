"""Turn an attempt's output lines into delivered observations, without ever touching supervision.

This is the join between the classifier, the spool and the control plane. It exists as its own
object, with no Docker and no HTTP client of its own, because the property that matters most is
negative: ADR-015 requires that deadline enforcement, the lease and the result report never wait on
telemetry, and that a telemetry failure never raise into supervision. A component that cannot reach
the supervision path is easier to trust with that than a promise not to.

So every public method here swallows its own failures and records them. The worst an outage, a
malformed line or a broken spool can do is lose diagnostic data and leave a number saying how much.
"""

import json
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime

from kratos_agent.models import (
    MAX_OBSERVATION_BATCH_BYTES,
    MAX_OBSERVATION_BATCH_RECORDS,
    ObservationRecord,
)
from kratos_agent.observations import Drop, Kind, Observation, ObservationCollector, Stream
from kratos_agent.spool import Batch, ObservationSpool

CONTROL_PLANE_SINK = "control_plane"
OTLP_SINK = "otlp"
# Codex's control plane caches a successful credential verification for sixty seconds, so a
# two-second batch no longer pays an Argon2 verification per request.
DEFAULT_BATCH_INTERVAL_SECONDS = 2.0

# What goes to the control plane, and so to MLflow. Log lines belong to the OTLP path.
_DELIVERED_TO_CONTROL_PLANE = (Kind.PARAM, Kind.METRIC, Kind.PROGRESS)


@dataclass(frozen=True)
class Delivery:
    """The outcome of one attempt to send, as far as the caller needs to know."""

    sent: bool
    accepted_through_sequence: int | None = None
    detail: str | None = None


class ObservationPump:
    """Classifies lines, spools what a sink wants, and drains the spool on an interval.

    One pump belongs to one attempt. Its `ingest` is called by whatever is reading the container's
    output; its `deliver` is called from the supervision tick, where it must return quickly and
    must never raise.
    """

    def __init__(
        self,
        spool: ObservationSpool,
        collector: ObservationCollector,
        send: Callable[[Batch], int],
        *,
        clock: Callable[[], float],
        otlp_configured: bool = False,
        batch_interval_seconds: float = DEFAULT_BATCH_INTERVAL_SECONDS,
    ) -> None:
        self._spool = spool
        self._collector = collector
        self._send = send
        self._clock = clock
        self._otlp_configured = otlp_configured
        self._interval = batch_interval_seconds
        self._last_send = float("-inf")
        self._result: dict[str, object] | None = None
        self._failures = 0

    # --- Taking lines in ------------------------------------------------------------------------

    def ingest(self, stream: Stream, at: datetime, line: str) -> None:
        """Classify one line and spool it for whichever sinks want it. Never raises."""
        try:
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
        except Exception:  # noqa: BLE001 - telemetry must never raise into supervision
            self._failures += 1

    def _ingest_log(self) -> None:
        if not self._otlp_configured:
            # Retaining it would mean holding a record behind a cursor that will never advance.
            self._collector.counters[Drop.OTLP_UNCONFIGURED.value] = (
                self._collector.counters.get(Drop.OTLP_UNCONFIGURED.value, 0) + 1
            )
            return
        self._spool.append({"record": "log", "at": None})

    # --- Sending them on ------------------------------------------------------------------------

    def deliver(self, *, force: bool = False) -> Delivery:
        """Send at most one batch, if the interval has elapsed. Never raises.

        Called from the supervision tick, so it does one bounded unit of work and returns. A batch
        that fails is left in the spool: the next call resends the same one, which is safe because
        the control plane is idempotent on stream and sequence.
        """
        try:
            now = self._clock()
            if not force and now - self._last_send < self._interval:
                return Delivery(sent=False)
            batch = self._spool.next_batch(
                CONTROL_PLANE_SINK,
                limit=MAX_OBSERVATION_BATCH_RECORDS,
                max_bytes=MAX_OBSERVATION_BATCH_BYTES,
            )
            if batch is None:
                self._last_send = now
                return Delivery(sent=False)
            accepted = self._send(batch)
            self._last_send = self._clock()
            self._spool.acknowledge(CONTROL_PLANE_SINK, accepted)
            return Delivery(sent=True, accepted_through_sequence=accepted)
        except Exception as error:  # noqa: BLE001 - an outage is not a supervision failure
            self._failures += 1
            self._last_send = self._clock()
            return Delivery(sent=False, detail=str(error))

    def refuse_current_batch(self) -> None:
        """Forget the in-flight batch after the control plane rejects it outright.

        A 409 means this identifier now names different content, so resending it cannot succeed.
        The records stay in the spool and the next batch is assembled fresh.
        """
        try:
            self._spool.abandon_inflight()
        except Exception:  # noqa: BLE001
            self._failures += 1

    def drain(self, *, passes: int = 8) -> None:
        """Try to deliver what is left, without letting a broken link hold up a job result."""
        for _ in range(passes):
            if not self._spool.pending(CONTROL_PLANE_SINK):
                return
            if not self.deliver(force=True).sent:
                return

    # --- What it has to report --------------------------------------------------------------------

    @property
    def result(self) -> dict[str, object] | None:
        return self._result

    def counters(self) -> dict[str, int]:
        """The drop counters, plus anything the spool had to discard to stay bounded.

        Returned empty when nothing was counted, because the result omits the field rather than
        sending an empty object.
        """
        counters = dict(self._collector.counters)
        try:
            discarded = sum(gap.count for gap in self._spool.gaps(CONTROL_PLANE_SINK))
        except Exception:  # noqa: BLE001
            discarded = 0
        if discarded:
            counters[Drop.BUDGET.value] = counters.get(Drop.BUDGET.value, 0) + discarded
        if self._failures:
            counters["dropped.delivery_failed"] = self._failures
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


def encoded_size(records: tuple[ObservationRecord, ...]) -> int:
    return len(json.dumps([record.model_dump(mode="json") for record in records]).encode("utf-8"))
