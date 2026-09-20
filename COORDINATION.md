# Agent coordination

A shared note between the AI agents working in this repository (Codex, which owns integration, and
Claude) and the user. Ownership of R0.2 work is defined in
[docs/r0.2-workstreams.md](docs/r0.2-workstreams.md); this file only records who is touching what
right now and what one agent needs the other to know. Keep entries short, date them, and delete
them once they are resolved or merged.

## Conventions

- Claim work here before starting it, with the branch name and the paths you expect to change.
- Do not edit paths another agent has claimed; leave a note under "Handover notes" instead.
- Work on a branch cut from `main` and open a PR. Codex reviews and integrates.
- The checkout at `F:\git\Kratos` may be shared. Say here when you switch its branch, or use a
  worktree.

## Active claims

| Agent | Branch | Paths | Status |
|---|---|---|---|
| Claude | `feature/r0.2-agent-network-loss-safety` | `workers/agent/**`, `docs/protocol/worker-v1.md`, `docs/acceptance/r0.2/network-loss-exercise-runbook.md` | 2026-09-20 — PR open for Codex review |
| Codex | — | `services/control-plane/**`, `infrastructure/**`, `apps/web/**` (artefacts, provenance; assumed from the workstreams plan) | In progress |

## Handover notes

### Claude → Codex, 2026-09-20

Found while reviewing for the network-loss workstream. None of these are changed by Claude's PR.

1. **No long-running approved workload.** The training example finishes in about 0.5 s and job
   submission takes no arguments, so the physical disconnect exercise cannot run yet. The draft
   runbook lists this as an open prerequisite.
2. **Leases never expire on the server.** `current_or_assign_job` returns an active attempt forever
   and nothing reaps it. A worker that never returns leaves its job `assigned` and itself `busy`,
   and `cancel_job` only accepts `queued`.
3. **Fabricated start time.** `report_job_result` sets
   `jobs.started_at = COALESCE(started_at, submitted_at)` and the attempt's to `assigned_at`, so
   queue time is shown as run time. The agent could report the container's real `StartedAt` and
   `FinishedAt` if the result contract gains those fields.
4. **Busy workers look offline.** The agent sends no heartbeat while supervising a container, so a
   job longer than 90 s shows the worker as `stale`, then `offline`. Documented as a known
   limitation in the protocol; fixing it needs heartbeats during execution.
5. **Agent interface change to be aware of for the artefact work:** `DockerExecutor` now has
   `prepare_job`, `run_job(assignment, *, may_start)` and `remove_job_container(attempt_id)`, and
   `AgentState` gained `started_attempt_id`. Output collection should happen after `run_job`
   returns and before `remove_job_container`.

6. **PR #31 (`test/r0.2-lease-safety`) and PR #33 do not overlap.** #31 is control-plane only; #33
   is agent only. They fit together: #31 answers a delayed stale result with `200`, which the agent
   treats as the acknowledgement and then removes its container. If a later change rejects stale
   results with a `4xx` instead, note that the agent treats any non-`429` `4xx` as fatal and exits;
   Docker restarts it and the orphan cleanup then removes the container, but a dedicated response
   the agent can handle in place would be cleaner.

### Codex → Claude

_Nothing yet._
