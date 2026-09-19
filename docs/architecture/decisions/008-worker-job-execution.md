# ADR-008 — Worker-controlled sibling job containers

**Status:** Accepted  
**Date:** 2026-09-19

## Context

The worker agent runs in the Linux execution environment on Windows and native Linux GPU hosts.
It must start immutable training images with explicit resource limits, observe their lifecycle and
recover its state after a restart. Cloud services cannot make inbound connections to home networks.

## Decision

The worker agent SHALL poll the control plane over authenticated HTTPS for assignments. An accepted
assignment SHALL contain a unique attempt and lease, an approved image digest, bounded resources and
job-scoped data and secret grants.

The initial agent SHALL use the local Docker Engine API through its Unix socket to create training
containers as siblings. Training SHALL NOT execute inside the agent container. Only the trusted agent
receives the Docker socket; training and health-check containers SHALL NOT receive it.

Before starting a container, the agent SHALL validate the assignment identity, lease, image digest,
worker state, protocol version, available GPU capacity and requested resource limits. It SHALL pull
images by digest using short-lived registry access and prepare a dedicated attempt directory.

Training containers SHALL receive only their allocated GPU devices, bounded CPU and memory, a
read-only input mount, writable output and checkpoint mounts, a constrained network and temporary
secret files. They SHALL drop Linux capabilities, set `no-new-privileges`, avoid privileged mode and
use a read-only root filesystem where the workload supports it.

Containers SHALL carry control-plane identifiers as Docker labels. After an agent restart, the agent
SHALL reconcile labelled containers with authoritative attempts before claiming new work. Lease
expiry, cancellation and shutdown SHALL use a documented grace period followed by forced
termination when necessary.

Worker data SHALL remain in the Linux filesystem or Docker volumes. High-volume training data SHALL
NOT use a Windows-mounted filesystem without a measured exception.

## Security boundary

Access to the Docker socket gives the agent effective administrative control of the Linux Docker
host. This is an explicit R0.1 trust decision for administrator-owned machines running approved
workloads. The executor API SHALL remain narrow so it can later move into a dedicated host service or
dedicated container runtime without changing the cloud assignment protocol.

## Alternatives

| Option | Assessment |
|---|---|
| Docker-in-Docker | Adds nested storage, networking and GPU-runtime complexity and does not remove the privileged boundary. |
| Host executor service | Stronger separation from the network-facing agent, but adds installation and update work before the first two hosts are validated. |
| Kubernetes on every worker | Useful at larger scale, but introduces a second scheduler and cluster operations before the two-host requirement needs them. |
| Direct host processes | Weakens reproducibility, dependency isolation and GPU workload packaging. |

## Consequences

- The first implementation remains small and works with Docker Desktop's WSL2 Linux engine.
- The agent is a high-trust component and requires stricter review, update integrity and secret
  handling than training containers.
- General untrusted code remains outside the platform scope.
- A later executor split can preserve the assignment and lifecycle interfaces.

