"""Resumable transfer of verified outputs to Cloud Storage.

The control plane creates each session and verifies each object; this module only sends bytes to a
session URI it was given. The URI is a bearer credential scoped to one object, so it is never
logged, never persisted here and never sent anywhere but Cloud Storage.
"""

from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

from kratos_agent.outputs import OutputIdentity, open_verified

# Cloud Storage requires every chunk except the last to be a multiple of 256 KiB.
CHUNK_BYTES = 8 * 1024 * 1024
UPLOAD_TIMEOUT_SECONDS = 300


class UploadError(RuntimeError):
    """The object could not be transferred, or what arrived is not what was declared."""


class UploadConflict(UploadError):
    """Cloud Storage refused the session permanently; a new one must be requested."""


@dataclass(frozen=True)
class CompletedUpload:
    storage_generation: int
    byte_length: int


def upload_object(
    client: httpx.Client,
    session_uri: str,
    source: Path,
    identity: OutputIdentity,
    byte_length: int,
) -> CompletedUpload:
    """Send one output through its session, resuming from whatever Cloud Storage already holds.

    ``identity`` is re-checked on the opened descriptor, so a file replaced or rewritten since it
    was hashed is never uploaded under the manifest's checksums.
    """
    probed = _probe(client, session_uri, byte_length)
    if isinstance(probed, CompletedUpload):
        return probed
    offset = probed
    with open_verified(source, identity) as stream:
        while offset < byte_length:
            stream.seek(offset)
            chunk = stream.read(CHUNK_BYTES)
            if not chunk:
                raise UploadError("output ended before its declared length")
            end = offset + len(chunk) - 1
            response = _send(
                client,
                session_uri,
                content=chunk,
                headers={
                    "Content-Length": str(len(chunk)),
                    "Content-Range": f"bytes {offset}-{end}/{byte_length}",
                },
            )
            if response.status_code in (200, 201):
                return _generation(response, byte_length)
            if response.status_code == 308:
                resumed = _resume_offset(response)
                if resumed <= offset:
                    # Cloud Storage acknowledged no more than it already had. Retrying the same
                    # range would spin forever, so this session needs replacing.
                    raise UploadConflict(f"upload made no progress past byte {offset}")
                offset = resumed
                continue
            raise _refused(response)
    # Every declared byte was acknowledged without a final response, so ask once more.
    final = _probe(client, session_uri, byte_length)
    if isinstance(final, CompletedUpload):
        return final
    raise UploadError("Cloud Storage did not report the completed object")


def _probe(client: httpx.Client, session_uri: str, byte_length: int) -> CompletedUpload | int:
    """Ask Cloud Storage what it holds: a finished object, or how many bytes have arrived."""
    response = _send(
        client,
        session_uri,
        content=b"",
        headers={"Content-Length": "0", "Content-Range": f"bytes */{byte_length}"},
    )
    if response.status_code in (200, 201):
        return _generation(response, byte_length)
    if response.status_code == 308:
        return _resume_offset(response)
    raise _refused(response)


def _send(
    client: httpx.Client, session_uri: str, *, content: bytes, headers: dict[str, str]
) -> httpx.Response:
    try:
        return client.put(
            session_uri, content=content, headers=headers, timeout=UPLOAD_TIMEOUT_SECONDS
        )
    except httpx.HTTPError as error:
        # Never include the session URI, which httpx puts in its own message.
        raise UploadError(f"upload request failed: {type(error).__name__}") from None


def _resume_offset(response: httpx.Response) -> int:
    """Read the acknowledged offset, which is authoritative over what this agent sent."""
    committed = response.headers.get("Range")
    if committed is None:
        # No Range means Cloud Storage holds nothing yet.
        return 0
    try:
        return int(committed.rsplit("-", 1)[1]) + 1
    except (IndexError, ValueError):
        raise UploadError(f"unusable resume position {committed!r}") from None


def _generation(response: httpx.Response, byte_length: int) -> CompletedUpload:
    generation = response.headers.get("x-goog-generation")
    stored = response.headers.get("x-goog-stored-content-length")
    if generation is None:
        raise UploadError("Cloud Storage did not return an object generation")
    try:
        completed = CompletedUpload(storage_generation=int(generation), byte_length=byte_length)
    except ValueError:
        raise UploadError("Cloud Storage returned an unusable generation") from None
    if stored is not None and stored != str(byte_length):
        raise UploadError(f"Cloud Storage stored {stored} bytes, not {byte_length}")
    return completed


def _refused(response: httpx.Response) -> UploadError:
    if response.status_code in (400, 404, 410):
        # The session is gone or was rejected; retrying it can never succeed.
        return UploadConflict(f"upload session refused with {response.status_code}")
    return UploadError(f"Cloud Storage returned {response.status_code}")


def redacted(value: Any) -> str:
    """Session URIs are credentials; nothing derived from one may reach a log."""
    return "<redacted>" if value else ""
