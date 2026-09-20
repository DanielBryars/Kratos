"""The output mount against a real Docker daemon, not a fake.

These prove the two things a fake cannot: that Docker accepts the subpath mount at all, and that a
workload running as an arbitrary non-root user can actually write into it. Skipped when no usable
daemon is present, so the suite still runs anywhere.
"""

import io
import os
import tarfile
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

docker = pytest.importorskip("docker")

from kratos_agent.executor import (  # noqa: E402
    MINIMUM_SUBPATH_ENGINE_MAJOR,
    OUTPUT_MOUNT_TARGET,
)
from kratos_agent.models import JobOutputRequirement  # noqa: E402
from kratos_agent.outputs import build_manifest, create_attempt_tree, discard_tree  # noqa: E402

BUSYBOX = "busybox:1.36-uclibc"
WORKLOAD_UID = 4242


def engine_supports_subpath(client: object) -> bool:
    try:
        version = client.version()  # type: ignore[attr-defined]
        return int(str(version["Version"]).split(".")[0]) >= MINIMUM_SUBPATH_ENGINE_MAJOR
    except Exception:
        return False


@pytest.fixture(scope="module")
def client() -> Iterator[object]:
    try:
        engine = docker.from_env()
        engine.ping()
    except Exception as error:  # pragma: no cover - depends on the host
        pytest.skip(f"no usable Docker daemon: {type(error).__name__}: {error}")
    if not engine_supports_subpath(engine):
        pytest.skip("this Docker Engine cannot mount a volume subpath")
    try:
        engine.images.get(BUSYBOX)
    except docker.errors.ImageNotFound:
        try:
            engine.images.pull(BUSYBOX)
        except Exception as error:  # pragma: no cover - depends on the host
            pytest.skip(f"{BUSYBOX} is unavailable: {type(error).__name__}")
    yield engine


@pytest.fixture
def state_volume(client: object) -> Iterator[str]:
    name = f"kratos-test-state-{uuid.uuid4().hex[:12]}"
    volume = client.volumes.create(name)  # type: ignore[attr-defined]
    try:
        yield name
    finally:
        volume.remove(force=True)


def read_from_volume(client: object, volume: str, path: str) -> bytes:
    """Read one file out of a named volume through a throwaway container."""
    container = client.containers.create(  # type: ignore[attr-defined]
        BUSYBOX, command=["true"], mounts=[docker.types.Mount("/state", volume, type="volume")]
    )
    try:
        stream, _ = container.get_archive(f"/state/{path}")
        archive = io.BytesIO(b"".join(stream))
        with tarfile.open(fileobj=archive) as tar:
            member = tar.next()
            assert member is not None
            extracted = tar.extractfile(member)
            assert extracted is not None
            return extracted.read()
    finally:
        container.remove(force=True)


def prepare_attempt(client: object, volume: str, attempt_id: uuid.UUID) -> None:
    """Create the attempt tree inside the volume exactly as the agent does on its own filesystem."""
    script = (
        f"mkdir -p /state/attempts/{attempt_id}/outputs && "
        f"chmod 700 /state/attempts/{attempt_id} && "
        f"chmod 777 /state/attempts/{attempt_id}/outputs"
    )
    client.containers.run(  # type: ignore[attr-defined]
        BUSYBOX,
        command=["sh", "-c", script],
        mounts=[docker.types.Mount("/state", volume, type="volume")],
        remove=True,
    )


@pytest.mark.slow
def test_a_workload_running_as_another_user_can_write_its_outputs(
    client: object, state_volume: str
) -> None:
    attempt_id = uuid.uuid4()
    prepare_attempt(client, state_volume, attempt_id)

    # A workload with a user the agent knows nothing about, as any published image may have.
    logs = client.containers.run(  # type: ignore[attr-defined]
        BUSYBOX,
        command=["sh", "-c", f"id -u && echo weights > {OUTPUT_MOUNT_TARGET}/model.pt"],
        user=str(WORKLOAD_UID),
        network_disabled=True,
        read_only=True,
        cap_drop=["ALL"],
        security_opt=["no-new-privileges"],
        mounts=[
            docker.types.Mount(
                target=OUTPUT_MOUNT_TARGET,
                source=state_volume,
                type="volume",
                read_only=False,
                subpath=f"attempts/{attempt_id}/outputs",
            )
        ],
        remove=True,
    )
    assert logs.decode().strip().splitlines()[0] == str(WORKLOAD_UID)

    written = read_from_volume(client, state_volume, f"attempts/{attempt_id}/outputs/model.pt")
    assert written == b"weights\n"


@pytest.mark.slow
def test_the_job_sees_only_its_own_attempt_and_not_the_volume_root(
    client: object, state_volume: str
) -> None:
    attempt_id, other = uuid.uuid4(), uuid.uuid4()
    prepare_attempt(client, state_volume, attempt_id)
    prepare_attempt(client, state_volume, other)
    # The worker credential lives at the volume root; nothing must reach it.
    client.containers.run(  # type: ignore[attr-defined]
        BUSYBOX,
        command=["sh", "-c", "echo credential > /state/state.json"],
        mounts=[docker.types.Mount("/state", state_volume, type="volume")],
        remove=True,
    )

    listing = client.containers.run(  # type: ignore[attr-defined]
        BUSYBOX,
        command=["sh", "-c", f"ls -a {OUTPUT_MOUNT_TARGET}/.. 2>&1; ls -a {OUTPUT_MOUNT_TARGET}"],
        user=str(WORKLOAD_UID),
        network_disabled=True,
        mounts=[
            docker.types.Mount(
                target=OUTPUT_MOUNT_TARGET,
                source=state_volume,
                type="volume",
                read_only=False,
                subpath=f"attempts/{attempt_id}/outputs",
            )
        ],
        remove=True,
    ).decode()

    assert "state.json" not in listing
    assert str(other) not in listing


@pytest.mark.skipif(os.name != "posix", reason="file modes are meaningless on this host")
def test_the_agent_collects_what_another_user_wrote(tmp_path: Path) -> None:
    # The agent's own filesystem view: it does not own the file and cannot chmod it, which is the
    # case the sealing must tolerate rather than reject.
    attempt = tmp_path / "attempts" / str(uuid.uuid4())
    outputs = create_attempt_tree(attempt)
    assert outputs.stat().st_mode & 0o777 == 0o777
    assert attempt.stat().st_mode & 0o777 == 0o700
    (outputs / "model.pt").write_bytes(b"123456789")

    requirement = JobOutputRequirement(
        logical_path="model.pt",
        role="model",
        media_type="application/octet-stream",
        mandatory=True,
        max_bytes=1024,
    )

    (output,) = build_manifest(outputs, [requirement])

    assert output.file.crc32c == "4waSgw=="
    discard_tree(attempt)
    assert not attempt.exists()
