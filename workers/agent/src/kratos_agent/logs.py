"""Turn Docker's log chunks back into the lines a workload actually wrote.

The Engine hands back byte chunks, not lines. A chunk boundary can fall anywhere: between a
timestamp and its text, inside a word, or in the middle of a multi-byte character. Everything
awkward about reading a container's output lives here, away from the supervision loop and away
from the classifier, so both can be about their own job.

Two bounds matter, because the writer is a workload and the reader is the agent that has to keep
enforcing its deadline.

**A line that never ends.** A workload can write megabytes with no newline. The assembler emits
what it has once the pending bytes exceed the line bound and discards the rest of that line, so
memory is bounded by the caller's limit rather than by the workload's restraint.

**A character split across chunks.** Decoding each chunk independently would corrupt any
multi-byte character that straddles a boundary, so bytes are joined before they are decoded.
"""

from collections.abc import Iterator
from datetime import UTC, datetime

# Docker's own timestamps are RFC 3339 with nanosecond precision, which datetime cannot parse:
# it accepts at most six fractional digits.
_MAX_FRACTIONAL_DIGITS = 6


class LineAssembler:
    """Reassembles one stream's chunks into whole decoded lines.

    One assembler per stream, because stdout and stderr arrive interleaved and a partial line on
    one must not be completed by bytes from the other.
    """

    def __init__(self, max_line_bytes: int) -> None:
        self._max = max_line_bytes
        self._pending = bytearray()
        # Set while discarding the tail of a line that was already too long, so the remainder
        # does not arrive as a series of spurious short lines.
        self._skipping = False

    def feed(self, chunk: bytes) -> Iterator[str]:
        """Yield every complete line this chunk finished."""
        self._pending.extend(chunk)
        while True:
            index = self._pending.find(b"\n")
            if index < 0:
                break
            line = bytes(self._pending[:index])
            del self._pending[: index + 1]
            if self._skipping:
                self._skipping = False
                continue
            # The bound applies however the line arrived. Checking only the pending bytes would
            # let a whole over-long line through whenever its newline happened to land in the
            # same chunk, which is the common case for a workload that writes one big line.
            yield _decode(line[: self._max])
        if len(self._pending) > self._max:
            # Emit the bound's worth and abandon the rest of this line. The classifier will
            # refuse it for being oversize, which is the honest outcome: the agent saw a line it
            # was not prepared to carry.
            emitted = bytes(self._pending[: self._max])
            self._pending.clear()
            self._skipping = True
            yield _decode(emitted)

    def flush(self) -> Iterator[str]:
        """Yield a final line that the container ended without a newline."""
        if self._pending and not self._skipping:
            yield _decode(bytes(self._pending))
        self._pending.clear()
        self._skipping = False


def _decode(line: bytes) -> str:
    # A workload's output is not required to be valid UTF-8, and a decoding error must not end
    # the run, so undecodable bytes are replaced rather than raised over.
    return line.decode("utf-8", errors="replace").rstrip("\r")


def split_timestamp(line: str, *, fallback: datetime) -> tuple[datetime, str]:
    """Separate Docker's prepended timestamp from the workload's own text.

    ADR-015 requires each line's original container timestamp to be preserved as an attribute, so
    it is read from the stream rather than invented at the point of parsing. A line that does not
    carry one is not an error: the fallback is the agent's clock, and the text is returned whole.
    """
    head, separator, rest = line.partition(" ")
    if not separator:
        return fallback, line
    parsed = _parse_rfc3339(head)
    if parsed is None:
        return fallback, line
    return parsed, rest


def _parse_rfc3339(value: str) -> datetime | None:
    text = value
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    if "." in text:
        # Trim nanoseconds to microseconds. Truncating rather than rounding keeps the timestamp
        # at or before the instant the runtime recorded, which is the safer direction for
        # something used as evidence.
        start = text.index(".") + 1
        end = start
        while end < len(text) and text[end].isdigit():
            end += 1
        digits = text[start:end][:_MAX_FRACTIONAL_DIGITS]
        text = text[:start] + digits + text[end:]
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else parsed.replace(tzinfo=UTC)
