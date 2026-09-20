import os
from pathlib import Path

import httpx
import pytest

from kratos_agent.outputs import DESCRIPTOR_WALK_SUPPORTED, OutputError, OutputIdentity
from kratos_agent.uploads import (
    CHUNK_BYTES,
    UploadConflict,
    UploadError,
    upload_object,
)

SESSION = "https://storage.googleapis.com/upload/session?upload_id=secret-value"

needs_posix = pytest.mark.skipif(
    not DESCRIPTOR_WALK_SUPPORTED, reason="output access requires a POSIX host"
)


def written(tmp_path: Path, content: bytes) -> tuple[Path, OutputIdentity, int]:
    path = tmp_path / "model.pt"
    path.write_bytes(content)
    return path, OutputIdentity.of(path.stat()), len(content)


def transport(handler: object) -> httpx.MockTransport:
    return httpx.MockTransport(handler)  # type: ignore[arg-type]


@needs_posix
def test_a_whole_object_is_sent_and_its_generation_returned(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")
    received: list[bytes] = []

    def handler(request: httpx.Request) -> httpx.Response:
        if request.headers["Content-Range"] == f"bytes */{length}":
            return httpx.Response(308)
        received.append(request.content)
        return httpx.Response(200, headers={"x-goog-generation": "17"})

    with httpx.Client(transport=transport(handler)) as client:
        completed = upload_object(client, SESSION, path, identity, length)

    assert completed.storage_generation == 17
    assert b"".join(received) == b"weights"


@needs_posix
def test_transfer_resumes_from_the_offset_cloud_storage_acknowledges(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"0123456789")
    ranges: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        sent = request.headers["Content-Range"]
        ranges.append(sent)
        if sent == f"bytes */{length}":
            # Four bytes already arrived before the agent restarted.
            return httpx.Response(308, headers={"Range": "bytes=0-3"})
        assert request.content == b"456789"
        return httpx.Response(201, headers={"x-goog-generation": "3"})

    with httpx.Client(transport=transport(handler)) as client:
        completed = upload_object(client, SESSION, path, identity, length)

    assert ranges == [f"bytes */{length}", f"bytes 4-9/{length}"]
    assert completed.storage_generation == 3


@needs_posix
def test_an_object_cloud_storage_already_holds_is_not_sent_again(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")
    bodies: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        bodies.append(len(request.content))
        return httpx.Response(200, headers={"x-goog-generation": "9"})

    with httpx.Client(transport=transport(handler)) as client:
        completed = upload_object(client, SESSION, path, identity, length)

    assert bodies == [0]
    assert completed.storage_generation == 9


@needs_posix
def test_a_partial_acknowledgement_continues_rather_than_restarting(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"x" * (CHUNK_BYTES + 32))
    offsets: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        sent = request.headers["Content-Range"]
        if sent == f"bytes */{length}":
            return httpx.Response(308)
        offsets.append(int(sent.split(" ")[1].split("-")[0]))
        if len(offsets) == 1:
            return httpx.Response(308, headers={"Range": f"bytes=0-{CHUNK_BYTES - 1}"})
        return httpx.Response(200, headers={"x-goog-generation": "5"})

    with httpx.Client(transport=transport(handler)) as client:
        upload_object(client, SESSION, path, identity, length)

    assert offsets == [0, CHUNK_BYTES]


@needs_posix
@pytest.mark.parametrize("status", [400, 404, 410])
def test_a_refused_session_is_permanent(tmp_path: Path, status: int) -> None:
    path, identity, length = written(tmp_path, b"weights")

    with (
        httpx.Client(transport=transport(lambda _: httpx.Response(status))) as client,
        pytest.raises(UploadConflict),
    ):
        upload_object(client, SESSION, path, identity, length)


@needs_posix
@pytest.mark.parametrize("status", [429, 500, 503])
def test_a_temporary_refusal_is_retryable(tmp_path: Path, status: int) -> None:
    path, identity, length = written(tmp_path, b"weights")

    with (
        httpx.Client(transport=transport(lambda _: httpx.Response(status))) as client,
        pytest.raises(UploadError) as raised,
    ):
        upload_object(client, SESSION, path, identity, length)

    assert not isinstance(raised.value, UploadConflict)


@needs_posix
def test_an_object_stored_at_the_wrong_length_is_rejected(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200, headers={"x-goog-generation": "2", "x-goog-stored-content-length": "3"}
        )

    with (
        httpx.Client(transport=transport(handler)) as client,
        pytest.raises(UploadError, match="stored 3 bytes, not 7"),
    ):
        upload_object(client, SESSION, path, identity, length)


@needs_posix
def test_a_completion_without_a_generation_is_rejected(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")

    with (
        httpx.Client(transport=transport(lambda _: httpx.Response(200))) as client,
        pytest.raises(UploadError, match="did not return an object generation"),
    ):
        upload_object(client, SESSION, path, identity, length)


@needs_posix
def test_a_file_replaced_since_it_was_hashed_is_never_uploaded(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")
    replacement = tmp_path / "other.pt"
    replacement.write_bytes(b"tamper!")  # same length, a different inode
    replacement.replace(path)

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(308, headers={"Range": "bytes=0-0"})

    with (
        httpx.Client(transport=transport(handler)) as client,
        pytest.raises(OutputError, match="changed after it was recorded"),
    ):
        upload_object(client, SESSION, path, identity, length)


@needs_posix
def test_a_session_that_never_advances_is_abandoned(tmp_path: Path) -> None:
    # A session that keeps acknowledging the same offset would otherwise be retried forever.
    path, identity, length = written(tmp_path, b"weights")

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(308, headers={"Range": "bytes=0-1"})

    with (
        httpx.Client(transport=transport(handler)) as client,
        pytest.raises(UploadConflict, match="made no progress past byte 2"),
    ):
        upload_object(client, SESSION, path, identity, length)


@needs_posix
def test_a_file_rewritten_in_place_since_it_was_hashed_is_never_uploaded(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")
    os.utime(path, ns=(1_000_000_000, 1_000_000_000))

    with (
        httpx.Client(transport=transport(lambda _: httpx.Response(308))) as client,
        pytest.raises(OutputError, match="changed after it was recorded"),
    ):
        upload_object(client, SESSION, path, identity, length)


@needs_posix
def test_a_transport_failure_never_reveals_the_session_uri(tmp_path: Path) -> None:
    path, identity, length = written(tmp_path, b"weights")

    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("no route to storage", request=request)

    with (
        httpx.Client(transport=transport(handler)) as client,
        pytest.raises(UploadError) as raised,
    ):
        upload_object(client, SESSION, path, identity, length)

    assert "upload_id" not in str(raised.value)
    assert "secret-value" not in str(raised.value)
