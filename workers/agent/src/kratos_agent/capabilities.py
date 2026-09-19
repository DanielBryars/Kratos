"""Collect capability facts visible from the agent's Linux environment."""

import csv
import os
import platform
import shutil
import subprocess
from collections.abc import Callable
from datetime import UTC, datetime
from pathlib import Path

from kratos_agent.models import (
    PROTOCOL_VERSION,
    GpuCapability,
    GpuHealth,
    GpuHealthStatus,
    WorkerCapabilities,
)

NVIDIA_SMI_QUERY = (
    "--query-gpu=index,name,memory.total,driver_version",
    "--format=csv,noheader,nounits",
)


def _memory_total_bytes(meminfo: Path = Path("/proc/meminfo")) -> int:
    try:
        for line in meminfo.read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        pass

    raise RuntimeError("unable to determine total memory from /proc/meminfo")


def _parse_nvidia_smi(output: str) -> tuple[GpuCapability, ...]:
    gpus: list[GpuCapability] = []
    for row in csv.reader(output.splitlines(), skipinitialspace=True):
        if not row:
            continue
        if len(row) != 4:
            raise ValueError("nvidia-smi returned an unexpected number of fields")
        index, name, memory_mib, driver_version = (value.strip() for value in row)
        gpus.append(
            GpuCapability(
                index=int(index),
                name=name,
                memory_total_bytes=int(memory_mib) * 1024 * 1024,
                driver_version=driver_version,
            )
        )
    return tuple(gpus)


def _detect_gpus(
    run: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run,
) -> tuple[tuple[GpuCapability, ...], GpuHealth]:
    try:
        result = run(
            ["nvidia-smi", *NVIDIA_SMI_QUERY],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
        gpus = _parse_nvidia_smi(result.stdout)
    except (FileNotFoundError, subprocess.SubprocessError, ValueError) as error:
        return (), GpuHealth(
            status=GpuHealthStatus.UNAVAILABLE,
            detail=f"NVIDIA GPU detection failed: {type(error).__name__}",
        )

    if not gpus:
        return (), GpuHealth(
            status=GpuHealthStatus.UNAVAILABLE,
            detail="nvidia-smi reported no GPU devices",
        )

    return gpus, GpuHealth(
        status=GpuHealthStatus.UNVERIFIED,
        detail="GPU devices detected; computation health check has not run",
    )


def collect_capabilities(storage_path: Path = Path("/")) -> WorkerCapabilities:
    """Collect a point-in-time report without claiming untested GPU health."""

    gpus, gpu_health = _detect_gpus()
    return WorkerCapabilities(
        protocol_version=PROTOCOL_VERSION,
        collected_at=datetime.now(UTC),
        hostname=platform.node(),
        operating_system=platform.system(),
        operating_system_version=platform.release(),
        architecture=platform.machine(),
        logical_cpu_count=os.cpu_count() or 1,
        memory_total_bytes=_memory_total_bytes(),
        storage_available_bytes=shutil.disk_usage(storage_path).free,
        python_version=platform.python_version(),
        gpus=gpus,
        gpu_health=gpu_health,
    )
