# R0.2 network-loss exercise runbook

**Status:** Ready to execute; both digests below are recorded and the worker is upgraded
**Witness:** the user, who physically disconnects and reconnects the selected worker

## Requirements

- R0.2 exit — network loss SHALL NOT permit duplicate execution
- ACC-023 — the lease bounds continued execution and reconnection produces no duplicate execution
  (the budget and charge clauses arrive with credits in R0.3/R0.4)
- ACC-018 — the record identifies software, hardware, inputs, expected and observed outcomes

## Prerequisites

1. The worker runs an agent image containing the execution-authority behaviour in the
   [worker protocol](../../protocol/worker-v1.md#execution-authority-and-network-loss). THESHED2
   was upgraded in place to

   ```text
   ghcr.io/danielbryars/kratos-agent@sha256:00d55fac255dbca35d7ff50242f320825a025009ab0f5d9fd65dd82ca854056e
   ```

   keeping its `kratos-agent-state` volume and therefore its existing worker identity. Confirm at
   step 1 that this is still the running digest. The image installed for the first scheduled job,
   `kratos-agent@sha256:d7d03326…`, exits on the first failed request — the very behaviour this
   exercise exists to disprove — so if that digest is running, stop and upgrade before continuing.
2. The soak workload is merged and published from `main`. Its publish workflow succeeded from
   commit `b69f254`, producing the `linux/amd64` image

   ```text
   ghcr.io/danielbryars/kratos-soak-workload@sha256:75fa1ec48131c799a7b8b272557c11ed205e81b2c6dac83aea50fbb8edfa8773
   ```

   Confirm that digest still matches the published `edge` tag, then approve **the digest** as a job
   image; never submit a mutable tag. The workload holds the GPU for a fixed 600 seconds, which is
   the window the disconnect happens in.
3. The worker is `ONLINE IDLE` in the `Home` group with a verified healthy GPU, and no other job is
   queued.
4. A second device, not on the worker's network link, is signed in to the operator console.

The witness needs no database access, cloud credential or hand-built API request. Every worker
command below is read-only.

## Procedure

Record the wall-clock time of every step.

| Step | Action | Expected |
|---|---|---|
| 1 | On the worker, record `docker inspect kratos-agent --format "{{.RestartCount}} {{.Config.Image}}"`. | The restart count, and **the agent digest from prerequisite 1**. If it shows the older `d7d03326…` digest, stop: the exercise cannot prove anything against it. |
| 2 | From the console, submit the soak workload with a 900-second maximum runtime, comfortably above its fixed 600-second duration. Record the job identifier. | `QUEUED`, then `ASSIGNED` to this worker within one heartbeat, about 30 seconds. |
| 3 | On the worker, run `docker ps --filter label=com.kratos.role=job`. | Exactly one running `kratos-job-<attempt>` container. Record its name. |
| 4 | When prompted, **disconnect the worker's network**: unplug Ethernet or disable its adapter. Leave the machine running. | — |
| 5 | Wait until `docker ps -a --filter label=com.kratos.role=job` shows the container as `Exited (0)`, then at least one further minute. | The workload finishes without a network. `docker logs kratos-agent --since 5m` shows `{"status": "retrying", …}` lines and no credential. The console still shows the job as `ASSIGNED`. |
| 6 | Repeat step 1. | The restart count is unchanged: the agent survived the outage. |
| 7 | When prompted, **reconnect the network**. | Within about a minute the console shows the job `SUCCEEDED` with the workload's structured result and the worker `ONLINE IDLE`. |
| 8 | On the worker, run `docker events --since 30m --until 0s --filter event=start --filter label=com.kratos.role=job`. | Exactly one `start` event, for the container recorded in step 3. This is the no-duplicate-execution evidence. |
| 9 | Repeat step 3 with `-a`. | No `kratos-job-*` container remains. |

Codex SHALL then confirm from the control plane that the job has exactly one attempt, that the
attempt belongs to this worker, and that one `job.assigned` and one `job.finished` audit event exist.

## Interpreting the console during the exercise

The agent heartbeats while it supervises a container, so the worker SHALL read `ONLINE BUSY` from
assignment until step 4. After the disconnect it SHALL read `STALE` within 90 seconds and `OFFLINE`
after five minutes, and return to `ONLINE` after step 7. Record those times. A worker that shows
`STALE` before step 4 is running an agent image that predates in-job heartbeats; stop and correct
prerequisite 1. Connectivity shows that the outage was real; the no-duplicate evidence remains the
job state, the container listing and the Docker event log.

## Failure handling

- A second `start` event, a second attempt, or a job that reaches `SUCCEEDED` twice fails the
  exercise. Preserve `docker logs kratos-agent` and the event output before changing anything.
- If the job is still `ASSIGNED` five minutes after reconnection, record the agent log and stop.
  The deployed control plane reconciles expired leases and permits active cancellation, so an
  attempt remaining assigned indicates a reconciliation or heartbeat failure. Preserve the job,
  attempt and audit evidence; do not recreate the agent container or its state volume.
- If the lease expires and the job is requeued or a second attempt starts, stop the exercise. That
  is bounded recovery, but it fails this exercise's single-execution replay path. Record both
  attempts and the `job.attempt.lease_expired` audit event before cancelling the job.
- If the workload is still running when its runtime bound passes, the agent **kills** the container
  at that bound, with no grace period, and the job is reported `FAILED` with a timeout after
  reconnection. Expect no interruption record from the workload: Kratos does not signal it. That
  is correct lease behaviour but not the replay path this exercise targets; repeat with a shorter
  outage.
