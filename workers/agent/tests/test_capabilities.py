import subprocess
from pathlib import Path

import pytest

from kratos_agent.capabilities import (
    _detect_gpus,
    _memory_total_bytes,
    _parse_nvidia_smi,
    collect_capabilities,
)
from kratos_agent.models import GpuHealth, GpuHealthStatus


def test_parses_nvidia_smi_capabilities() -> None:
    output = "0, NVIDIA GeForce RTX 4090, 24564, 576.80\n"

    (gpu,) = _parse_nvidia_smi(output)

    assert gpu.index == 0
    assert gpu.name == "NVIDIA GeForce RTX 4090"
    assert gpu.memory_total_bytes == 24_564 * 1024 * 1024
    assert gpu.driver_version == "576.80"


def test_detection_does_not_claim_computation_health() -> None:
    def successful_run(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(
            args=["nvidia-smi"], returncode=0, stdout="0, Test GPU, 1024, 1.2.3\n"
        )

    gpus, health = _detect_gpus(successful_run)

    assert len(gpus) == 1
    assert health.status is GpuHealthStatus.UNVERIFIED


def test_detection_reports_missing_nvidia_smi() -> None:
    def missing_run(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
        raise FileNotFoundError

    gpus, health = _detect_gpus(missing_run)

    assert not gpus
    assert health.status is GpuHealthStatus.UNAVAILABLE


def test_reads_linux_memory_total(tmp_path: Path) -> None:
    meminfo = tmp_path / "meminfo"
    meminfo.write_text("MemTotal:       16384 kB\n", encoding="utf-8")

    assert _memory_total_bytes(meminfo) == 16_384 * 1024


def test_rejects_unreadable_memory_information(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError, match="unable to determine total memory"):
        _memory_total_bytes(tmp_path / "missing")


def test_computation_health_overrides_detection_result(tmp_path: Path) -> None:
    verified = GpuHealth(status=GpuHealthStatus.UNVERIFIED, detail="override")

    capabilities = collect_capabilities(storage_path=tmp_path, gpu_health_override=verified)

    assert capabilities.gpu_health is verified
