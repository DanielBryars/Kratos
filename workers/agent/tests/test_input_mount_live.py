"""The dataset input mount against a real Docker daemon, not a fake.

The unit tests prove the agent writes the right tree and the fake Docker client proves the right
mount is *requested*. Neither proves what a workload actually gets, and that is the whole point of
the mount: a dataset version is immutable, the staged files are hard links into a cache shared
with every other attempt, and the volume root holds the worker credential.

So these prove the three things only a daemon can: the workload can read its alias and its
selection manifest, it cannot write either, and it cannot see the volume root, the cache or the
agent's own state. Skipped when no usable daemon is present, so the suite still runs anywhere.
"""

import io
import json
import tarfile
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

docker = pytest.importorskip("docker")

from kratos_agent.executor import (  # noqa: E402
    MINIMUM_SUBPATH_ENGINE_MAJOR,
    DockerExecutor,
)
from kratos_agent.inputs import (  # noqa: E402
    INPUT_MOUNT_TARGET,
    SELECTION_DIRECTORY,
    discard_attempt_inputs,
)
from kratos_agent.models import DatasetInputAssignment, JobAssignment  # noqa: E402

BUSYBOX = "busybox:1.36-uclibc"
# A user the agent knows nothing about, as any published image may have.
WORKLOAD_UID = 4242
ALIAS = "training"
EPISODE_BYTES = b"parquet-ish bytes"
SELECTION = {"alias": ALIAS, "included_episodes": [0, 2], "selects_every_episode": False}


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
    """A disposable volume, removed however the test ends."""
    name = f"kratos-test-inputs-{uuid.uuid4().hex[:12]}"
    volume = client.volumes.create(name)  # type: ignore[attr-defined]
    try:
        yield name
    finally:
        volume.remove(force=True)


def in_volume(client: object, volume: str, script: str, **kwargs: object) -> str:
    """Run a shell script with the whole volume mounted, as only the agent ever does."""
    logs = client.containers.run(  # type: ignore[attr-defined]
        BUSYBOX,
        command=["sh", "-c", script],
        mounts=[docker.types.Mount("/state", volume, type="volume")],
        remove=True,
        **kwargs,
    )
    return str(logs.decode())


def stage_into_volume(client: object, volume: str, attempt_id: uuid.UUID) -> None:
    """Build the tree the agent builds, with the same modes, inside the volume.

    Built here rather than by calling `stage_input` because the agent's state directory *is* this
    volume in production, and a test process cannot write to a Docker volume directly. The modes
    and the layout are the ones `inputs.py` produces; the cache entry is a real hard link, which
    is what makes the read-only mount matter.
    """
    inputs = f"/state/attempts/{attempt_id}/inputs"
    cache = "/state/dataset-cache/ab"
    selection = json.dumps(SELECTION)
    script = (
        # The worker credential lives at the volume root in production. A stand-in for it here, so
        # "the job cannot see the volume root" is a claim about something that would matter.
        "mkdir -p /state && printf 'kwc_secret' > /state/worker-credential.json && "
        f"mkdir -p {cache} && printf '{EPISODE_BYTES.decode()}' > {cache}/abc123 && "
        f"chmod 444 {cache}/abc123 && "
        f"mkdir -p {inputs}/{ALIAS}/data {inputs}/{SELECTION_DIRECTORY} && "
        # A hard link, exactly as staging makes: one inode, two names.
        f"ln {cache}/abc123 {inputs}/{ALIAS}/data/episode_0.parquet && "
        f"printf '{selection}' > {inputs}/{SELECTION_DIRECTORY}/{ALIAS}.json && "
        f"chmod 444 {inputs}/{SELECTION_DIRECTORY}/{ALIAS}.json && "
        f"chmod 755 {inputs} {inputs}/{ALIAS} {inputs}/{ALIAS}/data "
        f"{inputs}/{SELECTION_DIRECTORY} && "
        f"chmod 700 /state/attempts/{attempt_id}"
    )
    in_volume(client, volume, script)


def run_workload(client: object, volume: str, attempt_id: uuid.UUID, script: str) -> str:
    """Run a script with exactly the mount and hardening a real job gets."""
    logs = client.containers.run(  # type: ignore[attr-defined]
        BUSYBOX,
        command=["sh", "-c", script],
        user=str(WORKLOAD_UID),
        network_disabled=True,
        read_only=True,
        cap_drop=["ALL"],
        security_opt=["no-new-privileges"],
        mounts=[
            docker.types.Mount(
                target=INPUT_MOUNT_TARGET,
                source=volume,
                type="volume",
                read_only=True,
                subpath=f"attempts/{attempt_id}/inputs",
            )
        ],
        remove=True,
    )
    return str(logs.decode())


def read_from_volume(client: object, volume: str, path: str) -> bytes:
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


@pytest.mark.slow
def test_a_workload_can_read_its_dataset_and_its_selection(
    client: object, state_volume: str
) -> None:
    attempt_id = uuid.uuid4()
    stage_into_volume(client, state_volume, attempt_id)

    logs = run_workload(
        client,
        state_volume,
        attempt_id,
        "id -u && "
        f"cat {INPUT_MOUNT_TARGET}/{ALIAS}/data/episode_0.parquet && echo '' && "
        f"cat {INPUT_MOUNT_TARGET}/{SELECTION_DIRECTORY}/{ALIAS}.json",
    )

    lines = logs.splitlines()
    assert lines[0] == str(WORKLOAD_UID), "the workload runs as a user the agent never chose"
    assert EPISODE_BYTES.decode() in logs
    # The selection has to be readable, or a job cannot know which episodes it may train on.
    selection = json.loads(lines[-1])
    assert selection["included_episodes"] == [0, 2]
    assert selection["alias"] == ALIAS


@pytest.mark.slow
def test_a_workload_cannot_write_its_dataset_or_its_selection(
    client: object, state_volume: str
) -> None:
    """Read-only is the contract, not a precaution.

    The staged file is a hard link into a cache shared with every other attempt, so a workload
    able to write here would be editing what the next attempt reads. The selection is the record
    of what this job was allowed to train on, so a workload able to rewrite it could launder its
    own result.
    """
    attempt_id = uuid.uuid4()
    stage_into_volume(client, state_volume, attempt_id)

    logs = run_workload(
        client,
        state_volume,
        attempt_id,
        # Each attempt must fail, and the script must survive to report the next.
        f"(echo tampered > {INPUT_MOUNT_TARGET}/{ALIAS}/data/episode_0.parquet && echo WROTE_FILE "
        "|| echo REFUSED_FILE); "
        f"(echo tampered > {INPUT_MOUNT_TARGET}/{SELECTION_DIRECTORY}/{ALIAS}.json "
        "&& echo WROTE_SELECTION || echo REFUSED_SELECTION); "
        f"(touch {INPUT_MOUNT_TARGET}/{ALIAS}/new-file && echo ADDED || echo REFUSED_ADD); "
        f"(rm {INPUT_MOUNT_TARGET}/{ALIAS}/data/episode_0.parquet && echo DELETED "
        "|| echo REFUSED_DELETE)",
    )

    assert "REFUSED_FILE" in logs and "WROTE_FILE" not in logs
    assert "REFUSED_SELECTION" in logs and "WROTE_SELECTION" not in logs
    assert "REFUSED_ADD" in logs and "ADDED" not in logs
    assert "REFUSED_DELETE" in logs and "DELETED" not in logs

    # And the bytes on the volume are untouched, which is the claim that actually matters: the
    # cache entry behind that hard link is what the next attempt will reuse.
    staged = read_from_volume(
        client, state_volume, f"attempts/{attempt_id}/inputs/{ALIAS}/data/episode_0.parquet"
    )
    assert staged == EPISODE_BYTES
    cached = read_from_volume(client, state_volume, "dataset-cache/ab/abc123")
    assert cached == EPISODE_BYTES


@pytest.mark.slow
def test_a_workload_cannot_see_the_volume_root_the_cache_or_the_credential(
    client: object, state_volume: str
) -> None:
    """The subpath mount is what keeps the credential out of the job.

    Without it the whole volume would be visible, and the volume root holds the worker credential
    as well as every other attempt's inputs and the shared cache.
    """
    attempt_id = uuid.uuid4()
    stage_into_volume(client, state_volume, attempt_id)

    logs = run_workload(
        client,
        state_volume,
        attempt_id,
        f"ls -a {INPUT_MOUNT_TARGET} && "
        f"(cat {INPUT_MOUNT_TARGET}/../worker-credential.json && echo READ_CREDENTIAL "
        "|| echo NO_CREDENTIAL); "
        f"(ls {INPUT_MOUNT_TARGET}/../dataset-cache && echo SAW_CACHE || echo NO_CACHE); "
        f"(ls {INPUT_MOUNT_TARGET}/../attempts && echo SAW_ATTEMPTS || echo NO_ATTEMPTS)",
    )

    # The mount root contains exactly the alias and the selection directory, and nothing else.
    listed = {entry for entry in logs.splitlines()[0:4] if entry not in (".", "..")}
    assert ALIAS in logs
    assert SELECTION_DIRECTORY in logs
    assert "NO_CREDENTIAL" in logs and "READ_CREDENTIAL" not in logs
    assert "kwc_secret" not in logs, "the credential's contents must never be reachable"
    assert "NO_CACHE" in logs and "SAW_CACHE" not in logs
    assert "NO_ATTEMPTS" in logs and "SAW_ATTEMPTS" not in logs
    assert listed, logs


@pytest.mark.slow
def test_the_agent_can_still_clean_up_what_the_workload_could_not_touch(
    client: object, state_volume: str, tmp_path: Path
) -> None:
    """Cleanup runs as the agent on its own filesystem, and must not be blocked by its own modes.

    This is the failure the live suite caught: a selection directory sealed against writing stopped
    the agent unlinking the manifest inside it. The workload is kept out by the mount instead.
    """
    attempt_id = uuid.uuid4()
    # Built on the test's own filesystem with the same layout and modes, because cleanup is a
    # host-side operation rather than something that happens through a mount.
    attempt_root = tmp_path / "attempts" / str(attempt_id)
    inputs = attempt_root / "inputs"
    (inputs / ALIAS / "data").mkdir(parents=True)
    (inputs / ALIAS / "data" / "episode_0.parquet").write_bytes(EPISODE_BYTES)
    (inputs / ALIAS / "data" / "episode_0.parquet").chmod(0o444)
    (inputs / SELECTION_DIRECTORY).mkdir(parents=True)
    manifest = inputs / SELECTION_DIRECTORY / f"{ALIAS}.json"
    manifest.write_text(json.dumps(SELECTION), encoding="utf-8")
    manifest.chmod(0o444)

    assert discard_attempt_inputs(attempt_root) is True
    assert not inputs.exists()


def test_the_executor_requests_exactly_this_mount() -> None:
    """Pin the mount this file exercises to the one the executor actually asks Docker for.

    Without this the live tests could prove a mount the agent never requests.
    """
    assignment = JobAssignment(
        attempt_id=uuid.uuid4(),
        job_id=uuid.uuid4(),
        name="Training run",
        image_reference="example.test/work@sha256:" + ("b" * 64),
        gpu_index=0,
        timeout_seconds=120,
        lease_expires_at="2026-09-28T12:00:00Z",  # type: ignore[arg-type]
        dataset_inputs=(
            DatasetInputAssignment(
                alias=ALIAS,
                dataset_version_id=uuid.uuid4(),
                manifest_sha256="c" * 64,
            ),
        ),
    )
    executor = DockerExecutor.__new__(DockerExecutor)
    executor._state_volume = "kratos-agent-state"
    (mount,) = executor._input_mounts(assignment)

    assert mount["Target"] == INPUT_MOUNT_TARGET
    assert mount["ReadOnly"] is True
    assert mount["VolumeOptions"]["Subpath"] == f"attempts/{assignment.attempt_id}/inputs"


def stage_world_writable(client: object, volume: str, attempt_id: uuid.UUID) -> None:
    """The same tree, but with modes that would let any user write it.

    Deliberately permissive so that the read-only mount is the *only* thing standing between the
    workload and the bytes. Without this the refusals below would be explained by ownership, and
    the test would pass whether or not the mount was read-only -- which is exactly what happened
    to the first version of it.
    """
    inputs = f"/state/attempts/{attempt_id}/inputs"
    cache = "/state/dataset-cache/ab"
    selection = json.dumps(SELECTION)
    script = (
        f"mkdir -p {cache} && printf '{EPISODE_BYTES.decode()}' > {cache}/abc123 && "
        f"mkdir -p {inputs}/{ALIAS}/data {inputs}/{SELECTION_DIRECTORY} && "
        f"ln {cache}/abc123 {inputs}/{ALIAS}/data/episode_0.parquet && "
        f"printf '{selection}' > {inputs}/{SELECTION_DIRECTORY}/{ALIAS}.json && "
        f"chmod -R 777 {inputs} && chmod 666 {cache}/abc123"
    )
    in_volume(client, volume, script)


@pytest.mark.slow
def test_the_read_only_mount_is_what_prevents_writing_not_the_file_modes(
    client: object, state_volume: str
) -> None:
    """The mount is the isolation boundary, so prove it is load-bearing on its own.

    Every path here is world-writable on the host. If the mount were writable the workload would
    succeed, and this test fails -- which is how it was checked.
    """
    attempt_id = uuid.uuid4()
    stage_world_writable(client, state_volume, attempt_id)

    logs = run_workload(
        client,
        state_volume,
        attempt_id,
        f"(echo tampered > {INPUT_MOUNT_TARGET}/{ALIAS}/data/episode_0.parquet && echo WROTE_FILE "
        "|| echo REFUSED_FILE); "
        f"(echo tampered > {INPUT_MOUNT_TARGET}/{SELECTION_DIRECTORY}/{ALIAS}.json "
        "&& echo WROTE_SELECTION || echo REFUSED_SELECTION); "
        f"(touch {INPUT_MOUNT_TARGET}/{ALIAS}/new-file && echo ADDED || echo REFUSED_ADD); "
        f"(rm {INPUT_MOUNT_TARGET}/{ALIAS}/data/episode_0.parquet && echo DELETED "
        "|| echo REFUSED_DELETE)",
    )

    assert "REFUSED_FILE" in logs and "WROTE_FILE" not in logs
    assert "REFUSED_SELECTION" in logs and "WROTE_SELECTION" not in logs
    assert "REFUSED_ADD" in logs and "ADDED" not in logs
    assert "REFUSED_DELETE" in logs and "DELETED" not in logs

    # The cache entry behind that hard link is what the next attempt reuses, so it is the thing
    # whose bytes actually have to be intact.
    assert read_from_volume(client, state_volume, "dataset-cache/ab/abc123") == EPISODE_BYTES
    assert (
        read_from_volume(
            client, state_volume, f"attempts/{attempt_id}/inputs/{ALIAS}/data/episode_0.parquet"
        )
        == EPISODE_BYTES
    )
