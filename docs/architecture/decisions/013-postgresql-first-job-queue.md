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

Every assignment SHALL contain a hard lease deadline. The control plane SHALL durably fail an
expired attempt before making the job eligible for another assignment. R0.2 SHALL permit at most
two attempts per job: the initial attempt and one automatic retry from the immutable job definition.
Exhausting that limit SHALL fail the job. The retry starts from scratch; checkpoint-aware resume
remains R0.4 work.

Lease reconciliation SHALL lock the job and active attempt together, SHALL preserve the database
constraint that permits only one active attempt per job, and SHALL run in bounded batches during
authenticated worker heartbeats and operator job operations until a dedicated scheduler service is
introduced. A result received after its attempt lease expired SHALL terminalise that attempt and
SHALL NOT overwrite a replacement attempt or terminal job. Reconciliation SHALL release a worker's
durable `busy` state after its attempt closes; connectivity remains derived from heartbeat age and
may independently show that worker as stale or offline.

Cancelling queued work SHALL be immediately terminal and idempotent. Cancelling assigned or running
work SHALL record `cancelling` until the worker reports that execution has stopped or its lease
expires; either event SHALL terminalise both job and attempt as `cancelled`. R0.2 has no separate
worker cancellation command, so the lease remains the hard upper bound for an unreachable worker.

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
