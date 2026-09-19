"""Command-line entry point for the Kratos worker agent."""

import argparse
import json
import socket
import sys
from functools import partial
from pathlib import Path

import httpx
from docker.errors import DockerException

from kratos_agent.capabilities import collect_capabilities
from kratos_agent.executor import DockerExecutor, ExecutorError
from kratos_agent.models import GpuHealth, GpuHealthEvidence, GpuHealthStatus
from kratos_agent.protocol import ControlPlaneError, WorkerProtocolClient
from kratos_agent.runner import AgentRunner


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="kratos-agent")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("inspect", help="print detected capabilities as JSON")
    health = commands.add_parser("health-check", help="run the controlled GPU health image")
    health.add_argument("--image", required=True, help="immutable image digest or local image ID")
    health.add_argument("--gpu-index", type=int, default=0)
    run = commands.add_parser("run", help="enrol if needed and send periodic heartbeats")
    run.add_argument("--control-plane", default="https://kratos.bryars.com")
    run.add_argument("--display-name", default=socket.gethostname())
    run.add_argument("--state-file", type=Path, default=Path("/var/lib/kratos-agent/state.json"))
    run.add_argument("--enrolment-credential-file", type=Path)
    run.add_argument(
        "--health-check-image",
        help="immutable GPU health-check image run once before heartbeats",
    )
    return parser


def _reported_health(evidence: GpuHealthEvidence) -> GpuHealth:
    if evidence.status is GpuHealthStatus.HEALTHY:
        detail = (
            f"GPU computation passed on {evidence.device_name} in {evidence.duration_ms:.3f} ms"
        )
    else:
        detail = evidence.detail or "GPU computation health check failed"
    return GpuHealth(status=evidence.status, detail=detail, evidence=evidence)


def main() -> int:
    args = _parser().parse_args()
    if args.command == "inspect":
        print(collect_capabilities().model_dump_json(indent=2))
        return 0
    elif args.command == "health-check":
        try:
            evidence = DockerExecutor.from_environment().run_gpu_health_check(
                image_reference=args.image,
                gpu_index=args.gpu_index,
            )
        except (DockerException, ExecutorError) as error:
            print(json.dumps({"status": "error", "detail": str(error)}))
            return 2
        print(evidence.model_dump_json(indent=2, exclude_none=True))
        return 0 if evidence.status is GpuHealthStatus.HEALTHY else 1
    elif args.command == "run":
        try:
            capability_collector = collect_capabilities
            executor = DockerExecutor.from_environment()
            if args.health_check_image:
                evidence = executor.run_gpu_health_check(image_reference=args.health_check_image)
                health = _reported_health(evidence)
                capability_collector = partial(collect_capabilities, gpu_health_override=health)
            with WorkerProtocolClient(args.control_plane) as client:
                AgentRunner(
                    client=client,
                    display_name=args.display_name,
                    state_path=args.state_file,
                    enrolment_credential_path=args.enrolment_credential_file,
                    capability_collector=capability_collector,
                    executor=executor,
                ).run()
        except KeyboardInterrupt:
            return 0
        except (
            ControlPlaneError,
            DockerException,
            ExecutorError,
            httpx.HTTPError,
            OSError,
            ValueError,
        ) as error:
            print(json.dumps({"status": "error", "detail": str(error)}))
            return 2

    return 2


if __name__ == "__main__":
    sys.exit(main())
