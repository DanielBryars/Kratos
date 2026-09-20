from datetime import UTC, datetime

import pytest

from kratos_agent.logs import LineAssembler, split_timestamp

FALLBACK = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)


def collect(assembler: LineAssembler, *chunks: bytes) -> list[str]:
    lines: list[str] = []
    for chunk in chunks:
        lines.extend(assembler.feed(chunk))
    return lines


# --- Reassembly ------------------------------------------------------------------------------


def test_whole_lines_come_out_whole() -> None:
    assembler = LineAssembler(max_line_bytes=1024)
    assert collect(assembler, b"one\ntwo\n") == ["one", "two"]


def test_a_line_split_across_chunks_is_rejoined() -> None:
    assembler = LineAssembler(max_line_bytes=1024)
    assert collect(assembler, b"half", b" a line\n") == ["half a line"]


def test_a_multi_byte_character_split_across_chunks_survives() -> None:
    """Decoding each chunk alone would corrupt this, which is why bytes are joined first."""
    encoded = "中文".encode()
    assembler = LineAssembler(max_line_bytes=1024)
    assert collect(assembler, encoded[:1], encoded[1:], b"\n") == ["中文"]


def test_carriage_returns_are_stripped() -> None:
    assembler = LineAssembler(max_line_bytes=1024)
    assert collect(assembler, b"windows\r\n") == ["windows"]


def test_undecodable_bytes_do_not_end_the_run() -> None:
    assembler = LineAssembler(max_line_bytes=1024)
    lines = collect(assembler, b"\xff\xfe bad bytes\n")
    assert len(lines) == 1 and "bad bytes" in lines[0]


def test_a_final_line_without_a_newline_is_flushed() -> None:
    """A container can exit mid-line, and its last words are often the interesting ones."""
    assembler = LineAssembler(max_line_bytes=1024)
    assert collect(assembler, b"no newline at the end") == []
    assert list(assembler.flush()) == ["no newline at the end"]


def test_flush_is_empty_when_everything_was_complete() -> None:
    assembler = LineAssembler(max_line_bytes=1024)
    collect(assembler, b"done\n")
    assert list(assembler.flush()) == []


# --- Bounds ----------------------------------------------------------------------------------


def test_a_line_that_never_ends_is_bounded() -> None:
    """A workload writing megabytes without a newline must not grow the agent's memory."""
    assembler = LineAssembler(max_line_bytes=16)
    lines = collect(assembler, b"x" * 100)
    assert lines == ["x" * 16]


def test_the_bound_applies_to_a_line_that_arrived_complete() -> None:
    """Checking only the pending bytes let a whole over-long line through whenever its newline
    landed in the same chunk, which is the usual case for a workload writing one big line."""
    assembler = LineAssembler(max_line_bytes=16)
    assert collect(assembler, b"y" * 40 + b"\n" + b"after\n") == ["y" * 16, "after"]


def test_the_rest_of_an_over_long_line_is_discarded_not_re_emitted() -> None:
    """Otherwise one enormous line would arrive as a stream of plausible short ones.

    The tail here arrives in a later chunk, so this is the path where the assembler has already
    emitted what it will and has to swallow the remainder when its newline finally turns up.
    """
    assembler = LineAssembler(max_line_bytes=16)
    lines = collect(assembler, b"y" * 40, b"y" * 40 + b"\n", b"after\n")
    assert lines == ["y" * 16, "after"]


def test_a_discarded_tail_does_not_leak_into_flush() -> None:
    assembler = LineAssembler(max_line_bytes=16)
    collect(assembler, b"z" * 40)
    assert list(assembler.flush()) == []


# --- Timestamps ---------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "stamp, expected",
    [
        ("2026-09-20T12:00:00Z", datetime(2026, 9, 20, 12, 0, tzinfo=UTC)),
        ("2026-09-20T12:00:00.123456789Z", datetime(2026, 9, 20, 12, 0, 0, 123456, tzinfo=UTC)),
        ("2026-09-20T12:00:00.5Z", datetime(2026, 9, 20, 12, 0, 0, 500000, tzinfo=UTC)),
    ],
)
def test_dockers_timestamp_is_read_from_the_line(stamp: str, expected: datetime) -> None:
    """Nanosecond precision is what Docker emits and what datetime refuses, so it is truncated."""
    at, text = split_timestamp(f"{stamp} hello world", fallback=FALLBACK)
    assert at == expected
    assert text == "hello world"


def test_a_line_without_a_timestamp_keeps_all_its_text() -> None:
    at, text = split_timestamp("no stamp here", fallback=FALLBACK)
    assert at == FALLBACK and text == "no stamp here"


def test_something_that_only_looks_like_a_timestamp_is_left_alone() -> None:
    at, text = split_timestamp("2026-13-45T99:99:99Z impossible", fallback=FALLBACK)
    assert at == FALLBACK and text == "2026-13-45T99:99:99Z impossible"


def test_a_json_record_is_not_mistaken_for_a_stamped_line() -> None:
    """The classifier needs the object whole; losing its first token would break every record."""
    line = '{"schema_version":"1.0","record":"progress","step":1}'
    at, text = split_timestamp(line, fallback=FALLBACK)
    assert at == FALLBACK and text == line


def test_a_stamped_json_record_keeps_its_object() -> None:
    line = '{"schema_version":"1.0","record":"progress","step":1}'
    at, text = split_timestamp(f"2026-09-20T12:00:00Z {line}", fallback=FALLBACK)
    assert at == datetime(2026, 9, 20, 12, 0, tzinfo=UTC)
    assert text == line


def test_an_empty_line_is_not_a_timestamp() -> None:
    at, text = split_timestamp("", fallback=FALLBACK)
    assert at == FALLBACK and text == ""
