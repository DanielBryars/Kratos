import hashlib
import os
import shutil
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from datetime import UTC, datetime
from pathlib import Path
from uuid import UUID

import pytest
from pydantic import ValidationError

from kratos_agent import outputs
from kratos_agent.models import JobAssignment, JobOutputRequirement, valid_logical_path
from kratos_agent.outputs import DESCRIPTOR_WALK_SUPPORTED, OutputError, build_manifest

# os.mkfifo does not exist on Windows, where the collector refuses to run anyway.
_mkfifo: Callable[[str], None] | None = getattr(os, "mkfifo", None)

needs_descriptor_walk = pytest.mark.skipif(
    not DESCRIPTOR_WALK_SUPPORTED,
    reason="output collection requires descriptor-relative file access",
)


def requirement(
    logical_path: str, *, mandatory: bool = True, max_bytes: int = 1024
) -> JobOutputRequirement:
    return JobOutputRequirement(
        logical_path=logical_path,
        role="model",
        media_type="application/octet-stream",
        mandatory=mandatory,
        max_bytes=max_bytes,
    )


def write(root: Path, logical_path: str, content: bytes) -> Path:
    path = root / logical_path
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    return path


@needs_descriptor_walk
def test_manifest_records_length_and_both_checksums_in_path_order(tmp_path: Path) -> None:
    write(tmp_path, "metrics/final.json", b"")
    write(tmp_path, "checkpoints/model.pt", b"123456789")

    manifest = build_manifest(
        tmp_path, [requirement("metrics/final.json"), requirement("checkpoints/model.pt")]
    )

    assert [output.file.logical_path for output in manifest] == [
        "checkpoints/model.pt",
        "metrics/final.json",
    ]
    model, metrics = (output.file for output in manifest)
    assert model.byte_length == 9
    assert model.sha256 == "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225"
    # The CRC-32C check value for "123456789" is 0xE3069283.
    assert model.crc32c == "4waSgw=="
    assert (metrics.byte_length, metrics.crc32c) == (0, "AAAAAA==")


@needs_descriptor_walk
def test_absent_optional_output_is_omitted(tmp_path: Path) -> None:
    write(tmp_path, "model.pt", b"weights")

    manifest = build_manifest(
        tmp_path, [requirement("model.pt"), requirement("samples.txt", mandatory=False)]
    )

    assert [output.file.logical_path for output in manifest] == ["model.pt"]


@needs_descriptor_walk
def test_missing_mandatory_output_is_rejected(tmp_path: Path) -> None:
    with pytest.raises(OutputError, match="mandatory output 'model.pt' was not produced"):
        build_manifest(tmp_path, [requirement("model.pt")])


@needs_descriptor_walk
def test_undeclared_output_is_rejected(tmp_path: Path) -> None:
    write(tmp_path, "model.pt", b"weights")
    write(tmp_path, "scratch/cache.bin", b"cache")

    with pytest.raises(OutputError, match="'scratch/cache.bin' was not declared"):
        build_manifest(tmp_path, [requirement("model.pt")])


@needs_descriptor_walk
def test_output_larger_than_its_declared_limit_is_rejected(tmp_path: Path) -> None:
    write(tmp_path, "model.pt", b"x" * 11)

    with pytest.raises(OutputError, match="exceeds its size limit"):
        build_manifest(tmp_path, [requirement("model.pt", max_bytes=10)])


@needs_descriptor_walk
def test_symbolic_link_is_rejected_without_being_followed(tmp_path: Path) -> None:
    outputs = tmp_path / "outputs"
    outputs.mkdir()
    secret = write(tmp_path, "agent-state.json", b"credential")
    try:
        (outputs / "model.pt").symlink_to(secret)
    except OSError:
        pytest.skip("symbolic links are unavailable on this host")

    with pytest.raises(OutputError, match="'model.pt' is not a regular file"):
        build_manifest(outputs, [requirement("model.pt")])


@needs_descriptor_walk
def test_symbolic_link_to_a_directory_is_not_traversed(tmp_path: Path) -> None:
    outputs = tmp_path / "outputs"
    outputs.mkdir()
    write(tmp_path, "elsewhere/model.pt", b"weights")
    try:
        (outputs / "checkpoints").symlink_to(tmp_path / "elsewhere", target_is_directory=True)
    except OSError:
        pytest.skip("symbolic links are unavailable on this host")

    with pytest.raises(OutputError, match="'checkpoints' is not a regular file"):
        build_manifest(outputs, [requirement("checkpoints/model.pt")])


@needs_descriptor_walk
def test_hard_link_alias_is_rejected(tmp_path: Path) -> None:
    outputs = tmp_path / "outputs"
    outputs.mkdir()
    secret = write(tmp_path, "agent-state.json", b"credential")
    try:
        os.link(secret, outputs / "model.pt")
    except OSError:
        pytest.skip("hard links are unavailable on this host")

    with pytest.raises(OutputError, match="more than one hard link"):
        build_manifest(outputs, [requirement("model.pt")])


@pytest.mark.skipif(_mkfifo is None, reason="named pipes require a POSIX host")
def test_named_pipe_is_rejected_without_being_opened(tmp_path: Path) -> None:
    assert _mkfifo is not None
    _mkfifo(str(tmp_path / "model.pt"))

    with pytest.raises(OutputError, match="'model.pt' is not a regular file"):
        build_manifest(tmp_path, [requirement("model.pt")])


@pytest.mark.skipif(os.name != "posix", reason="these names cannot be created on this host")
@pytest.mark.parametrize("name", ["back\\slash.pt", "line\nbreak.pt", "x" * 241])
def test_file_name_the_control_plane_would_refuse_is_rejected(tmp_path: Path, name: str) -> None:
    try:
        (tmp_path / name).write_bytes(b"weights")
    except OSError:
        pytest.skip("the filesystem cannot hold this name")

    with pytest.raises(OutputError, match="is not permitted"):
        build_manifest(tmp_path, [])


@pytest.mark.parametrize(
    ("path", "valid"),
    [
        ("model.pt", True),
        ("checkpoints/epoch-1/model.pt", True),
        ("é" * 120, True),
        ("é" * 121, False),
        ("", False),
        ("/model.pt", False),
        ("checkpoints//model.pt", False),
        ("checkpoints/", False),
        ("./model.pt", False),
        ("checkpoints/../model.pt", False),
        ("checkpoints\\model.pt", False),
        ("model\x00.pt", False),
        ("model\x7f.pt", False),
        ("model\x85.pt", False),
        ("model\udc80.pt", False),
    ],
)
def test_logical_path_rules_match_the_control_plane(path: str, valid: bool) -> None:
    assert valid_logical_path(path) is valid


@needs_descriptor_walk
def test_outputs_are_sealed_and_identified_for_the_uploader(tmp_path: Path) -> None:
    path = write(tmp_path, "checkpoints/model.pt", b"weights")

    (output,) = build_manifest(tmp_path, [requirement("checkpoints/model.pt")])

    status = path.stat()
    assert (output.identity.inode, output.identity.byte_length) == (status.st_ino, 7)
    assert output.identity.changed_ns == status.st_ctime_ns
    assert status.st_mode & 0o777 == 0o400
    assert path.parent.stat().st_mode & 0o777 == 0o500
    path.parent.chmod(0o700)


@needs_descriptor_walk
def test_output_this_agent_may_not_seal_is_still_collected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A job image may write its outputs as another user, so the agent cannot chmod them.
    write(tmp_path, "model.pt", b"123456789")

    def refuse(descriptor: int, mode: int) -> None:
        raise PermissionError(1, "Operation not permitted")

    monkeypatch.setattr(outputs, "_fchmod", refuse)

    (output,) = build_manifest(tmp_path, [requirement("model.pt")])

    assert output.file.crc32c == "4waSgw=="
    assert output.identity.byte_length == 9


@contextmanager
def swapped_before_open(
    monkeypatch: pytest.MonkeyPatch, target: str, swap: Callable[[], None]
) -> Iterator[None]:
    """Replace ``target`` at the last moment: after it was inspected, before it is opened."""
    real_open = os.open

    def racing_open(path: str, flags: int, mode: int = 0o777, *, dir_fd: int | None = None) -> int:
        if path == target:
            swap()
        return real_open(path, flags, mode, dir_fd=dir_fd)

    monkeypatch.setattr(os, "open", racing_open)
    yield


@needs_descriptor_walk
def test_directory_swapped_for_a_symlink_after_inspection_is_not_traversed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    tree = tmp_path / "outputs"
    write(tree, "checkpoints/model.pt", b"weights")
    write(tmp_path, "elsewhere/model.pt", b"another tenant's credential")

    def swap() -> None:
        tree.chmod(0o700)  # the walk has already sealed the parent
        shutil.rmtree(tree / "checkpoints")
        (tree / "checkpoints").symlink_to(tmp_path / "elsewhere", target_is_directory=True)

    with (
        swapped_before_open(monkeypatch, "checkpoints", swap),
        pytest.raises(OutputError, match="'checkpoints' could not be opened as a directory"),
    ):
        build_manifest(tree, [requirement("checkpoints/model.pt")])


@needs_descriptor_walk
def test_output_root_that_is_a_symlink_is_refused(tmp_path: Path) -> None:
    write(tmp_path, "elsewhere/model.pt", b"weights")
    (tmp_path / "outputs").symlink_to(tmp_path / "elsewhere", target_is_directory=True)

    with pytest.raises(OutputError, match="the output directory could not be opened"):
        build_manifest(tmp_path / "outputs", [requirement("model.pt")])


@needs_descriptor_walk
def test_file_swapped_for_a_symlink_after_inspection_is_not_followed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    tree = tmp_path / "outputs"
    write(tree, "model.pt", b"weights")
    secret = write(tmp_path, "agent-state.json", b"credential")

    def swap() -> None:
        tree.chmod(0o700)  # the walk has already sealed the parent
        (tree / "model.pt").unlink()
        (tree / "model.pt").symlink_to(secret)

    with (
        swapped_before_open(monkeypatch, "model.pt", swap),
        pytest.raises(OutputError, match="'model.pt' could not be opened"),
    ):
        build_manifest(tree, [requirement("model.pt")])


@needs_descriptor_walk
def test_file_swapped_for_a_named_pipe_neither_blocks_nor_passes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    write(tmp_path, "model.pt", b"weights")

    def swap() -> None:
        tmp_path.chmod(0o700)  # the walk has already sealed the parent
        (tmp_path / "model.pt").unlink()
        assert _mkfifo is not None
        _mkfifo(str(tmp_path / "model.pt"))

    with (
        swapped_before_open(monkeypatch, "model.pt", swap),
        pytest.raises(OutputError, match="'model.pt' is not a regular file"),
    ):
        build_manifest(tmp_path, [requirement("model.pt")])


@needs_descriptor_walk
def test_same_size_rewrite_while_hashing_is_detected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = write(tmp_path, "model.pt", b"original")
    os.utime(path, ns=(1_000_000_000, 1_000_000_000))
    real_sha256 = hashlib.sha256

    class RewritingDigest:
        def __init__(self) -> None:
            self.inner = real_sha256()

        def update(self, chunk: bytes) -> None:
            path.chmod(0o600)
            path.write_bytes(b"tampered")
            self.inner.update(chunk)

        def hexdigest(self) -> str:
            return str(self.inner.hexdigest())

    monkeypatch.setattr(hashlib, "sha256", RewritingDigest)

    with pytest.raises(OutputError, match="changed while it was being read"):
        build_manifest(tmp_path, [requirement("model.pt")])


def assignment_with(requirements: list[dict[str, object]] | None) -> JobAssignment:
    payload: dict[str, object] = {
        "attempt_id": str(UUID(int=1)),
        "job_id": str(UUID(int=2)),
        "name": "Training",
        "image_reference": "example.test/work@sha256:" + ("a" * 64),
        "gpu_index": 0,
        "timeout_seconds": 120,
        "lease_expires_at": datetime(2026, 9, 20, tzinfo=UTC).isoformat(),
    }
    if requirements is not None:
        payload["output_requirements"] = requirements
    return JobAssignment.model_validate(payload)


def declared(logical_path: str = "model.pt", **changes: object) -> dict[str, object]:
    return {
        "logical_path": logical_path,
        "role": "model",
        "media_type": "application/octet-stream",
        "mandatory": True,
        "max_bytes": 1024,
    } | changes


def test_assignment_without_output_requirements_remains_valid() -> None:
    assert assignment_with(None).output_requirements == ()


def test_assignment_accepts_requirements_the_control_plane_accepts() -> None:
    accepted = assignment_with(
        [
            declared("é" * 120, media_type="application/vnd.kratos.model+json"),
            declared("metrics.json", max_bytes=5 * 1024**3),
            declared("samples.bin", max_bytes=5 * 1024**3 - 1024, mandatory=False),
        ]
    )

    assert len(accepted.output_requirements) == 3


@pytest.mark.parametrize(
    "requirements",
    [
        [declared("é" * 121)],
        [declared("checkpoints/../model.pt")],
        [declared(media_type="application")],
        [declared(media_type="application/octet stream")],
        [declared(media_type="application/" + "x" * 120)],
        [declared(role="Model")],
        [declared(role="m" * 33)],
        [declared(max_bytes=0)],
        [declared(max_bytes=5 * 1024**3 + 1)],
        [declared("model.pt"), declared("model.pt")],
        [declared(f"part-{index}.bin", max_bytes=5 * 1024**3) for index in range(3)],
        [declared(f"part-{index}.bin") for index in range(101)],
    ],
)
def test_assignment_refuses_requirements_the_control_plane_refuses(
    requirements: list[dict[str, object]],
) -> None:
    with pytest.raises(ValidationError):
        assignment_with(requirements)
