"""Command-line entry point for the Kratos worker agent."""

import argparse

from kratos_agent.capabilities import collect_capabilities


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="kratos-agent")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("inspect", help="print detected capabilities as JSON")
    return parser


def main() -> None:
    args = _parser().parse_args()
    if args.command == "inspect":
        print(collect_capabilities().model_dump_json(indent=2))


if __name__ == "__main__":
    main()
