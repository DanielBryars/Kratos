"""Parse and bound the Kratos records a job writes to its standard output.

ADR-015 gives a job container no network, no credential and no telemetry client: a workload's only
telemetry channel is a line of JSON on stdout, which the trusted agent reads through the Docker
Engine API it already holds. This module is that reader's judgement, and nothing else. It decides
what a line is, applies every limit in the decision's v1 profile, and counts what it refused.

Two properties matter more than anything this module does with a valid record.

**It is hostile input.** A workload can print anything, at any rate, including JSON crafted to be
awkward. Every function here is total: it returns a classification, and it never raises. A parse
failure is a log line, not an exception, because an exception would reach supervision.

**It never blocks.** Every limit discards rather than waits. A workload that floods stdout loses
observations, which is diagnostic data; it does not gain the ability to stall the agent that is
enforcing its deadline. What was discarded is counted by reason and reported with the attempt, so a
gap in a chart is a number an operator can read rather than silence they have to infer.
"""

import json
import math
import re
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Any, TypeGuard

# The line bound includes the newline, so the JSON itself has one byte less.
MAX_LINE_BYTES = 8 * 1024
MAX_PARAM_VALUE_BYTES = 512
MAX_UNIT_BYTES = 32
MAX_STEP = 2**53
MAX_METRIC_NAMES = 100
MAX_PARAM_NAMES = 200
MAX_FORWARDED_BYTES = 64 * 1024 * 1024

RECORD_RATE_PER_SECOND = 20.0
RECORD_BURST = 100
LOG_RATE_PER_SECOND = 200.0
LOG_BURST = 1000

SCHEMA_VERSION = "1.0"
NAME_PATTERN = re.compile(r"^[a-z][a-z0-9_.]{0,63}$")

# Only these become OpenTelemetry metric series. A name outside the list still reaches MLflow
# through the control plane, which is a per-run store and so is not bounded by cardinality; it is
# kept off the metric path because one new series per workload name is unbounded across a fleet.
# Extending this list is a deployment configuration change, never a workload's decision.
METRIC_NAME_ALLOWLIST = frozenset(
    {
        "train.loss",
        "train.accuracy",
        "validation.loss",
        "validation.accuracy",
        "throughput.steps_per_second",
        "gpu.memory.peak_bytes",
    }
)


class Stream(Enum):
    STDOUT = "stdout"
    STDERR = "stderr"


class Kind(Enum):
    """What a line turned out to be. `LOG` covers everything that is not a valid record."""

    PARAM = "param"
    METRIC = "metric"
    PROGRESS = "progress"
    RESULT = "result"
    LOG = "log"


class Drop(Enum):
    """Why a line was not delivered, in the names the control plane persists unchanged.

    Two namespaces, and the difference is the point. `dropped.` means the agent refused the line
    and nobody will ever see it. `not_exported.` means the line was kept and delivered, but one
    sink did not take it, which is not a loss and should not read as one on a run view.

    The names are deliberately coarser than the checks that produce them: an operator asking
    "what did I lose" is served by `dropped.rate`, and which bucket ran out is a detail of this
    module rather than execution evidence.
    """

    OVERSIZE = "dropped.oversize"
    MALFORMED = "dropped.malformed"
    RATE = "dropped.rate"
    BUDGET = "dropped.budget"
    NAME_LIMIT = "dropped.name_limit"
    # Kept and delivered to the control plane, and so to MLflow, but never made an OTLP series.
    METRIC_NAME_NOT_ALLOWED = "not_exported.metric_name_not_allowed"
    # A log line with no collector configured for this attempt. ADR-015 would have it wait behind
    # an OTLP cursor; with no OTLP sink that cursor is fictional, so the line is counted here
    # instead. It is not a delivery gap, because no sink was ever configured to take it.
    OTLP_UNCONFIGURED = "not_exported.otlp_unconfigured"


@dataclass(frozen=True)
class Observation:
    """One classified line, ready to be sequenced and spooled."""

    kind: Kind
    stream: Stream
    at: datetime
    # The original line, kept for log lines and for a record that must be forwarded as one.
    text: str
    name: str | None = None
    value: Any = None
    step: int | None = None
    total_steps: int | None = None
    unit: str | None = None
    result: dict[str, Any] | None = None
    # False for a metric whose name is not on the allowlist: it goes to the control plane, and so
    # to MLflow, but never becomes an OpenTelemetry series.
    metric_series: bool = True


@dataclass
class _Bucket:
    """A token bucket. Sustained rate with a burst, and it discards rather than waiting."""

    rate: float
    capacity: int
    tokens: float
    updated: float

    def take(self, now: float) -> bool:
        elapsed = max(0.0, now - self.updated)
        self.updated = now
        self.tokens = min(float(self.capacity), self.tokens + elapsed * self.rate)
        if self.tokens < 1.0:
            return False
        self.tokens -= 1.0
        return True


def _byte_length(text: str) -> int:
    return len(text.encode("utf-8", errors="surrogatepass"))


def _is_finite_number(value: Any) -> TypeGuard[float]:
    # bool is an int in Python, and a boolean is not a measurement.
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    return math.isfinite(value)


def _is_step(value: Any) -> TypeGuard[int]:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value < MAX_STEP


def _valid_name(value: Any) -> TypeGuard[str]:
    return isinstance(value, str) and NAME_PATTERN.match(value) is not None


def _valid_unit(value: Any) -> TypeGuard[str]:
    return isinstance(value, str) and _byte_length(value) <= MAX_UNIT_BYTES


def _valid_param_value(value: Any) -> bool:
    if not isinstance(value, (bool, str)) and not _is_finite_number(value):
        return False
    try:
        encoded = json.dumps(value, separators=(",", ":"))
    except (TypeError, ValueError):
        return False
    return _byte_length(encoded) <= MAX_PARAM_VALUE_BYTES


@dataclass
class ObservationCollector:
    """Classifies an attempt's output and holds the per-attempt limits ADR-015 sets.

    One instance belongs to one attempt. It is not thread-safe by itself; the reader that owns it
    is expected to be the only caller, which is how the agent uses it.
    """

    clock: Callable[[], float]
    counters: dict[str, int] = field(default_factory=dict)
    forwarded_bytes: int = 0
    _metric_names: set[str] = field(default_factory=set)
    _param_names: set[str] = field(default_factory=set)
    _last_step: dict[str, int] = field(default_factory=dict)
    _records: _Bucket = field(init=False)
    _logs: _Bucket = field(init=False)

    def __post_init__(self) -> None:
        now = self.clock()
        self._records = _Bucket(RECORD_RATE_PER_SECOND, RECORD_BURST, float(RECORD_BURST), now)
        self._logs = _Bucket(LOG_RATE_PER_SECOND, LOG_BURST, float(LOG_BURST), now)

    def _count(self, reason: Drop) -> None:
        self.counters[reason.value] = self.counters.get(reason.value, 0) + 1

    def observe(self, stream: Stream, at: datetime, line: str) -> Observation | None:
        """Classify one line. Returns None when nothing should be forwarded.

        Never raises. A caller may hand this anything a container printed.
        """
        try:
            return self._observe(stream, at, line)
        except Exception:  # noqa: BLE001 - telemetry must never raise into supervision
            self._count(Drop.MALFORMED)
            return None

    def _observe(self, stream: Stream, at: datetime, line: str) -> Observation | None:
        size = _byte_length(line) + 1  # the newline the workload wrote
        if size > MAX_LINE_BYTES:
            # Oversize is refused whole. Truncating would invent a line the workload never wrote,
            # and a truncated JSON object parses as nothing useful anyway.
            self._count(Drop.OVERSIZE)
            return None

        record = self._parse_record(stream, line)
        if record is not None:
            if not self._records.take(self.clock()):
                self._count(Drop.RATE)
                return None
            built = self._build(record, stream, at, line)
            if isinstance(built, Observation):
                return built if self._charge(size) else None
            self._count(built)
            if built is not Drop.MALFORMED:
                # A deliberate discard: a limit was reached, and the line is not shown again as a
                # log line because that would defeat the limit it just hit.
                return None
            # A line that claimed to be a record and was not one is still worth showing. It is
            # counted, and forwarded as a log line at most, which is how an author sees the
            # mistake rather than watching their output disappear.

        return self._as_log(stream, at, line, size)

    def _parse_record(self, stream: Stream, line: str) -> dict[str, Any] | None:
        """Return the record object, or None when this line is not a Kratos record.

        Every stderr line is a log line by decision, so stderr is never parsed. An image that
        predates this contract prints bare JSON with no `record` member; that is a log line too,
        and it is deliberately not required to carry a `schema_version` it has never heard of.
        """
        if stream is Stream.STDERR:
            return None
        stripped = line.strip()
        if not stripped.startswith("{"):
            return None
        try:
            parsed = json.loads(stripped)
        except (ValueError, RecursionError):
            return None
        if not isinstance(parsed, dict) or "record" not in parsed:
            return None
        if parsed.get("schema_version") != SCHEMA_VERSION:
            # It claims to be a record but not one this agent knows how to read. Counted here and
            # returned as not-a-record, so it is forwarded as a log line at most and never parsed
            # further. A future schema version is a line an operator should still be able to read.
            self._count(Drop.MALFORMED)
            return None
        return parsed

    def _as_log(self, stream: Stream, at: datetime, line: str, size: int) -> Observation | None:
        if not self._logs.take(self.clock()):
            self._count(Drop.RATE)
            return None
        if not self._charge(size):
            return None
        return Observation(kind=Kind.LOG, stream=stream, at=at, text=line)

    def _charge(self, size: int) -> bool:
        if self.forwarded_bytes + size > MAX_FORWARDED_BYTES:
            self._count(Drop.BUDGET)
            return False
        self.forwarded_bytes += size
        return True

    def _build(
        self, record: dict[str, Any], stream: Stream, at: datetime, line: str
    ) -> "Observation | Drop":
        kind = record.get("record")
        if kind == "param":
            return self._param(record, stream, at, line)
        if kind == "metric":
            return self._metric(record, stream, at, line)
        if kind == "progress":
            return self._progress(record, stream, at, line)
        if kind == "result":
            return self._result(record, stream, at, line)
        return Drop.MALFORMED

    def _param(
        self, record: dict[str, Any], stream: Stream, at: datetime, line: str
    ) -> "Observation | Drop":
        if set(record) - {"schema_version", "record", "name", "value"}:
            return Drop.MALFORMED
        name, value = record.get("name"), record.get("value")
        if not _valid_name(name):
            return Drop.MALFORMED
        if not _valid_param_value(value):
            return Drop.MALFORMED
        if name not in self._param_names and len(self._param_names) >= MAX_PARAM_NAMES:
            return Drop.NAME_LIMIT
        self._param_names.add(name)
        return Observation(kind=Kind.PARAM, stream=stream, at=at, text=line, name=name, value=value)

    def _metric(
        self, record: dict[str, Any], stream: Stream, at: datetime, line: str
    ) -> "Observation | Drop":
        if set(record) - {"schema_version", "record", "name", "value", "step", "unit"}:
            return Drop.MALFORMED
        name, value, step = record.get("name"), record.get("value"), record.get("step")
        unit = record.get("unit")
        if not _valid_name(name):
            return Drop.MALFORMED
        if not _is_finite_number(value):
            return Drop.MALFORMED
        if not _is_step(step):
            return Drop.MALFORMED
        if unit is not None and not _valid_unit(unit):
            return Drop.MALFORMED
        if step < self._last_step.get(name, -1):
            # A step that goes backwards within a name is not a later observation of the same
            # series, and accepting it would make the series unorderable.
            return Drop.MALFORMED
        if name not in self._metric_names and len(self._metric_names) >= MAX_METRIC_NAMES:
            return Drop.NAME_LIMIT
        allowed = name in METRIC_NAME_ALLOWLIST
        if not allowed:
            self._count(Drop.METRIC_NAME_NOT_ALLOWED)
        self._metric_names.add(name)
        self._last_step[name] = step
        return Observation(
            kind=Kind.METRIC,
            stream=stream,
            at=at,
            text=line,
            name=name,
            value=value,
            step=step,
            unit=unit,
            metric_series=allowed,
        )

    def _progress(
        self, record: dict[str, Any], stream: Stream, at: datetime, line: str
    ) -> "Observation | Drop":
        if set(record) - {"schema_version", "record", "step", "total_steps", "unit"}:
            return Drop.MALFORMED
        step, total, unit = record.get("step"), record.get("total_steps"), record.get("unit")
        if not _is_step(step):
            return Drop.MALFORMED
        if total is not None:
            if not _is_step(total):
                return Drop.MALFORMED
            if total < step:
                return Drop.MALFORMED
        if unit is not None and not _valid_unit(unit):
            return Drop.MALFORMED
        # Progress carries no name, so its monotonicity is over the attempt's single position.
        if step < self._last_step.get("", -1):
            return Drop.MALFORMED
        self._last_step[""] = step
        return Observation(
            kind=Kind.PROGRESS,
            stream=stream,
            at=at,
            text=line,
            step=step,
            total_steps=total,
            unit=unit,
        )

    def _result(
        self, record: dict[str, Any], stream: Stream, at: datetime, line: str
    ) -> "Observation | Drop":
        if set(record) != {"schema_version", "record", "result"}:
            return Drop.MALFORMED
        result = record.get("result")
        if not isinstance(result, dict):
            return Drop.MALFORMED
        return Observation(kind=Kind.RESULT, stream=stream, at=at, text=line, result=result)
