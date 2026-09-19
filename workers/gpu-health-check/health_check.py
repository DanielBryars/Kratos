"""Execute and verify a small CUDA matrix operation without CPU fallback."""

import json
import sys
from datetime import UTC, datetime
from time import perf_counter
from typing import Any

import cupy as cp
import numpy as np

MATRIX_SIZE = 512


def _cuda_version(value: int) -> str:
    major = value // 1000
    minor = (value % 1000) // 10
    return f"{major}.{minor}"


def run_check() -> dict[str, Any]:
    if cp.cuda.runtime.getDeviceCount() < 1:
        raise RuntimeError("CUDA reported no GPU devices")

    device = cp.cuda.Device(0)
    with device:
        properties = cp.cuda.runtime.getDeviceProperties(device.id)
        host_left = np.arange(MATRIX_SIZE * MATRIX_SIZE, dtype=np.float32).reshape(
            MATRIX_SIZE, MATRIX_SIZE
        )
        left = cp.asarray(host_left)
        identity = cp.asarray(np.eye(MATRIX_SIZE, dtype=np.float32))

        started = perf_counter()
        result = cp.matmul(left, identity)
        device.synchronize()
        duration_ms = (perf_counter() - started) * 1000

        host_result = cp.asnumpy(result)
        max_absolute_error = float(np.max(np.abs(host_result - host_left)))
        if max_absolute_error != 0:
            raise RuntimeError(f"GPU result verification failed: error={max_absolute_error}")

        device_name = properties["name"]
        if isinstance(device_name, bytes):
            device_name = device_name.decode("utf-8")

        return {
            "schema_version": "1.0",
            "status": "healthy",
            "checked_at": datetime.now(UTC).isoformat(),
            "device_index": device.id,
            "device_name": device_name,
            "operation": "float32 matrix multiplication by identity",
            "matrix_size": MATRIX_SIZE,
            "max_absolute_error": max_absolute_error,
            "duration_ms": round(duration_ms, 3),
            "cuda_driver_api_version": _cuda_version(cp.cuda.runtime.driverGetVersion()),
            "cuda_runtime_version": _cuda_version(cp.cuda.runtime.runtimeGetVersion()),
        }


def main() -> int:
    try:
        result = run_check()
    except Exception as error:  # noqa: BLE001 - container boundary must return structured failure
        print(
            json.dumps(
                {
                    "schema_version": "1.0",
                    "status": "unhealthy",
                    "checked_at": datetime.now(UTC).isoformat(),
                    "error_type": type(error).__name__,
                    "detail": str(error),
                }
            )
        )
        return 1

    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
