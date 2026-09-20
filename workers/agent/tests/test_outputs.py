import os
from pathlib import Path

import pytest

from kratos_agent.models import JobOutputRequirement
from kratos_agent.outputs import OutputError, build_manifest, valid_logical_path


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


def test_manifest_records_length_and_both_checksums_in_path_order(tmp_path: Path) -> None:
    write(tmp_path, "metrics/final.json", b"")
    write(tmp_path, "checkpoints/model.pt", b"123456789")

    manifest = build_manifest(
        tmp_path, [requirement("metrics/final.json"), requirement("checkpoints/model.pt")]
    )

    assert [file.logical_path for file in manifest] == [
        "checkpoints/model.pt",
        "metrics/final.json",
    ]
    model, metrics = manifest
    assert model.byte_length == 9
    assert model.sha256 == "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225"
    # The CRC-32C check value for "123456789" is 0xE3069283.
    assert model.crc32c == "4waSgw=="
    assert (metrics.byte_length, metrics.crc32c) == (0, "AAAAAA==")


def test_absent_optional_output_is_omitted(tmp_path: Path) -> None:
    write(tmp_path, "model.pt", b"weights")

    manifest = build_manifest(
        tmp_path, [requirement("model.pt"), requirement("samples.txt", mandatory=False)]
    )

    assert [file.logical_path for file in manifest] == ["model.pt"]


def test_missing_mandatory_output_is_rejected(tmp_path: Path) -> None:
    with pytest.raises(OutputError, match="mandatory output 'model.pt' was not produced"):
        build_manifest(tmp_path, [requirement("model.pt")])


def test_undeclared_output_is_rejected(tmp_path: Path) -> None:
    write(tmp_path, "model.pt", b"weights")
    write(tmp_path, "scratch/cache.bin", b"cache")

    with pytest.raises(OutputError, match="'scratch/cache.bin' was not declared"):
        build_manifest(tmp_path, [requirement("model.pt")])


def test_output_larger_than_its_declared_limit_is_rejected(tmp_path: Path) -> None:
    write(tmp_path, "model.pt", b"x" * 11)

    with pytest.raises(OutputError, match="exceeds its size limit"):
        build_manifest(tmp_path, [requirement("model.pt", max_bytes=10)])


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


@pytest.mark.skipif(not hasattr(os, "mkfifo"), reason="named pipes require a POSIX host")
def test_named_pipe_is_rejected_without_being_opened(tmp_path: Path) -> None:
    os.mkfifo(tmp_path / "model.pt")

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
