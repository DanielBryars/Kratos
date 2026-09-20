"""Delivers observations for the whole agent, not for one attempt.

This exists because the first version of this path got the ownership wrong, and the failure was
invisible: the only delivery thread belonged to the assignment being supervised and was stopped on
the result path. A control plane that was briefly unreachable could therefore see the job report
its result, watch the attempt disappear from assignment polling, and leave records on disk that
nothing would ever send. A restart was no better, because the code that knew where to send them
was built from an assignment that no longer existed.

So delivery lives above the assignment, and three things follow from that.

**The spool records its own stream.** After a restart there is no assignment to read it from. A
spool that cannot say where it is addressed can never be delivered, so the stream identifier is
written beside the records when the spool is created.

**Pending spools are discovered, not remembered.** The courier lists the observations directory at
startup and adopts whatever is there, so records that survived a crash are picked up by the next
process rather than depending on one that has gone.

**A spool is removed only when its cursor proves it was taken.** Deleting on the attempt becoming
terminal would discard exactly the records that a failing link had not yet managed to send.

Nothing here can delay a job. A result is reported without consulting this, and every failure is
counted and retried rather than raised.
"""

import threading
import time
from collections.abc import Callable
from pathlib import Path
from uuid import UUID

from kratos_agent.models import MAX_OBSERVATION_BATCH_BYTES, MAX_OBSERVATION_BATCH_RECORDS
from kratos_agent.pump import CONTROL_PLANE_SINK
from kratos_agent.spool import Batch, ObservationSpool, SpoolError

# How often the courier looks for something to send. The batch interval itself is enforced by
# the control plane's tolerance rather than here: a pass that finds nothing costs a directory
# listing.
POLL_SECONDS = 2.0
# How long a spool with nothing left to deliver is kept before removal. Not zero, because a spool
# whose last acknowledgement arrived moments ago may still be receiving a final line from a log
# reader that has not noticed its container exited.
RETIREMENT_SECONDS = 60.0

# Submit one batch for a stream and return the sequence the control plane accepted through.
Submit = Callable[[UUID, Batch], int]


class ObservationCourier:
    """Owns every spool on this worker and keeps trying to empty them."""

    def __init__(
        self,
        root: Path,
        submit: Submit,
        *,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], None] = time.sleep,
        poll_seconds: float = POLL_SECONDS,
    ) -> None:
        self._root = root
        self._submit = submit
        self._clock = clock
        self._sleep = sleep
        self._poll = poll_seconds
        self._adopted: dict[Path, ObservationSpool] = {}
        self._idle_since: dict[Path, float] = {}
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self.failures = 0
        # Spools that hold records but record no stream, so nothing can address them.
        self.unaddressable = 0

    # --- Ownership -----------------------------------------------------------------------------

    def adopt(self, spool: ObservationSpool, path: Path) -> None:
        """Take over delivery for a spool the runner has just created.

        The runner hands over the same object it gave the pump, rather than a second one on the
        same files, so appends and acknowledgements share the spool's own lock.
        """
        with self._lock:
            self._adopted[path] = spool

    def discover(self) -> list[Path]:
        """Find spools left behind by an earlier process."""
        if not self._root.exists():
            return []
        found: list[Path] = []
        try:
            entries = sorted(self._root.iterdir())
        except OSError:
            return []
        for entry in entries:
            if not entry.is_dir():
                continue
            with self._lock:
                if entry in self._adopted:
                    continue
            try:
                spool = ObservationSpool(entry, (CONTROL_PLANE_SINK,))
            except (SpoolError, OSError):
                self.failures += 1
                continue
            with self._lock:
                self._adopted[entry] = spool
            found.append(entry)
        return found

    # --- Delivery ------------------------------------------------------------------------------

    def deliver_once(self) -> int:
        """One pass: at most one batch per spool. Returns how many batches were accepted."""
        self.discover()
        with self._lock:
            spools = list(self._adopted.items())
        delivered = 0
        for path, spool in spools:
            try:
                if self._deliver(path, spool):
                    delivered += 1
            except Exception:  # noqa: BLE001 - nothing here may reach a job
                self.failures += 1
        return delivered

    def _deliver(self, path: Path, spool: ObservationSpool) -> bool:
        stream_id = spool.stream_id
        if stream_id is None:
            # A crash between creating the directory and recording the stream leaves records that
            # nothing can address. They are **not** deleted: a spool is removed only once its
            # cursor proves it was taken, and these were never taken. They are counted instead, so
            # an operator can see disk being held rather than discovering it as free space that
            # quietly went missing.
            self.unaddressable += 1
            return False
        batch = spool.next_batch(
            CONTROL_PLANE_SINK,
            limit=MAX_OBSERVATION_BATCH_RECORDS,
            max_bytes=MAX_OBSERVATION_BATCH_BYTES,
        )
        if batch is None:
            self._retire_if_idle(path, spool)
            return False
        self._idle_since.pop(path, None)
        accepted = self._submit(stream_id, batch)
        spool.acknowledge(CONTROL_PLANE_SINK, accepted)
        return True

    def _retire_if_idle(self, path: Path, spool: ObservationSpool) -> None:
        """Remove a spool whose records have all been acknowledged.

        Reached only when `next_batch` returned nothing, and it checks `pending` again anyway.
        That double guard is the point: a spool is never removed because its attempt ended, only
        because its cursor proves every record was taken. Anything else discards exactly the
        records a failing link had not managed to send.

        The wait after that is not caution about delivery, which is already proven, but about the
        writer: a log reader that has not yet noticed its container exited can still append a
        final line, and deleting the directory underneath it would lose that line and log an error
        for something that is not wrong.
        """
        if spool.pending(CONTROL_PLANE_SINK):
            self._idle_since.pop(path, None)
            return
        first_idle = self._idle_since.setdefault(path, self._clock())
        if self._clock() - first_idle < RETIREMENT_SECONDS:
            return
        try:
            # Rechecked and removed inside one lock. Asking whether it is empty and then removing
            # it are two moments, and a log reader can append between them.
            if not spool.discard_if_empty(CONTROL_PLANE_SINK):
                self._idle_since.pop(path, None)
                return
        except (OSError, SpoolError):
            self.failures += 1
            return
        with self._lock:
            self._adopted.pop(path, None)
        self._idle_since.pop(path, None)

    # --- The thread ----------------------------------------------------------------------------

    def start(self) -> None:
        if self._thread is not None:
            return
        self._thread = threading.Thread(target=self._run, name="kratos-courier", daemon=True)
        self._thread.start()

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                self.deliver_once()
            except Exception:  # noqa: BLE001 - the courier never dies of one bad pass
                self.failures += 1
            self._sleep(self._poll)

    def stop(self) -> None:
        """End the courier when the agent itself is ending, never because a job finished."""
        self._stop.set()

    def pending_paths(self) -> list[Path]:
        with self._lock:
            return [
                path for path, spool in self._adopted.items() if spool.pending(CONTROL_PLANE_SINK)
            ]
