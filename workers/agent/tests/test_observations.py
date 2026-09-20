import json
from datetime import UTC, datetime

import pytest

from kratos_agent.observations import (
    LOG_BURST,
    MAX_FORWARDED_BYTES,
    MAX_LINE_BYTES,
    MAX_METRIC_NAMES,
    MAX_PARAM_NAMES,
    MAX_PARAM_VALUE_BYTES,
    MAX_STEP,
    MAX_UNIT_BYTES,
    RECORD_BURST,
    Drop,
    Kind,
    Observation,
    ObservationCollector,
    Stream,
)

AT = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)


class Clock:
    """A clock the test moves deliberately, so rate limits are exercised without sleeping."""

    def __init__(self) -> None:
        self.now = 1_000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture
def clock() -> Clock:
    return Clock()


@pytest.fixture
def collector(clock: Clock) -> ObservationCollector:
    return ObservationCollector(clock=clock)


def record(**fields: object) -> str:
    return json.dumps({"schema_version": "1.0", **fields}, separators=(",", ":"))


def emit(
    collector: ObservationCollector, line: str, stream: Stream = Stream.STDOUT
) -> Observation | None:
    return collector.observe(stream, AT, line)


def shown(
    collector: ObservationCollector, line: str, stream: Stream = Stream.STDOUT
) -> Observation:
    """Assert the line was forwarded, and return it, so a test can inspect what it became."""
    observation = emit(collector, line, stream)
    assert observation is not None
    return observation


# --- What a valid record becomes ------------------------------------------------------------


def test_param_metric_progress_and_result_are_recognised(collector: ObservationCollector) -> None:
    param = emit(collector, record(record="param", name="batch.size", value=64))
    assert param is not None and param.kind is Kind.PARAM
    assert param.name == "batch.size" and param.value == 64

    metric = emit(
        collector, record(record="metric", name="train.loss", value=0.5, step=1, unit="ratio")
    )
    assert metric is not None and metric.kind is Kind.METRIC
    assert metric.value == 0.5 and metric.step == 1 and metric.unit == "ratio"
    assert metric.metric_series is True

    progress = emit(collector, record(record="progress", step=3, total_steps=10, unit="steps"))
    assert progress is not None and progress.kind is Kind.PROGRESS
    assert progress.step == 3 and progress.total_steps == 10

    result = emit(collector, record(record="result", result={"status": "ok"}))
    assert result is not None and result.kind is Kind.RESULT
    assert result.result == {"status": "ok"}

    assert collector.counters == {}


# --- Rollout compatibility: images that predate the contract ---------------------------------


def test_bare_json_without_a_record_member_is_a_log_line(collector: ObservationCollector) -> None:
    """Every approved image today prints this. It must keep working and not be counted a fault."""
    observation = emit(collector, json.dumps({"status": "ok", "gpu": "RTX 5090"}))
    assert observation is not None and observation.kind is Kind.LOG
    assert collector.counters == {}


def test_a_line_without_a_record_member_is_not_required_to_carry_a_schema_version(
    collector: ObservationCollector,
) -> None:
    observation = emit(collector, json.dumps({"loss": 0.1}))
    assert observation is not None and observation.kind is Kind.LOG
    assert collector.counters == {}


def test_plain_text_and_stderr_are_log_lines(collector: ObservationCollector) -> None:
    assert shown(collector, "epoch 1 complete").kind is Kind.LOG
    # stderr is never parsed as a record, even when it looks exactly like one.
    line = record(record="metric", name="train.loss", value=0.5, step=1)
    assert shown(collector, line, Stream.STDERR).kind is Kind.LOG
    assert collector.counters == {}


# --- Malformed input is refused, and counted, and never raises -------------------------------


@pytest.mark.parametrize(
    "line",
    [
        '{"schema_version":"1.0","record":"metric","name":"train.loss","value":"x","step":1}',
        '{"schema_version":"1.0","record":"metric","name":"Train.Loss","value":1,"step":1}',
        '{"schema_version":"1.0","record":"metric","name":"train.loss","value":1,"step":-1}',
        '{"schema_version":"1.0","record":"metric","name":"train.loss","value":1,"step":1.5}',
        '{"schema_version":"1.0","record":"metric","name":"train.loss","value":1,"step":true}',
        '{"schema_version":"1.0","record":"unknown_kind"}',
        '{"schema_version":"1.0","record":"param","name":"a","value":{"nested":1}}',
        '{"schema_version":"1.0","record":"result","result":"not an object"}',
        '{"schema_version":"1.0","record":"result","result":{},"extra":1}',
        '{"schema_version":"1.0","record":"progress","step":5,"total_steps":4}',
        '{"schema_version":"1.0","record":"metric","name":"train.loss","value":1,"step":1,"x":2}',
    ],
)
def test_malformed_records_are_counted_and_shown_as_log_lines(
    collector: ObservationCollector, line: str
) -> None:
    """A broken record is counted, and still forwarded so its author can see it."""
    observation = emit(collector, line)
    assert observation is not None and observation.kind is Kind.LOG
    assert observation.text == line
    assert collector.counters[Drop.MALFORMED.value] == 1


@pytest.mark.parametrize("literal", ["NaN", "Infinity", "-Infinity"])
def test_non_finite_metric_values_are_rejected_not_coerced(
    collector: ObservationCollector, literal: str
) -> None:
    """Python's json accepts these non-standard literals, so the check must be explicit."""
    line = (
        '{"schema_version":"1.0","record":"metric","name":"train.loss",'
        f'"value":{literal},"step":1}}'
    )
    assert shown(collector, line).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1


def test_a_wrong_schema_version_is_not_parsed_further(collector: ObservationCollector) -> None:
    line = json.dumps(
        {"schema_version": "2.0", "record": "metric", "name": "train.loss", "value": 1, "step": 1}
    )
    assert shown(collector, line).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1


def test_a_boolean_is_not_a_measurement(collector: ObservationCollector) -> None:
    line = record(record="metric", name="train.loss", value=True, step=1)
    assert shown(collector, line).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1


def test_a_boolean_is_a_valid_parameter(collector: ObservationCollector) -> None:
    observation = emit(collector, record(record="param", name="amp.enabled", value=True))
    assert observation is not None and observation.value is True


def test_observe_never_raises_on_hostile_input(collector: ObservationCollector) -> None:
    hostile = [
        "{" * 2_000,
        '{"schema_version":"1.0","record":null}',
        '{"schema_version":"1.0","record":["metric"]}',
        "\x00\x01\x02",
        "\udcff",  # a lone surrogate, which naive encoding would raise on
        json.dumps({"schema_version": "1.0", "record": "param", "name": "a", "value": 1e400}),
    ]
    for line in hostile:
        collector.observe(Stream.STDOUT, AT, line)  # must not raise


# --- Bounds ----------------------------------------------------------------------------------


def test_an_oversize_line_is_refused_whole(collector: ObservationCollector) -> None:
    # The bound includes the newline, so a line of exactly MAX_LINE_BYTES is already too long.
    assert emit(collector, "x" * MAX_LINE_BYTES) is None
    assert collector.counters[Drop.OVERSIZE.value] == 1
    assert emit(collector, "x" * (MAX_LINE_BYTES - 1)) is not None


def test_the_line_bound_counts_bytes_not_characters(collector: ObservationCollector) -> None:
    # A three-byte character must not be counted as one.
    assert emit(collector, "中" * (MAX_LINE_BYTES // 3 + 1)) is None
    assert collector.counters[Drop.OVERSIZE.value] == 1


def test_an_over_long_parameter_value_is_refused(collector: ObservationCollector) -> None:
    line = record(record="param", name="a", value="v" * (MAX_PARAM_VALUE_BYTES + 1))
    assert shown(collector, line).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1


def test_an_over_long_unit_is_refused(collector: ObservationCollector) -> None:
    line = record(
        record="metric", name="train.loss", value=1, step=1, unit="u" * (MAX_UNIT_BYTES + 1)
    )
    assert shown(collector, line).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1


def test_a_step_at_the_precision_limit_is_refused(collector: ObservationCollector) -> None:
    assert shown(collector, record(record="progress", step=MAX_STEP)).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1
    assert emit(collector, record(record="progress", step=MAX_STEP - 1)) is not None


def test_a_step_may_repeat_but_not_go_backwards(collector: ObservationCollector) -> None:
    name = "train.loss"
    assert emit(collector, record(record="metric", name=name, value=1, step=5)) is not None
    assert emit(collector, record(record="metric", name=name, value=2, step=5)) is not None
    assert shown(collector, record(record="metric", name=name, value=3, step=4)).kind is Kind.LOG
    assert collector.counters[Drop.MALFORMED.value] == 1


def test_steps_are_tracked_per_name(collector: ObservationCollector) -> None:
    """A low step under one name must not be refused because another name is further ahead."""
    assert emit(collector, record(record="metric", name="train.loss", value=1, step=99)) is not None
    other = record(record="metric", name="validation.loss", value=1, step=1)
    assert emit(collector, other) is not None
    assert collector.counters == {}


# --- Cardinality -------------------------------------------------------------------------------


def test_a_metric_name_outside_the_allowlist_is_kept_off_the_metric_path(
    collector: ObservationCollector,
) -> None:
    """It still reaches MLflow, which is per-run, but must never become a fleet-wide series."""
    observation = emit(collector, record(record="metric", name="custom.thing", value=1, step=1))
    assert observation is not None and observation.kind is Kind.METRIC
    assert observation.metric_series is False
    assert collector.counters[Drop.METRIC_NAME_NOT_ALLOWED.value] == 1


def test_distinct_metric_and_parameter_names_are_capped(
    collector: ObservationCollector, clock: Clock
) -> None:
    for index in range(MAX_METRIC_NAMES):
        line = record(record="metric", name=f"m.{index}", value=1, step=1)
        assert emit(collector, line) is not None
    clock.advance(60)
    assert emit(collector, record(record="metric", name="m.overflow", value=1, step=1)) is None
    assert collector.counters[Drop.METRIC_NAME_LIMIT.value] == 1

    # A name already seen is still accepted once the cap is reached.
    assert emit(collector, record(record="metric", name="m.0", value=2, step=2)) is not None

    for index in range(MAX_PARAM_NAMES):
        clock.advance(0.1)
        assert emit(collector, record(record="param", name=f"p.{index}", value=1)) is not None
    clock.advance(60)
    assert emit(collector, record(record="param", name="p.overflow", value=1)) is None
    assert collector.counters[Drop.PARAM_NAME_LIMIT.value] == 1


# --- Rate and byte budget ----------------------------------------------------------------------


def test_records_are_rate_limited_with_a_burst(
    collector: ObservationCollector, clock: Clock
) -> None:
    line = record(record="progress", step=0)
    for _ in range(RECORD_BURST):
        assert emit(collector, line) is not None
    assert emit(collector, line) is None
    assert collector.counters[Drop.RECORD_RATE.value] == 1

    # The bucket refills at the sustained rate: half a second buys ten records.
    clock.advance(0.5)
    for _ in range(10):
        assert emit(collector, line) is not None
    assert emit(collector, line) is None


def test_log_lines_have_their_own_budget(collector: ObservationCollector, clock: Clock) -> None:
    """A flood of logs must not consume the allowance a record would need."""
    for _ in range(LOG_BURST):
        assert emit(collector, "noise") is not None
    assert emit(collector, "noise") is None
    assert collector.counters[Drop.LOG_RATE.value] == 1
    assert emit(collector, record(record="progress", step=1)) is not None


def test_the_byte_budget_is_enforced_and_stops_forwarding(
    collector: ObservationCollector, clock: Clock
) -> None:
    collector.forwarded_bytes = MAX_FORWARDED_BYTES - 10
    assert emit(collector, "x" * 40) is None
    assert collector.counters[Drop.BYTE_BUDGET.value] == 1
    # Something that still fits is still taken, so the budget bounds bytes rather than ending
    # collection at the first refusal.
    assert emit(collector, "x" * 5) is not None


def test_a_refused_line_does_not_consume_the_byte_budget(
    collector: ObservationCollector,
) -> None:
    before = collector.forwarded_bytes
    emit(collector, "x" * MAX_LINE_BYTES)
    assert collector.forwarded_bytes == before


# --- Counters are evidence ---------------------------------------------------------------------


def test_counters_accumulate_by_reason(collector: ObservationCollector) -> None:
    emit(collector, "x" * MAX_LINE_BYTES)
    emit(collector, "y" * MAX_LINE_BYTES)
    assert shown(collector, '{"schema_version":"1.0","record":"nope"}').kind is Kind.LOG
    assert collector.counters == {Drop.OVERSIZE.value: 2, Drop.MALFORMED.value: 1}


# --- The first real workload -------------------------------------------------------------------
#
# DanielBryars/Kratos.SmolVLA at 0ec53cc is the first workload Kratos will run for its own sake
# rather than to prove the machinery. Codex named it the agent-side acceptance target, so these
# are the exact record shapes it emits, taken from its `emit()` calls. If this test starts
# failing, a real run stops being observable, which is a more serious thing than a unit test.


def smolvla_line(record_type: str, **fields: object) -> str:
    """Exactly how the workload encodes a record: default separators, not compact ones."""
    return json.dumps({"schema_version": "1.0", "record": record_type, **fields})


def test_the_smolvla_workload_stream_is_accepted_whole(
    collector: ObservationCollector, clock: Clock
) -> None:
    params = {
        "model.id": "lerobot/smolvla_base",
        "model.revision": "a" * 40,
        "dataset.id": "lerobot/svla_so100_pickplace",
        "dataset.revision": "b" * 40,
        "training.steps": 2000,
        "training.batch_size": 8,
    }
    for name, value in params.items():
        observation = shown(collector, smolvla_line("param", name=name, value=value))
        assert observation.kind is Kind.PARAM
        assert observation.name == name and observation.value == value

    for step in range(1, 5):
        clock.advance(1)
        progress = shown(
            collector, smolvla_line("progress", step=step, total_steps=2000, unit="steps")
        )
        assert progress.kind is Kind.PROGRESS and progress.total_steps == 2000

        metric = shown(
            collector, smolvla_line("metric", name="train.loss", value=1.0 / step, step=step)
        )
        assert metric.kind is Kind.METRIC
        # train.loss is on the allowlist, so it is one of the few names that may become a series.
        assert metric.metric_series is True

    summary = {
        "model": {"id": "lerobot/smolvla_base", "revision": "a" * 40},
        "dataset": {"id": "lerobot/svla_so100_pickplace", "revision": "b" * 40},
        "steps": 2000,
        "batch_size": 8,
        "checkpoint": "smolvla-checkpoint.tar",
    }
    result = shown(collector, smolvla_line("result", result=summary))
    assert result.kind is Kind.RESULT and result.result == summary

    # Nothing in a correct run is refused.
    assert collector.counters == {}


def test_the_workloads_training_output_on_stderr_stays_log_lines(
    collector: ObservationCollector,
) -> None:
    """It forwards lerobot's own output to stderr, which must never be parsed as records."""
    for line in ("INFO 2026-09-20 step 1 loss 0.42", '{"looks":"like json"}'):
        assert shown(collector, line, Stream.STDERR).kind is Kind.LOG
    assert collector.counters == {}
