# R0.1 worker liveness evidence — local-gpu-01

**Observed:** 2026-09-19 20:32–20:38 Europe/London
**Status:** Passed for the first home worker; the second worker and revocation remain outstanding

## Environment

| Item | Observed value |
|---|---|
| Worker alias | `local-gpu-01` |
| Worker identity | Stable across container replacement; exact identifier retained privately |
| Agent instance identity | Stable across container replacement; exact identifier retained privately |
| Agent image | `ghcr.io/danielbryars/kratos-agent@sha256:9d1c9f096cbf2a95bde82213321afac54c169eec6d721eb4ebc54a0284f03921` |
| Persistent state volume | `kratos-agent-state` |
| GPU shown by fleet console | NVIDIA GeForce RTX 5090, 31.8 GB |
| Compute group | `Home` |
| Durable state | `idle` |

The worker credential remained inside the Docker volume and was not displayed, copied or moved out
of that volume during the test.

## Disconnect and reconnect

The last pre-disconnect heartbeat was accepted at 20:32:25. The agent container was stopped at
20:32:28. The authenticated fleet console subsequently showed:

| Observation | Console result |
|---|---|
| 20:34:11 | `stale`, while durable state remained `idle` |
| 20:37:36 | `offline`, while durable state remained `idle` |

This matches the protocol thresholds of stale after 90 seconds and offline after five minutes. The
console continued to show the last accepted heartbeat and retained the `Home` membership throughout.

The stopped container was removed and recreated from the same immutable image with the same state
volume. It used the physical host name inside the container, correcting the earlier container-generated
capability hostname without publishing that name in this evidence.
The control plane accepted a new heartbeat at 20:37:49 and the console returned to `online` without
registration or operator approval. The agent retained the same worker and instance identifiers and
continued at heartbeat sequence 279. The replacement container was running with zero restarts.

## Remaining R0.1 evidence

- Repeat registration, capability, disconnect and reconnect evidence on the second home worker.
- Exercise credential revocation after another worker is available, then prove the revoked credential
  cannot heartbeat and the machine can radio in as a new registration.
