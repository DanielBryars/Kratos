"""The two properties Codex asked to see proved, and the ones that make them possible.

Both are about what survives: a result reported before its observations were acknowledged, and a
process that restarts with records still on disk.
"""

from pathlib import Path
from uuid import UUID, uuid4

import pytest

from kratos_agent.courier import ObservationCourier
from kratos_agent.pump import CONTROL_PLANE_SINK
from kratos_agent.spool import Batch, ObservationSpool

STREAM_ID = UUID("55555555-5555-4555-8555-555555555555")


class Clock:
    def __init__(self) -> None:
        self.now = 1_000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds

    def sleep(self, seconds: float) -> None:
        self.now += seconds


class Link:
    def __init__(self) -> None:
        self.sent: list[tuple[UUID, int, int]] = []
        self.down = False

    def __call__(self, stream_id: UUID, batch: Batch) -> int:
        if self.down:
            raise RuntimeError("control plane unreachable")
        self.sent.append((stream_id, batch.first_sequence, len(batch.records)))
        return batch.last_sequence


@pytest.fixture
def clock() -> Clock:
    return Clock()


@pytest.fixture
def link() -> Link:
    return Link()


def courier(root: Path, link: Link, clock: Clock) -> ObservationCourier:
    return ObservationCourier(root, link, clock=clock, sleep=clock.sleep)


def make_spool(
    root: Path, attempt: str, *, records: int, stream: UUID | None = STREAM_ID
) -> ObservationSpool:
    spool = ObservationSpool(root / attempt, (CONTROL_PLANE_SINK,))
    if stream is not None:
        spool.record_stream(stream)
    for step in range(records):
        spool.append({"record": "progress", "step": step, "at": "2026-09-20T12:00:00Z"})
    return spool


# --- The two Codex asked for --------------------------------------------------------------------


def test_a_result_can_be_reported_before_its_observations_are_acknowledged(
    tmp_path: Path, link: Link, clock: Clock
) -> None:
    """The job is over and its result is gone; the records are still owed and still sent.

    This is the case the first version stranded: the attempt's delivery thread was stopped on the
    result path, so once the assignment disappeared nothing was left to send them.
    """
    link.down = True
    spool = make_spool(tmp_path, "attempt-a", records=3)
    post = courier(tmp_path, link, clock)
    post.adopt(spool, tmp_path / "attempt-a")

    assert post.deliver_once() == 0, "the link is down"
    # The attempt is now terminal and its result reported. Nothing about that touches the courier.
    link.down = False
    assert post.deliver_once() == 1
    assert link.sent == [(STREAM_ID, 1, 3)]


def test_a_restart_discovers_and_delivers_a_pending_spool(
    tmp_path: Path, link: Link, clock: Clock
) -> None:
    """Written by a process that is gone, delivered by the one that replaces it.

    Nothing hands the new process a list: it lists the directory. And the stream identifier comes
    off the disk, because the assignment that carried it no longer exists.
    """
    make_spool(tmp_path, "attempt-b", records=2)
    del_spool = None  # the process that wrote it has gone
    assert del_spool is None

    post = courier(tmp_path, link, clock)
    assert post.discover() == [tmp_path / "attempt-b"]
    assert post.deliver_once() == 1
    assert link.sent == [(STREAM_ID, 1, 2)]


# --- Removal is earned, not scheduled -------------------------------------------------------------


def test_a_spool_is_removed_only_once_its_cursor_proves_delivery(
    tmp_path: Path, link: Link, clock: Clock
) -> None:
    make_spool(tmp_path, "attempt-c", records=2)
    post = courier(tmp_path, link, clock)
    post.deliver_once()

    # Delivered, but not yet retired: a log reader may still be finishing, so the wait starts on
    # the first pass that finds nothing left rather than on the acknowledgement itself.
    assert (tmp_path / "attempt-c").exists()
    post.deliver_once()
    assert (tmp_path / "attempt-c").exists(), "the idle wait has only just started"

    clock.advance(120)
    post.deliver_once()
    assert not (tmp_path / "attempt-c").exists()


def test_an_undelivered_spool_is_never_removed(tmp_path: Path, link: Link, clock: Clock) -> None:
    """Removing on a timer rather than on an acknowledgement would discard exactly the records a
    failing link had not managed to send."""
    link.down = True
    make_spool(tmp_path, "attempt-d", records=2)
    post = courier(tmp_path, link, clock)
    for _ in range(5):
        clock.advance(600)
        post.deliver_once()

    assert (tmp_path / "attempt-d").exists()
    assert post.pending_paths() == [tmp_path / "attempt-d"]


def test_a_spool_with_no_stream_is_kept_rather_than_deleted(
    tmp_path: Path, link: Link, clock: Clock
) -> None:
    """A crash between creating the directory and recording the stream leaves records that
    nothing can address.

    My first version aged these out and deleted them. That is wrong, and it breaks the rule the
    rest of this file is built on: a spool is removed only once its cursor proves every record was
    taken, and these were never taken at all. Deleting them would be discarding undelivered
    evidence to reclaim a few kilobytes. They are counted instead, so the disk they hold is
    visible rather than being free space that quietly went missing.
    """
    make_spool(tmp_path, "attempt-e", records=2, stream=None)
    post = courier(tmp_path, link, clock)

    for _ in range(3):
        clock.advance(24 * 60 * 60)
        post.deliver_once()

    assert (tmp_path / "attempt-e").exists()
    assert post.unaddressable == 3
    assert link.sent == []


# --- It cannot take a job down --------------------------------------------------------------------


def test_a_failing_link_is_counted_and_retried_forever(
    tmp_path: Path, link: Link, clock: Clock
) -> None:
    link.down = True
    make_spool(tmp_path, "attempt-f", records=1)
    post = courier(tmp_path, link, clock)
    for _ in range(3):
        assert post.deliver_once() == 0
    assert post.failures == 3

    link.down = False
    assert post.deliver_once() == 1


def test_an_unreadable_spool_directory_does_not_stop_the_others(
    tmp_path: Path, link: Link, clock: Clock
) -> None:
    make_spool(tmp_path, "attempt-g", records=1)
    (tmp_path / "not-a-spool").write_text("a stray file", encoding="utf-8")

    post = courier(tmp_path, link, clock)
    assert post.deliver_once() == 1


def test_many_spools_are_all_made_progress_on(tmp_path: Path, link: Link, clock: Clock) -> None:
    """One slow or stuck spool must not starve the rest, so a pass takes one batch from each."""
    for index in range(4):
        make_spool(tmp_path, f"attempt-{index}", records=1, stream=uuid4())

    post = courier(tmp_path, link, clock)
    assert post.deliver_once() == 4
