# 02 — Compute and Execution

[Overview](README.md) · Version 0.5

## Inventory and allocation

| ID | Requirement |
|---|---|
| CMP-001 | The platform SHALL display registered machines and compute capabilities, including GPU model and memory capacity. |
| CMP-002 | The platform SHALL display each machine's availability, allocated workloads and last contact time. |
| CMP-003 | The platform SHALL display GPU utilisation and memory usage for active GPU workers. |
| CMP-004 | The platform SHALL distinguish unreachable, idle, busy and draining machines. |
| CMP-005 | The platform SHALL label stale or unavailable measurements. |
| CMP-006 | The platform SHALL prevent allocations exceeding configured resource limits. |
| CMP-007 | The platform SHOULD display temperature, CPU utilisation, system memory usage and network throughput. |
| CMP-008 | Operators SHALL be able to disable new allocations to a worker while allowing active jobs to finish. |

## Windows and Linux hosts

GPU pass-through means that a Linux training container can execute actual GPU operations on its Windows host's supported GPU through the selected virtualisation and container stack. A particular pass-through mechanism is not prescribed by this requirements document.

| ID | Requirement |
|---|---|
| ENV-001 | Training workloads SHALL execute inside Linux containers on supported Windows and native Linux machines. |
| ENV-002 | GPU workers SHALL expose their allocated GPU devices to training containers through a supported host-to-container GPU access mechanism, including the Windows-to-Linux mechanism for Windows workers. |
| ENV-003 | The platform SHALL maintain a tested compatibility matrix covering host operating system, any Linux virtualisation layer, container runtime, GPU hardware, host driver and workload runtime. |
| ENV-004 | Worker registration SHALL verify GPU visibility and successful execution of a GPU computation inside the intended training container. |
| ENV-005 | Jobs requiring a GPU SHALL fail explicitly when GPU access is unavailable; silent CPU fallback SHALL NOT occur. |
| ENV-006 | Workload images SHALL be selected by immutable identity and their runtime dependencies SHALL be reproducible. |
| ENV-007 | CPU, memory, storage and GPU allocation limits SHALL reserve configurable capacity for use of the host outside the platform. |
| ENV-008 | The platform SHALL account for host sleep, reboot and loss of connectivity as worker interruption events. |
| ENV-009 | Training data, checkpoints and platform state SHALL persist independently of container lifetimes. |
| ENV-010 | Operators SHALL have documented installation, update and diagnostic procedures for the Windows and Linux execution stack. |
| ENV-011 | A host driver or runtime change SHALL require a successful GPU health check before that worker accepts new GPU jobs. |
| ENV-012 | Storage placement SHOULD avoid unnecessary transfers across Windows and Linux filesystem boundaries for training workloads. |
| ENV-013 | GPU allocation SHALL be exclusive by default; any sharing policy SHALL be explicit, capacity-aware and visible to affected users. |

## Distributed training — subsequent release

| ID | Requirement |
|---|---|
| DST-001 | The platform SHALL support one training run across at least two compatible GPU machines. |
| DST-002 | The platform SHALL validate worker connectivity and execution compatibility before distributed training begins. |
| DST-003 | The platform SHALL display participating workers and individual resource measurements. |
| DST-004 | Worker failures SHALL be reflected in run status and diagnostics. |
| DST-005 | The platform SHALL support restarting an interrupted distributed run from a compatible checkpoint. |
| DST-006 | Each distributed run SHALL record worker topology and effective training configuration. |
| DST-007 | The platform SHOULD support throughput comparisons between single-worker and distributed executions. |
| DST-008 | The platform SHALL NOT represent distributed execution as a guaranteed performance improvement. |
| DST-009 | Distributed jobs SHALL acquire their required worker allocation as a unit, or remain queued without holding a partial allocation indefinitely. |
| DST-010 | Distributed GPU communication SHALL be validated across the actual participating hosts and container network before that topology is advertised as supported. |

## Worker enrolment and capability discovery

| ID | Requirement |
|---|---|
| WRK-001 | An authorised worker owner SHALL be able to enrol a GPU server through a documented agent installation and registration process. |
| WRK-002 | New workers SHALL require approval under administrator-defined policy before receiving jobs or project data. |
| WRK-003 | Enrolment SHALL exchange a short-lived, single-use bootstrap credential for a unique, revocable worker identity. |
| WRK-004 | Agents SHALL initiate authenticated, encrypted outbound connections to cloud services; inbound port forwarding on worker networks SHALL NOT be required for single-worker execution. |
| WRK-005 | Workers SHALL advertise host operating system, CPU architecture and capacity, RAM, GPU count and model, per-device memory, driver and runtime versions, free storage and supported workload capabilities. |
| WRK-006 | Capability reports SHALL distinguish detected hardware, owner-imposed limits, verified capabilities and current allocatable capacity. |
| WRK-007 | The control plane SHALL validate required capabilities through health checks before marking a worker eligible. |
| WRK-008 | Agents SHALL refresh capabilities after relevant hardware, runtime or capacity changes, and send periodic heartbeats. |
| WRK-009 | Worker owners SHALL be able to configure resource limits, availability windows and the projects or worker pools allowed to use their machines. |
| WRK-010 | Administrators SHALL be able to approve, quarantine, drain and revoke workers with audited state changes. |
| WRK-011 | Scheduling SHALL exclude workers whose health or capability information has expired under a documented freshness policy. |
| WRK-012 | A worker SHALL validate assignment identity, approved image, resource limits and execution lease before starting a job. |
| WRK-013 | Agent and control-plane protocol versions SHALL be negotiated; incompatible agents SHALL be blocked from new assignments with actionable diagnostics. |
| WRK-014 | Disconnects SHALL trigger bounded reconnect attempts with backoff and reconciliation of existing assignments before new jobs start. |
| WRK-015 | Worker ownership, pool membership and trust classification SHALL be visible to authorised users selecting compute. |

## Compute groups

The initial home compute group contains two machines on the same LAN. Group registration and visibility are initial-release capabilities; execution of a distributed job across group members belongs to the distributed-training release.

| ID | Requirement |
|---|---|
| GRP-001 | The platform SHALL represent a compute group with a stable identifier, name, owner, approved members and network configuration. |
| GRP-002 | Authorised owners SHALL be able to create groups and request membership for their workers; membership changes SHALL follow an audited approval policy. |
| GRP-003 | Each worker SHALL advertise its group membership and group-reachable execution endpoints separately from its cloud control connection. |
| GRP-004 | The platform SHALL verify required peer connectivity from the actual container execution environment before declaring a group ready for a supported multi-worker workload. |
| GRP-005 | A common public IP address, claimed group identifier or shared cloud connection SHALL NOT constitute proof of peer connectivity or authorisation. |
| GRP-006 | The UI SHALL show group members, per-worker capabilities, available GPU capacity and network validation status. |
| GRP-007 | Group capacity SHALL distinguish individual GPU memory capacities; summed memory SHALL NOT be presented as a single GPU's usable memory. |
| GRP-008 | Group membership SHALL NOT grant project access or override worker sharing, trust or resource policies. |
| GRP-009 | Membership, endpoint or network changes SHALL invalidate affected connectivity checks until revalidated. |
| GRP-010 | Workers SHALL retain individual identities, health state, leases and usage records when operating within a group. |
| GRP-011 | The platform SHALL distinguish compute groups, which describe network topology, from worker pools, which describe logical scheduling or access policy. |
| GRP-012 | A worker MAY advertise multiple approved network group memberships; its resources SHALL be counted and allocated only once across overlapping groups. |

## Distributed network placement — subsequent release

| ID | Requirement |
|---|---|
| DST-011 | Distributed jobs SHALL be assigned only to worker groups with a validated data-plane network, including measured bandwidth, latency and required connectivity. |
| DST-012 | Connection to the same cloud control plane SHALL NOT by itself imply eligibility for joint distributed training. |
| DST-013 | Worker-to-worker training traffic SHALL use an explicitly configured secure network path separate from control-plane messaging. |
| DST-014 | A distributed job SHALL use compatible workers within one validated compute group; cross-group distributed execution is outside the initial distributed-training scope. |
| DST-015 | Training collectives between members of the home compute group SHALL use the configured LAN path rather than being relayed through the cloud control plane. |
