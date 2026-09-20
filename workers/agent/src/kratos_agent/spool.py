"""Durable, bounded storage for an attempt's observations, with a cursor per sink.

ADR-015 requires that delivery be ordered by an agent-assigned sequence rather than by container
timestamps, that each sink hold **its own** durable cursor, and that a record be retained until
every configured sink has passed it. This module is that storage.

Three things here are load-bearing, and each exists because the obvious alternative is wrong.

**A sequence, not a timestamp.** Container timestamps repeat, can go backwards, and cannot express
a gap. A sequence assigned by the agent can do all three, which is what makes a missing range
reportable rather than merely absent.

**A cursor per sink, not one high-water mark.** The control plane and the collector acknowledge at
different times and fail independently. One mark either advances past records a sink never received
or replays records another already holds; there is no single number that is correct for both.

**A batch is written down before it is sent.** Not because a new identifier would be refused --
the control plane is idempotent on stream and sequence, so an identical batch replays safely under
any identifier -- but because a batch rebuilt from whatever records happen to be present after a
restart can split the stream differently. Presenting an already-accepted sequence inside a
differently-shaped batch is a content change, and that is what earns a 409. Writing the batch down
makes the resend identical by construction, and keeps the audit trail intact.

The spool is bounded. When it fills, the oldest records a sink has not yet taken are discarded
first, and the discarded range is remembered as a gap for that sink, because ADR-015 requires loss
above a cursor to be reported explicitly rather than inferred from a hole in the numbering.
"""

import json
import os
import tempfile
import threading
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from uuid import UUID, uuid4

# Sized so an attempt's telemetry cannot crowd out the state the agent needs to enforce a lease.
MAX_SPOOL_BYTES = 32 * 1024 * 1024
# Rewriting the record file discards what every sink has taken. Doing it on every acknowledgement
# would rewrite the file constantly; this is the wasted-space threshold that triggers one.
COMPACT_THRESHOLD_BYTES = 4 * 1024 * 1024

RECORDS_NAME = "records.jsonl"
STREAM_NAME = "stream.json"
CURSORS_NAME = "cursors.json"
INFLIGHT_NAME = "inflight.json"


class SpoolError(RuntimeError):
    """The spool could not be read or written. Never raised into supervision by the caller."""


@dataclass(frozen=True)
class Gap:
    """A range this sink will never receive, because the spool discarded it to stay bounded."""

    first_sequence: int
    last_sequence: int

    @property
    def count(self) -> int:
        return self.last_sequence - self.first_sequence + 1


@dataclass(frozen=True)
class Batch:
    """A contiguous run of records, identified durably before it is ever sent."""

    batch_id: UUID
    sink: str
    first_sequence: int
    records: tuple[dict[str, Any], ...]

    @property
    def last_sequence(self) -> int:
        return self.first_sequence + len(self.records) - 1


def _sequence_of(record: dict[str, Any]) -> int:
    """The one member this module relies on, checked rather than trusted.

    The file is written by this agent, but it is still a file on disk that a crash, a partial
    write or a stray edit can reach, so a record whose sequence is not a whole number is treated
    as unusable rather than coerced into one.
    """
    value = record.get("sequence")
    if not isinstance(value, int) or isinstance(value, bool):
        raise SpoolError("a spooled record has no usable sequence")
    return value


def _atomic_write(path: Path, payload: object) -> None:
    descriptor, temporary_name = tempfile.mkstemp(prefix=".spool-", dir=path.parent)
    temporary_path = Path(temporary_name)
    try:
        os.chmod(temporary_path, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(payload, stream, separators=(",", ":"))
            stream.flush()
            os.fsync(stream.fileno())
        temporary_path.replace(path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


class ObservationSpool:
    """One attempt's observations on disk, plus where each sink has got to.

    Thread-safe, and it has to be: the attempt's log reader appends while the agent-level courier
    reads, acknowledges and eventually removes. Anything that both inspects and then acts -- most
    of all `discard_if_empty` -- does so inside a single acquisition, because releasing between
    the two is what lets a record be written and then deleted without ever being seen.
    """

    def __init__(self, root: Path, sinks: Iterable[str]) -> None:
        # Held by every public method. The spool outlives the attempt that created it: an
        # attempt's log reader may still be appending while the agent-level courier is reading
        # and acknowledging, and after the attempt ends the courier is the only caller left.
        self._lock = threading.RLock()
        self._sinks = tuple(dict.fromkeys(sinks))
        if not self._sinks:
            raise SpoolError("a spool needs at least one sink to retain records for")
        self._root = root
        self._records_path = root / RECORDS_NAME
        self._cursors_path = root / CURSORS_NAME
        self._inflight_path = root / INFLIGHT_NAME
        self._stream_path = root / STREAM_NAME
        root.mkdir(mode=0o700, parents=True, exist_ok=True)

        self._cursors: dict[str, int] = dict.fromkeys(self._sinks, 0)
        self._gaps: dict[str, list[Gap]] = {sink: [] for sink in self._sinks}
        self._last_sequence = 0
        self._bytes = 0
        self._load()

    # --- Stream identity -------------------------------------------------------------------------

    def record_stream(self, stream_id: UUID) -> None:
        """Bind these records to a control-plane stream, once and only once.

        The courier that delivers a spool after a restart has no assignment to read this from:
        the attempt is gone, and with it the only thing that knew where its observations were
        addressed. Without this on disk, a spool that survived a crash could never be sent.

        Binding is **write-once**. Recording the same identifier again is accepted and changes
        nothing; recording a different one raises, leaving every file untouched. An overwrite would
        silently redirect records that were already spooled for one stream into another, which is
        worse than refusing: the records would arrive, be accepted, and be attributed to the wrong
        run. An existing file that cannot be read is treated the same way, because a spool whose
        address is unreadable is not a spool whose address may be replaced.
        """
        with self._lock:
            existing = self._stream_path.exists()
            if existing:
                current = self._stream_id()
                if current == stream_id:
                    return
                raise SpoolError("spool is already bound to a different observation stream")
            _atomic_write(self._stream_path, {"schema_version": 1, "stream_id": str(stream_id)})

    @property
    def stream_id(self) -> UUID | None:
        with self._lock:
            return self._stream_id()

    def _stream_id(self) -> UUID | None:
        if not self._stream_path.exists():
            return None
        try:
            stored = json.loads(self._stream_path.read_text(encoding="utf-8"))
            return UUID(str(stored["stream_id"]))
        except (OSError, ValueError, KeyError):
            return None

    # --- Recovery ------------------------------------------------------------------------------

    def _load(self) -> None:
        if self._cursors_path.exists():
            try:
                stored = json.loads(self._cursors_path.read_text(encoding="utf-8"))
            except (OSError, ValueError) as error:
                raise SpoolError("spool cursors are unreadable") from error
            for sink in self._sinks:
                self._cursors[sink] = int(stored.get("cursors", {}).get(sink, 0))
                self._gaps[sink] = [
                    Gap(int(gap["first_sequence"]), int(gap["last_sequence"]))
                    for gap in stored.get("gaps", {}).get(sink, ())
                ]
            self._last_sequence = int(stored.get("last_sequence", 0))

        if self._records_path.exists():
            self._bytes = self._records_path.stat().st_size
            # The highest sequence on disk may exceed the recorded high-water mark if the agent
            # stopped between appending a record and updating the cursor file. Trust the records.
            for record in self._read_records():
                self._last_sequence = max(self._last_sequence, _sequence_of(record))

    def _read_records(self) -> list[dict[str, Any]]:
        if not self._records_path.exists():
            return []
        records: list[dict[str, Any]] = []
        try:
            with self._records_path.open("r", encoding="utf-8") as stream:
                for line in stream:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        parsed = json.loads(line)
                    except ValueError:
                        # A torn final line is the normal shape of a crash mid-append. Everything
                        # before it is intact, so the spool keeps what it can rather than failing.
                        continue
                    if isinstance(parsed, dict) and "sequence" in parsed:
                        records.append(parsed)
        except OSError as error:
            raise SpoolError("spool records are unreadable") from error
        return records

    # --- Writing -------------------------------------------------------------------------------

    def append(self, record: dict[str, Any]) -> int:
        """Store one record and return the sequence it was given."""
        with self._lock:
            return self._append(record)

    def _append(self, record: dict[str, Any]) -> int:
        sequence = self._last_sequence + 1
        stored = {**record, "sequence": sequence}
        line = json.dumps(stored, separators=(",", ":")) + "\n"
        encoded = line.encode("utf-8")

        if self._bytes + len(encoded) > MAX_SPOOL_BYTES:
            self._evict_oldest(len(encoded))

        try:
            with self._records_path.open("a", encoding="utf-8") as stream:
                stream.write(line)
                stream.flush()
                os.fsync(stream.fileno())
        except OSError as error:
            raise SpoolError("spool record could not be written") from error

        self._bytes += len(encoded)
        self._last_sequence = sequence
        self._save_cursors()
        return sequence

    def _evict_oldest(self, needed: int) -> None:
        """Discard the oldest records to make room, and remember what each sink thereby lost."""
        records = self._read_records()
        if not records:
            return
        freed = 0
        discarded: list[int] = []
        remaining: list[dict[str, Any]] = []
        for record in records:
            line = json.dumps(record, separators=(",", ":")) + "\n"
            if freed < needed:
                freed += len(line.encode("utf-8"))
                discarded.append(_sequence_of(record))
            else:
                remaining.append(record)
        if not discarded:
            return

        first, last = min(discarded), max(discarded)
        for sink in self._sinks:
            # Only a sink that had not already passed the range loses anything by this.
            if self._cursors[sink] < last:
                start = max(first, self._cursors[sink] + 1)
                self._gaps[sink].append(Gap(start, last))
                # The sink can never receive these, so its cursor moves past them: leaving it
                # behind would make the spool try forever to send records that no longer exist.
                self._cursors[sink] = last
        self._rewrite(remaining)

    def _rewrite(self, records: list[dict[str, Any]]) -> None:
        descriptor, temporary_name = tempfile.mkstemp(prefix=".records-", dir=self._root)
        temporary_path = Path(temporary_name)
        try:
            os.chmod(temporary_path, 0o600)
            total = 0
            with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
                for record in records:
                    line = json.dumps(record, separators=(",", ":")) + "\n"
                    total += len(line.encode("utf-8"))
                    stream.write(line)
                stream.flush()
                os.fsync(stream.fileno())
            temporary_path.replace(self._records_path)
            self._bytes = total
        except BaseException:
            temporary_path.unlink(missing_ok=True)
            raise

    def _save_cursors(self) -> None:
        _atomic_write(
            self._cursors_path,
            {
                "schema_version": 1,
                "last_sequence": self._last_sequence,
                "cursors": dict(self._cursors),
                "gaps": {
                    sink: [
                        {"first_sequence": gap.first_sequence, "last_sequence": gap.last_sequence}
                        for gap in gaps
                    ]
                    for sink, gaps in self._gaps.items()
                },
            },
        )

    # --- Reading -------------------------------------------------------------------------------

    def next_batch(self, sink: str, limit: int, max_bytes: int) -> Batch | None:
        with self._lock:
            return self._next_batch(sink, limit, max_bytes)

    def _next_batch(self, sink: str, limit: int, max_bytes: int) -> Batch | None:
        """The next contiguous run for this sink, or None when it has taken everything.

        A batch already in flight is returned unchanged, with the identity it was first given, so
        a resend after a restart is the exact replay the control plane acknowledges idempotently
        rather than a new batch carrying the same sequences.
        """
        if sink not in self._cursors:
            raise SpoolError("unknown sink")

        inflight = self._read_inflight()
        if inflight is not None and inflight.sink == sink:
            return inflight

        cursor = self._cursors[sink]
        chosen: list[dict[str, Any]] = []
        total = 0
        expected = cursor + 1
        for record in self._read_records():
            sequence = _sequence_of(record)
            if sequence <= cursor:
                continue
            if sequence != expected:
                # Only contiguous runs are sent, so a hole left by eviction ends the batch rather
                # than being silently closed up.
                break
            size = len(json.dumps(record, separators=(",", ":")).encode("utf-8"))
            if chosen and (len(chosen) >= limit or total + size > max_bytes):
                break
            chosen.append(record)
            total += size
            expected += 1
        if not chosen:
            return None

        batch = Batch(
            batch_id=uuid4(),
            sink=sink,
            first_sequence=int(chosen[0]["sequence"]),
            records=tuple(chosen),
        )
        self._write_inflight(batch)
        return batch

    def _read_inflight(self) -> Batch | None:
        if not self._inflight_path.exists():
            return None
        try:
            stored = json.loads(self._inflight_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            # An unreadable in-flight note is not worth failing over: the batch is rebuilt from
            # the records, which are still there, and the control plane refuses a true duplicate.
            self._inflight_path.unlink(missing_ok=True)
            return None
        return Batch(
            batch_id=UUID(stored["batch_id"]),
            sink=str(stored["sink"]),
            first_sequence=int(stored["first_sequence"]),
            records=tuple(stored["records"]),
        )

    def _write_inflight(self, batch: Batch) -> None:
        _atomic_write(
            self._inflight_path,
            {
                "schema_version": 1,
                "batch_id": str(batch.batch_id),
                "sink": batch.sink,
                "first_sequence": batch.first_sequence,
                "records": list(batch.records),
            },
        )

    # --- Acknowledgement -----------------------------------------------------------------------

    def acknowledge(self, sink: str, through_sequence: int) -> None:
        with self._lock:
            self._acknowledge(sink, through_sequence)

    def _acknowledge(self, sink: str, through_sequence: int) -> None:
        """Record that this sink holds everything up to and including `through_sequence`."""
        if sink not in self._cursors:
            raise SpoolError("unknown sink")
        # A cursor never goes backwards: a late or repeated acknowledgement for an earlier batch
        # must not undo progress another acknowledgement already recorded.
        self._cursors[sink] = max(self._cursors[sink], through_sequence)
        self._inflight_path.unlink(missing_ok=True)
        self._save_cursors()
        self._compact_if_worthwhile()

    def abandon_inflight(self) -> None:
        with self._lock:
            self._abandon_inflight()

    def _abandon_inflight(self) -> None:
        """Forget the in-flight batch without advancing anything.

        Used when the control plane refuses a batch outright, so the next attempt builds a fresh
        one rather than resending an identity the server has already rejected.
        """
        self._inflight_path.unlink(missing_ok=True)

    def _compact_if_worthwhile(self) -> None:
        taken = min(self._cursors.values()) if self._cursors else 0
        if taken <= 0:
            return
        records = self._read_records()
        remaining = [record for record in records if _sequence_of(record) > taken]
        if len(remaining) == len(records):
            return
        discarded_bytes = self._bytes - sum(
            len(json.dumps(record, separators=(",", ":")).encode("utf-8")) + 1
            for record in remaining
        )
        if discarded_bytes < COMPACT_THRESHOLD_BYTES and remaining:
            return
        self._rewrite(remaining)

    # --- Reporting -----------------------------------------------------------------------------

    def gaps(self, sink: str) -> tuple[Gap, ...]:
        # Named apart from the `_gaps` mapping it reads, which a private twin would
        # shadow: the attribute wins, and the call becomes a dict lookup.
        with self._lock:
            return self._gaps_for(sink)

    def _gaps_for(self, sink: str) -> tuple[Gap, ...]:
        return tuple(self._gaps.get(sink, ()))

    def cursor(self, sink: str) -> int:
        with self._lock:
            return self._cursor(sink)

    def _cursor(self, sink: str) -> int:
        return self._cursors[sink]

    @property
    def last_sequence(self) -> int:
        return self._last_sequence

    def pending(self, sink: str) -> bool:
        with self._lock:
            return self._pending(sink)

    def _pending(self, sink: str) -> bool:
        return self._cursors[sink] < self._last_sequence

    def discard_if_empty(self, sink: str) -> bool:
        """Remove the spool only if this sink has taken everything, without letting go.

        The emptiness check and the removal are one critical section on purpose. Checking under
        one lock acquisition and discarding under another leaves a window in which the attempt's
        log reader appends a record, and the discard then deletes a record nobody has seen. The
        window is small, which is exactly what makes it the kind of loss that is never reproduced
        and never explained.
        """
        with self._lock:
            if sink not in self._cursors:
                raise SpoolError("unknown sink")
            if self._cursors[sink] < self._last_sequence:
                return False
            self._discard()
            return True

    def discard(self) -> None:
        with self._lock:
            self._discard()

    def _discard(self) -> None:
        """Remove everything for this attempt, once every sink has been satisfied or given up.

        Every file this class writes has to be listed here. The directory is removed only when it
        is empty, so one forgotten file leaves it behind for ever, and the failure is silent: the
        records are gone, the cursor is gone, and all that remains is a directory the courier
        will pick up and find nothing in.
        """
        for path in (
            self._records_path,
            self._cursors_path,
            self._inflight_path,
            self._stream_path,
        ):
            path.unlink(missing_ok=True)
        with os.scandir(self._root) as entries:
            if not any(entries):
                self._root.rmdir()
