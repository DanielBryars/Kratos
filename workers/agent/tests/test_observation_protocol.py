"""The wire shapes for observation streaming, as agreed with Codex for protocol 1.2.

These are contract tests. The classifier has already applied ADR-015's rules to a workload's
line, so validating again in the model looks redundant — it is not. This model is what leaves the
worker, and it should not be possible to assemble an invalid batch out of valid code.
"""

import json
from datetime import UTC, datetime
from uuid import UUID

import pytest
from pydantic import ValidationError

from kratos_agent.models import (
    MAX_OBSERVATION_BATCH_RECORDS,
    PROTOCOL_VERSION,
    JobAssignment,
    JobExecutionResult,
    ObservationBatchResponse,
    ObservationRecord,
    SubmitObservationBatchRequest,
)

AT = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)
STREAM_ID = UUID("55555555-5555-4555-8555-555555555555")
BATCH_ID = UUID("66666666-6666-4666-8666-666666666666")


def progress(sequence: int, step: int = 0) -> ObservationRecord:
    return ObservationRecord(sequence=sequence, at=AT, record="progress", step=step)


# --- The assignment ---------------------------------------------------------------------------


def assignment(**overrides: object) -> dict[str, object]:
    fields: dict[str, object] = {
        "attempt_id": "33333333-3333-4333-8333-333333333333",
        "job_id": "44444444-4444-4444-8444-444444444444",
        "name": "job",
        "image_reference": "example.com/image@sha256:" + "a" * 64,
        "gpu_index": 0,
        "timeout_seconds": 60,
        "lease_expires_at": AT.isoformat(),
    }
    fields.update(overrides)
    return fields


def test_an_assignment_without_a_stream_is_still_valid() -> None:
    """The server omits the field for agents below 1.2, and during its own rollout."""
    assert JobAssignment.model_validate(assignment()).observation_stream_id is None


def test_an_assignment_carrying_a_stream_is_accepted() -> None:
    """The field must exist here before the server ever sends it.

    JobAssignment forbids unknown fields, so an agent that did not know `observation_stream_id`
    would reject the whole assignment and refuse to run the job. This test is the guard on that
    ordering, which is why it matters more than it looks.
    """
    parsed = JobAssignment.model_validate(assignment(observation_stream_id=str(STREAM_ID)))
    assert parsed.observation_stream_id == STREAM_ID


# --- Records ----------------------------------------------------------------------------------


def test_each_kind_carries_exactly_what_it_needs() -> None:
    ObservationRecord(sequence=1, at=AT, record="param", name="model.id", value="smolvla")
    ObservationRecord(sequence=2, at=AT, record="metric", name="train.loss", value=0.5, step=1)
    ObservationRecord(sequence=3, at=AT, record="progress", step=1, total_steps=10)


@pytest.mark.parametrize(
    "fields",
    [
        {"record": "param", "name": "a"},  # no value
        {"record": "param", "value": 1},  # no name
        {"record": "param", "name": "a", "value": 1, "step": 1},  # a parameter has no position
        {"record": "metric", "name": "a", "value": 1},  # no step
        {"record": "metric", "name": "a", "step": 1},  # no value
        {"record": "metric", "name": "a", "value": "text", "step": 1},  # not a number
        {"record": "metric", "name": "a", "value": True, "step": 1},  # a boolean is not a number
        {"record": "metric", "name": "a", "value": 1, "step": 1, "total_steps": 2},
        {"record": "progress"},  # no step
        {"record": "progress", "step": 1, "name": "a"},  # progress has no name
        {"record": "progress", "step": 5, "total_steps": 4},  # beyond its own total
        {"record": "log", "step": 1},  # log lines do not go to this sink
    ],
)
def test_a_record_that_does_not_fit_its_kind_is_refused(fields: dict[str, object]) -> None:
    with pytest.raises(ValidationError):
        ObservationRecord(sequence=1, at=AT, **fields)  # type: ignore[arg-type]


def test_a_sequence_starts_at_one() -> None:
    with pytest.raises(ValidationError):
        ObservationRecord(sequence=0, at=AT, record="progress", step=0)


# --- Batches ----------------------------------------------------------------------------------


def test_a_batch_is_contiguous_from_its_first_sequence() -> None:
    request = SubmitObservationBatchRequest(
        protocol_version=PROTOCOL_VERSION,
        first_sequence=7,
        records=(progress(7), progress(8), progress(9)),
    )
    assert len(request.records) == 3


@pytest.mark.parametrize(
    "records, first",
    [
        ((1, 3), 1),  # a gap
        ((1, 2), 2),  # does not start where it says
        ((2, 1), 2),  # out of order
        ((1, 1), 1),  # repeated
    ],
)
def test_a_batch_with_a_gap_or_a_repeat_is_refused(records: tuple[int, ...], first: int) -> None:
    """A gap makes `accepted_through_sequence` ambiguous, and that is the acknowledgement."""
    with pytest.raises(ValidationError):
        SubmitObservationBatchRequest(
            protocol_version=PROTOCOL_VERSION,
            first_sequence=first,
            records=tuple(progress(sequence) for sequence in records),
        )


def test_a_batch_is_bounded_and_never_empty() -> None:
    with pytest.raises(ValidationError):
        SubmitObservationBatchRequest(
            protocol_version=PROTOCOL_VERSION, first_sequence=1, records=()
        )
    too_many = tuple(progress(n) for n in range(1, MAX_OBSERVATION_BATCH_RECORDS + 2))
    with pytest.raises(ValidationError):
        SubmitObservationBatchRequest(
            protocol_version=PROTOCOL_VERSION, first_sequence=1, records=too_many
        )


def test_observations_require_protocol_1_2_or_newer() -> None:
    for version in ("1.0", "1.1"):
        with pytest.raises(ValidationError):
            SubmitObservationBatchRequest(
                protocol_version=version, first_sequence=1, records=(progress(1),)
            )
    SubmitObservationBatchRequest(protocol_version="1.10", first_sequence=1, records=(progress(1),))


def test_the_acknowledgement_names_the_stream_and_batch_it_answers() -> None:
    response = ObservationBatchResponse.model_validate(
        {
            "stream_id": str(STREAM_ID),
            "batch_id": str(BATCH_ID),
            "accepted_through_sequence": 12,
        }
    )
    assert response.stream_id == STREAM_ID and response.accepted_through_sequence == 12


# --- Counters on the result ---------------------------------------------------------------------


def result(**overrides: object) -> JobExecutionResult:
    fields: dict[str, object] = {"exit_code": 0, "timed_out": False, "stdout": "", "stderr": ""}
    fields.update(overrides)
    return JobExecutionResult.model_validate(fields)


def test_a_result_without_counters_is_unchanged() -> None:
    """A 1.1 result must keep working, so the field is absent rather than an empty object."""
    assert result().observation_counters is None
    assert "observation_counters" not in result().model_dump(exclude_none=True)


def test_counters_are_carried_as_given() -> None:
    counters = {"dropped.rate": 4, "not_exported.otlp_unconfigured": 91}
    assert result(observation_counters=counters).observation_counters == counters


def test_an_empty_counter_object_is_refused() -> None:
    """Nothing counted means the field is omitted; an empty object would be a third state."""
    with pytest.raises(ValidationError):
        result(observation_counters={})


@pytest.mark.parametrize(
    "counters",
    [
        {"dropped.rate": -1},
        {"dropped.rate": 1.5},
        {"dropped.rate": True},
        {"unknown_namespace.thing": 1},
        {"dropped.Rate": 1},
        {"dropped.": 1},
    ],
)
def test_counter_names_and_values_are_constrained(counters: dict[str, object]) -> None:
    with pytest.raises(ValidationError):
        result(observation_counters=counters)


def test_the_counter_object_survives_a_round_trip() -> None:
    """Codex persists this object unchanged, so it has to serialise as given."""
    counters = {"dropped.malformed": 2, "not_exported.metric_name_not_allowed": 7}
    encoded = result(observation_counters=counters).model_dump_json()
    assert json.loads(encoded)["observation_counters"] == counters


# --- The cross-half contract ---------------------------------------------------------------------


def test_the_agent_can_only_emit_counters_the_control_plane_accepts() -> None:
    """Pinned against the merged control plane, because a mismatch rejects the job result.

    `registry.rs` holds a nine-name allowlist and answers anything outside it with
    `invalid_request`, which fails the whole result submission rather than just dropping the
    counter. A new counter added here without a matching change there would therefore turn a minor
    telemetry event into a job whose result the control plane refuses.

    If this test fails, the fix is not to change the expected set: it is to add the name to
    `OBSERVATION_COUNTERS` in the control plane first, and only then here.
    """
    from kratos_agent.observations import Drop
    from kratos_agent.pump import ABANDONED_COUNTER, FAILURE_COUNTER

    emittable = {reason.value for reason in Drop} | {FAILURE_COUNTER, ABANDONED_COUNTER}
    accepted_by_the_control_plane = {
        "dropped.oversize",
        "dropped.malformed",
        "dropped.rate",
        "dropped.budget",
        "dropped.name_limit",
        "dropped.delivery_abandoned",
        "not_exported.metric_name_not_allowed",
        "not_exported.otlp_unconfigured",
        "delivery.failures",
    }
    assert emittable == accepted_by_the_control_plane


def test_every_accepted_counter_name_passes_the_models_own_rule() -> None:
    """The model's rule is broader than the allowlist, so it must at least admit all of it."""
    for name in (
        "dropped.oversize",
        "dropped.delivery_abandoned",
        "not_exported.metric_name_not_allowed",
        "delivery.failures",
    ):
        assert result(observation_counters={name: 1}).observation_counters == {name: 1}
