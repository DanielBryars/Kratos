"""Command-line entry point for the Kratos worker agent."""

import argparse
import json
import sys

from docker.errors import DockerException

from kratos_agent.capabilities import collect_capabilities
from kratos_agent.executor import DockerExecutor, ExecutorError
from kratos_agent.models import GpuHealthStatus


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="kratos-agent")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("inspect", help="print detected capabilities as JSON")
    health = commands.add_parser("health-check", help="run the controlled GPU health image")
    health.add_argument("--image", required=True, help="immutable image digest or local image ID")
    health.add_argument("--gpu-index", type=int, default=0)
    return parser


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

    return 2


if __name__ == "__main__":
    sys.exit(main())
