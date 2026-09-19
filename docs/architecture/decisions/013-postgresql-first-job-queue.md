# ADR-013 — PostgreSQL-backed first job queue

**Status:** Accepted for the first R0.2 slice  
**Date:** 2026-09-19

## Context

Kratos already stores worker identity, liveness and operator state in PostgreSQL. The first scheduled
workload needs durable ordering, atomic worker allocation and replay-safe assignment, but it does not
yet need broker-scale throughput or a continuously running scheduler service.

## Decision

The first R0.2 queue SHALL use PostgreSQL. An authenticated operator SHALL submit an immutable OCI
image reference, a name and a bounded runtime. The control plane SHALL persist the request before it
can be assigned.

An approved, idle worker with a verified healthy GPU SHALL claim the oldest compatible queued job as
part of its heartbeat. Selection SHALL use a row lock with `SKIP LOCKED`; the job, attempt and worker
state changes SHALL commit atomically. A repeated heartbeat SHALL return the same active attempt and
SHALL NOT create a second attempt.

Every assignment SHALL contain a hard lease deadline. The initial scheduler SHALL NOT reassign an
expired attempt automatically because doing so could create duplicate GPU execution after a network
partition. Recovery and checkpoint-aware reassignment remain R0.4 work.

The worker SHALL run the immutable image in a sibling Linux container with no network, a read-only
root filesystem, dropped capabilities, `no-new-privileges`, bounded CPU, memory, processes and runtime,
and one explicitly selected GPU. It SHALL retain the named stopped container until the control plane
acknowledges the result, allowing a restarted agent to report the same result without rerunning work.

The first slice SHALL expose stdout, stderr, exit status and terminal state. It SHALL NOT claim
training provenance, dataset versioning, artefact storage, credits or ETA; those capabilities are
added in the releases that own them.

## Consequences

This keeps assignment and state transitions in the database that already defines worker authority,
and avoids operating another durable service during the first end-to-end run. Heartbeat polling adds
up to one heartbeat interval of queue latency. PostgreSQL queue contention and scheduler separation
SHALL be revisited when measured load or richer placement policy justifies it.
