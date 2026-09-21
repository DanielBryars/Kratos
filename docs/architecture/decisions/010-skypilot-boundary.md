# ADR-010 — SkyPilot as a later cloud-capacity provider

**Status:** Accepted  
**Date:** 2026-09-19

Detailed implementation design: [Queue scheduling and bounded SkyPilot capacity](../queue-and-skypilot-capacity.md).

## Context

SkyPilot can provision and operate workloads across cloud providers and existing Kubernetes
clusters. Kratos must initially register two existing Docker-based home workers and owns user access,
worker identity, scheduling leases, internal credits, provenance and audit history.

Introducing SkyPilot into the home-worker path would duplicate lifecycle and scheduling authority or
require the home machines to become a Kubernetes cluster before that is otherwise needed.

## Decision

SkyPilot SHALL NOT participate in R0.1–R0.6 execution on registered home workers. Those workers SHALL
use the Kratos agent and local Docker executor defined in ADR-008.

R0.7 SHALL evaluate and, if acceptance tests support it, implement SkyPilot behind a cloud-capacity
provider interface. Kratos will remain authoritative for users, projects, job intent, budgets,
credits, provenance and the user-visible run record. The SkyPilot adapter may own provisioning,
cloud placement, interruption recovery and teardown for resources it creates.

Each provisioned resource SHALL have exactly one lifecycle owner. Terraform SHALL provision durable
platform infrastructure; it SHALL NOT attempt to manage ephemeral instances owned by SkyPilot.
Kratos SHALL import provider status and charges without representing internal credits as provider
invoices.

## Provider boundary

```text
Kratos scheduler
  ├── RegisteredWorkerProvider -> home and third-party Kratos agents
  └── SkyPilotProvider         -> temporary cloud GPU capacity
```

The adapter SHALL translate an approved Kratos attempt into a constrained SkyPilot task, retain the
external identifiers and reconcile asynchronous state. Job secrets and data access SHALL remain
scoped and time-bounded.

## Reconsider when

- R0.7 cloud GPU expansion begins;
- an existing Kubernetes fleet becomes an execution requirement; or
- measured provisioning and recovery work shows that direct provider integration is preferable.
