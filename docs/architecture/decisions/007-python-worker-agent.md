# ADR-007 — Python worker agent with outbound HTTPS

**Status:** Accepted  
**Date:** 2026-09-19

## Context

Kratos needs an agent inside each GPU worker's Linux execution environment. The agent must inspect
the same GPU and runtime surface used by training containers, enrol without inbound network access
and report capabilities without overstating their verification state.

## Decision

The initial worker agent SHALL use Python 3.12 or later, Pydantic models and a versioned outbound
HTTPS protocol. `uv` SHALL lock its dependencies and run its development tools. The R0.1 agent
SHALL use periodic heartbeats rather than a persistent connection.

GPU discovery SHALL use `nvidia-smi` from the Linux execution environment. Discovery and a real GPU
computation check SHALL remain separate results. The computation check will run in a controlled,
versioned GPU container before a worker becomes eligible.

The agent SHALL NOT require an inbound listener. A single-use bootstrap credential will be exchanged
for a unique, scoped and revocable worker credential. Credential transport, storage and logging must
follow the [worker protocol](../../protocol/worker-v1.md).

## Alternatives

| Option | Assessment |
|---|---|
| Go agent | Compact static distribution, but it duplicates more GPU and ML environment integration work. |
| .NET agent | Strong tooling and service support, but it is not the selected runtime for worker-side ML integration. |
| WebSocket or gRPC stream | Useful for low-latency assignment later, but adds connection lifecycle complexity before R0.1 needs it. |
| Inbound control connection | Conflicts with home-network deployment and would require firewall or port-forwarding changes. |

## Consequences

- Agent code can share Python-native GPU/runtime inspection libraries with later health checks.
- The agent needs its own locked dependency set, linting, type checking and tests.
- Periodic HTTPS requests add modest request overhead and make state freshness interval-based.
- Python packaging is larger than a small static binary; the agent container remains independently
  versioned from training workload containers.

