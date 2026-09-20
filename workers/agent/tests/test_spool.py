"""The spool's job is to still be right after a crash, so most of these tests kill it and reopen.

Everything runs as a real directory on disk rather than against a fake, because the properties
being tested — a torn append, a cursor that survives, a batch identity that does not change across
a restart — only exist at the filesystem.
"""

import json
from pathlib import Path
from typing import Any

import pytest

from kratos_agent.spool import (
    MAX_SPOOL_BYTES,
    Batch,
    ObservationSpool,
    SpoolError,
)

CONTROL_PLANE = "control_plane"
OTLP = "otlp"


@pytest.fixture
def root(tmp_path: Path) -> Path:
    return tmp_path / "attempt"


def spool(root: Path, *sinks: str) -> ObservationSpool:
    return ObservationSpool(root, sinks or (CONTROL_PLANE,))


def record(step: int) -> dict[str, Any]:
    return {"record": "progress", "step": step, "at": "2026-09-20T12:00:00Z"}


def fill(store: ObservationSpool, count: int, first_step: int = 0) -> None:
    for step in range(first_step, first_step + count):
        store.append(record(step))


# --- Sequencing ----------------------------------------------------------------------------


def test_sequences_start_at_one_and_increase(root: Path) -> None:
    store = spool(root)
    assert [store.append(record(step)) for step in range(3)] == [1, 2, 3]
    assert store.last_sequence == 3


def test_a_spool_needs_a_sink(root: Path) -> None:
    """With no sink there is nobody to retain a record for, so retaining one is meaningless."""
    with pytest.raises(SpoolError):
        ObservationSpool(root, ())


# --- Batching ------------------------------------------------------------------------------


def test_a_batch_is_contiguous_and_bounded_by_count(root: Path) -> None:
    store = spool(root)
    fill(store, 10)
    batch = store.next_batch(CONTROL_PLANE, limit=4, max_bytes=MAX_SPOOL_BYTES)
    assert batch is not None
    assert batch.first_sequence == 1 and len(batch.records) == 4
    assert [r["sequence"] for r in batch.records] == [1, 2, 3, 4]
    assert batch.last_sequence == 4


def test_a_batch_is_bounded_by_bytes_but_never_empty(root: Path) -> None:
    store = spool(root)
    fill(store, 5)
    # A budget smaller than one record still yields one, because a batch of nothing makes no
    # progress and the spool would never drain.
    batch = store.next_batch(CONTROL_PLANE, limit=100, max_bytes=1)
    assert batch is not None and len(batch.records) == 1


def test_nothing_to_send_is_nothing(root: Path) -> None:
    store = spool(root)
    assert store.next_batch(CONTROL_PLANE, limit=10, max_bytes=MAX_SPOOL_BYTES) is None
    assert not store.pending(CONTROL_PLANE)


def test_an_unknown_sink_is_refused(root: Path) -> None:
    store = spool(root)
    with pytest.raises(SpoolError):
        store.next_batch("nowhere", limit=1, max_bytes=MAX_SPOOL_BYTES)
    with pytest.raises(SpoolError):
        store.acknowledge("nowhere", 1)


# --- Acknowledgement -------------------------------------------------------------------------


def test_acknowledgement_advances_the_cursor_and_the_next_batch_follows_it(root: Path) -> None:
    store = spool(root)
    fill(store, 6)
    first = store.next_batch(CONTROL_PLANE, limit=3, max_bytes=MAX_SPOOL_BYTES)
    assert first is not None
    store.acknowledge(CONTROL_PLANE, first.last_sequence)

    second = store.next_batch(CONTROL_PLANE, limit=3, max_bytes=MAX_SPOOL_BYTES)
    assert second is not None and second.first_sequence == 4
    assert second.batch_id != first.batch_id


def test_a_cursor_never_goes_backwards(root: Path) -> None:
    """A late acknowledgement for an earlier batch must not undo progress already recorded."""
    store = spool(root)
    fill(store, 5)
    store.acknowledge(CONTROL_PLANE, 4)
    store.acknowledge(CONTROL_PLANE, 2)
    assert store.cursor(CONTROL_PLANE) == 4


# --- Two sinks, two cursors ------------------------------------------------------------------


def test_each_sink_advances_independently(root: Path) -> None:
    store = spool(root, CONTROL_PLANE, OTLP)
    fill(store, 4)
    store.acknowledge(CONTROL_PLANE, 4)

    assert not store.pending(CONTROL_PLANE)
    assert store.pending(OTLP)
    # The records are retained for the slower sink, which is the whole reason for two cursors.
    batch = store.next_batch(OTLP, limit=10, max_bytes=MAX_SPOOL_BYTES)
    assert batch is not None and batch.first_sequence == 1


def test_records_survive_until_every_sink_has_them(root: Path) -> None:
    store = spool(root, CONTROL_PLANE, OTLP)
    fill(store, 3)
    store.acknowledge(CONTROL_PLANE, 3)
    store.acknowledge(OTLP, 3)
    assert not store.pending(CONTROL_PLANE) and not store.pending(OTLP)


# --- Durability ------------------------------------------------------------------------------


def test_a_reopened_spool_resumes_where_it_stopped(root: Path) -> None:
    store = spool(root)
    fill(store, 5)
    store.acknowledge(CONTROL_PLANE, 2)

    reopened = spool(root)
    assert reopened.last_sequence == 5
    assert reopened.cursor(CONTROL_PLANE) == 2
    batch = reopened.next_batch(CONTROL_PLANE, limit=10, max_bytes=MAX_SPOOL_BYTES)
    assert batch is not None and batch.first_sequence == 3


def test_an_inflight_batch_is_resent_with_the_same_identity(root: Path) -> None:
    """The point of writing the batch down: a restart must replay, not invent a new batch.

    Rebuilding from the records after a crash would produce the same sequences under a new batch
    id, which the control plane answers with a 409 rather than the idempotent acknowledgement.
    """
    store = spool(root)
    fill(store, 4)
    sent = store.next_batch(CONTROL_PLANE, limit=2, max_bytes=MAX_SPOOL_BYTES)
    assert sent is not None

    reopened = spool(root)
    resent = reopened.next_batch(CONTROL_PLANE, limit=2, max_bytes=MAX_SPOOL_BYTES)
    assert resent is not None
    assert resent.batch_id == sent.batch_id
    assert resent.first_sequence == sent.first_sequence
    assert [r["sequence"] for r in resent.records] == [r["sequence"] for r in sent.records]


def test_an_abandoned_batch_is_rebuilt_fresh(root: Path) -> None:
    store = spool(root)
    fill(store, 4)
    refused = store.next_batch(CONTROL_PLANE, limit=2, max_bytes=MAX_SPOOL_BYTES)
    assert refused is not None
    store.abandon_inflight()

    rebuilt = store.next_batch(CONTROL_PLANE, limit=2, max_bytes=MAX_SPOOL_BYTES)
    assert rebuilt is not None and rebuilt.batch_id != refused.batch_id
    assert rebuilt.first_sequence == refused.first_sequence


def test_a_torn_final_line_does_not_lose_what_came_before(root: Path) -> None:
    """A crash mid-append leaves half a line. Everything written before it is still good."""
    store = spool(root)
    fill(store, 3)
    with (root / "records.jsonl").open("a", encoding="utf-8") as stream:
        stream.write('{"record":"progress","seq')

    reopened = spool(root)
    batch = reopened.next_batch(CONTROL_PLANE, limit=10, max_bytes=MAX_SPOOL_BYTES)
    assert batch is not None and len(batch.records) == 3


def test_a_sequence_on_disk_beyond_the_recorded_mark_is_trusted(root: Path) -> None:
    """The records are the evidence; the cursor file is a summary that can lag a crash."""
    store = spool(root)
    fill(store, 3)
    stored = json.loads((root / "cursors.json").read_text(encoding="utf-8"))
    stored["last_sequence"] = 1
    (root / "cursors.json").write_text(json.dumps(stored), encoding="utf-8")

    assert spool(root).last_sequence == 3


# --- Bounds and gaps ---------------------------------------------------------------------------


def test_the_spool_stays_bounded_and_reports_what_it_discarded(
    root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("kratos_agent.spool.MAX_SPOOL_BYTES", 400)
    store = spool(root)
    fill(store, 40)

    assert (root / "records.jsonl").stat().st_size <= 400
    gaps = store.gaps(CONTROL_PLANE)
    assert gaps, "discarding records without reporting a gap would hide the loss"
    assert sum(gap.count for gap in gaps) > 0
    # The cursor moved past what was discarded, so the spool does not try forever to send
    # records that no longer exist.
    assert store.cursor(CONTROL_PLANE) >= gaps[-1].last_sequence


def test_a_gap_survives_a_restart(root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("kratos_agent.spool.MAX_SPOOL_BYTES", 400)
    store = spool(root)
    fill(store, 40)
    before = store.gaps(CONTROL_PLANE)

    assert spool(root).gaps(CONTROL_PLANE) == before


def test_a_sink_that_already_passed_a_discarded_range_loses_nothing(
    root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    store = spool(root, CONTROL_PLANE, OTLP)
    fill(store, 20)
    store.acknowledge(CONTROL_PLANE, 20)

    # Tighten the bound to just under what is already stored, so the next append evicts only the
    # oldest few records: ones the control plane has taken and the collector has not.
    stored_bytes = (root / "records.jsonl").stat().st_size
    monkeypatch.setattr("kratos_agent.spool.MAX_SPOOL_BYTES", stored_bytes + 10)
    store.append(record(100))

    assert store.gaps(CONTROL_PLANE) == (), "a sink that already has a record loses nothing by it"
    otlp_gaps = store.gaps(OTLP)
    assert otlp_gaps and otlp_gaps[0].first_sequence == 1
    assert otlp_gaps[0].last_sequence < 20, "only the oldest few were needed to make room"


# --- Housekeeping ------------------------------------------------------------------------------


def test_acknowledged_records_are_eventually_reclaimed(
    root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("kratos_agent.spool.COMPACT_THRESHOLD_BYTES", 1)
    store = spool(root)
    fill(store, 20)
    before = (root / "records.jsonl").stat().st_size
    store.acknowledge(CONTROL_PLANE, 20)
    assert (root / "records.jsonl").stat().st_size < before


def test_discard_removes_the_attempt(root: Path) -> None:
    store = spool(root)
    fill(store, 3)
    store.discard()
    assert not root.exists()


def test_a_batch_knows_the_range_it_covers() -> None:
    batch = Batch(
        batch_id=__import__("uuid").uuid4(),
        sink=CONTROL_PLANE,
        first_sequence=7,
        records=({"sequence": 7}, {"sequence": 8}),
    )
    assert batch.last_sequence == 8
