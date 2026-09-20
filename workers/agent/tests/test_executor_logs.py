"""Following a container's output, and the one case where the agent declines to.

These use their own fakes rather than the ones in test_executor, because the streaming call has a
different shape from the bounded capture used for the result, and a fake that quietly refuses the
streaming arguments would let the reader do nothing while every test still passed.
"""

import threading
from datetime import UTC, datetime, timedelta
from typing import Any
from uuid import UUID

import docker

from kratos_agent.executor import DockerExecutor
from kratos_agent.models import JobAssignment, JobExecutionResult
from kratos_agent.observations import Stream

T0 = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)
IMAGE = "example.com/workload@sha256:" + "a" * 64
ATTEMPT_ID = UUID("33333333-3333-4333-8333-333333333333")


class Clock:
    def __init__(self) -> None:
        self.now = T0

    def __call__(self) -> datetime:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += timedelta(seconds=seconds)


class Container:
    """A container whose log stream is a fixed list of demultiplexed chunks."""

    def __init__(self, clock: Clock, chunks: list[tuple[bytes | None, bytes | None]]) -> None:
        self.clock = clock
        self.chunks = chunks
        self.status = "created"
        self.exits_at = T0 + timedelta(seconds=5)
        self.streamed = False
        self.attrs: dict[str, Any] = {
            "State": {
                "StartedAt": T0.isoformat().replace("+00:00", "Z"),
                "FinishedAt": "0001-01-01T00:00:00Z",
            }
        }

    def reload(self) -> None:
        if self.status == "created":
            self.status = "running"
        if self.clock.now >= self.exits_at:
            self.status = "exited"
            self.attrs["State"]["FinishedAt"] = self.clock.now.isoformat().replace("+00:00", "Z")

    def wait(self, timeout: int) -> dict[str, int]:
        return {"StatusCode": 0}

    def kill(self) -> None:
        self.status = "exited"

    def logs(self, **options: Any) -> Any:
        if options.get("stream"):
            # The reader must ask for exactly this shape, or it cannot tell the streams apart or
            # recover the container's own timestamps.
            assert options["demux"] is True
            assert options["timestamps"] is True
            assert options["follow"] is True
            self.streamed = True
            return iter(self.chunks)
        return b"captured\n"


class Containers:
    def __init__(self, container: Container, *, existing: bool) -> None:
        self.container = container
        self.existing = existing

    def get(self, _: str) -> Container:
        if not self.existing:
            raise docker.errors.NotFound("missing")
        self.container.status = "running"
        return self.container

    def run(self, _: str, **__: Any) -> Container:
        return self.container


class Client:
    def __init__(self, containers: Containers) -> None:
        self.containers = containers
        self.images = self
        self.api = self

    def version(self) -> dict[str, str]:
        return {"Version": "26.0.0"}


def assignment() -> JobAssignment:
    return JobAssignment(
        attempt_id=ATTEMPT_ID,
        job_id=UUID("44444444-4444-4444-8444-444444444444"),
        name="job",
        image_reference=IMAGE,
        gpu_index=0,
        timeout_seconds=60,
        lease_expires_at=T0 + timedelta(minutes=5),
    )


def run(
    chunks: list[tuple[bytes | None, bytes | None]], *, existing: bool = False
) -> tuple[Container, list[tuple[Stream, datetime, str]], JobExecutionResult]:
    clock = Clock()
    container = Container(clock, chunks)
    executor = DockerExecutor(
        Client(Containers(container, existing=existing)), clock=clock, sleep=clock.sleep
    )
    seen: list[tuple[Stream, datetime, str]] = []
    result = executor.run_job(
        assignment(),
        tick_seconds=1,
        observe=lambda stream, at, text: seen.append((stream, at, text)),
    )
    return container, seen, result


def test_output_reaches_the_observer_with_its_own_timestamps() -> None:
    container, seen, _ = run(
        [
            (b"2026-09-20T12:00:01Z first line\n", None),
            (None, b"2026-09-20T12:00:02Z on stderr\n"),
            (b"2026-09-20T12:00:03Z second line\n", None),
        ]
    )
    assert container.streamed
    assert [(stream, text) for stream, _, text in seen] == [
        (Stream.STDOUT, "first line"),
        (Stream.STDERR, "on stderr"),
        (Stream.STDOUT, "second line"),
    ]
    # The container's clock, not the agent's: these are evidence of when the line was written.
    assert seen[0][1] == datetime(2026, 9, 20, 12, 0, 1, tzinfo=UTC)


def test_a_line_split_across_chunks_arrives_once_and_whole() -> None:
    _, seen, _ = run([(b"2026-09-20T12:00:01Z half a ", None), (b"line\n", None)])
    assert [text for _, _, text in seen] == ["half a line"]


def test_a_final_line_without_a_newline_still_arrives() -> None:
    _, seen, _ = run([(b"2026-09-20T12:00:01Z no trailing newline", None)])
    assert [text for _, _, text in seen] == ["no trailing newline"]


def test_output_buffered_until_container_exit_is_drained_after_result() -> None:
    """A short job may exit before Docker yields its first buffered log chunk.

    Reporting the result must not wait for telemetry, but it also must not tell the daemon reader
    to discard that finite stream. The reader owns the drain and may finish just after the result.
    """

    release = threading.Event()

    class ExitBuffered(Container):
        def __init__(self, clock: Clock) -> None:
            super().__init__(clock, [])
            self.exited = threading.Event()

        def reload(self) -> None:
            super().reload()
            if self.status == "exited":
                self.exited.set()

        def logs(self, **options: Any) -> Any:
            if not options.get("stream"):
                return b"captured\n"

            def after_exit() -> Any:
                assert self.exited.wait(timeout=1)
                assert release.wait(timeout=1)
                yield (b"2026-09-20T12:00:04Z buffered until exit\n", None)

            self.streamed = True
            return after_exit()

    clock = Clock()
    container = ExitBuffered(clock)
    executor = DockerExecutor(
        Client(Containers(container, existing=False)), clock=clock, sleep=clock.sleep
    )
    seen: list[tuple[Stream, datetime, str]] = []
    delivered = threading.Event()

    def observe(stream: Stream, at: datetime, text: str) -> None:
        seen.append((stream, at, text))
        delivered.set()

    result = executor.run_job(assignment(), tick_seconds=1, observe=observe)

    assert result.exit_code == 0
    assert not delivered.is_set()
    release.set()
    assert delivered.wait(timeout=1)
    assert [(stream, text) for stream, _, text in seen] == [(Stream.STDOUT, "buffered until exit")]


def test_a_broken_log_stream_does_not_fail_the_job() -> None:
    """Reading output is telemetry, and telemetry may never end a run."""

    class Exploding(Container):
        def logs(self, **options: Any) -> Any:
            if options.get("stream"):
                raise docker.errors.APIError("log stream went away")
            return b"captured\n"

    clock = Clock()
    container = Exploding(clock, [])
    executor = DockerExecutor(
        Client(Containers(container, existing=False)), clock=clock, sleep=clock.sleep
    )
    result = executor.run_job(assignment(), tick_seconds=1, observe=lambda *_: None)
    assert result.exit_code == 0


def test_a_resumed_container_is_not_re_read_and_says_so() -> None:
    """Docker replays a log from the start, and every replayed line would take a new sequence.

    Batch idempotency is keyed on sequence, so it cannot help: the control plane would receive
    the same observations twice under different numbers. The absence is announced rather than
    left silent.
    """
    container, seen, _ = run([(b"2026-09-20T12:00:01Z would be replayed\n", None)], existing=True)
    assert container.streamed is False
    assert len(seen) == 1
    stream, _, text = seen[0]
    assert stream is Stream.STDERR
    assert "resumed" in text and "duplicate" in text


def test_no_observer_means_no_log_stream_at_all() -> None:
    clock = Clock()
    container = Container(clock, [])
    executor = DockerExecutor(
        Client(Containers(container, existing=False)), clock=clock, sleep=clock.sleep
    )
    executor.run_job(assignment(), tick_seconds=1)
    assert container.streamed is False


# --- The log driver -------------------------------------------------------------------------


def test_a_job_container_gets_a_bounded_non_blocking_log_driver() -> None:
    """Both halves of this matter, and for different reasons.

    Bounded, so a workload printing without restraint cannot fill the disk holding the agent's
    own state and every attempt's outputs. Non-blocking, because the default driver applies back
    pressure: a full logging pipe blocks the container's write, so a slow reader would stall the
    workload. That would let telemetry halt execution, which ADR-015 forbids outright.
    """
    clock = Clock()
    container = Container(clock, [])
    containers = Containers(container, existing=False)
    captured: dict[str, Any] = {}

    def run(_: str, **options: Any) -> Container:
        captured.update(options)
        return container

    containers.run = run  # type: ignore[method-assign]
    executor = DockerExecutor(Client(containers), clock=clock, sleep=clock.sleep)
    executor.run_job(assignment(), tick_seconds=1)

    config = captured["log_config"]
    assert config["type"] == "json-file"
    assert config["config"]["mode"] == "non-blocking"
    assert config["config"]["max-size"] == "10m"
    assert config["config"]["max-file"] == "3"
