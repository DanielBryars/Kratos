# Project concurrency limit

Implements the per-project concurrency part of SCH-007 and the project-quota input to SCH-004. It does not cover per-user quotas, resource quotas, fairness, priority or credits.

## Scope

Migration 032 adds a nullable `projects.max_concurrent_jobs` (positive integer, CHECK > 0). Null means unlimited, which is how every existing project starts, and there is no default quota. A partial index on `jobs (project_id)` covers the active statuses so the count is cheap to hold a lock across.

A job holds a slot while its status is `assigned`, `running` or `cancelling`. Output delivery happens while the job is still assigned or running, so it keeps its slot until the job is terminal. A cancelling job keeps its slot until the worker acknowledges or the lease expires. Slots are freed only by existing lifecycle transitions (result, cancellation acknowledgement, lease recovery), never by elapsed time alone.

Scheduler (`registry::current_or_assign_job`): replaying an existing active assignment happens before the gate and is unchanged. Before allocating a new job, the scheduler locks the worker's project row `FOR NO KEY UPDATE`, then counts active project jobs in a separate statement. Under READ COMMITTED that statement takes a fresh snapshot after the lock is granted, so it sees an assignment committed by the previous lock holder. At or above the limit, no assignment is made. The lock is held until the assignment transaction commits.

Lock order is worker → project → job. `FOR NO KEY UPDATE` conflicts with another scheduler and with a policy update. It does not conflict with the `FOR KEY SHARE` taken by job submission's foreign key, so submitting a job never waits on scheduling. Membership changes lock identity → project `FOR UPDATE` and never lock workers or jobs afterwards, so they can briefly delay a heartbeat but cannot form a cycle with one. Transitions that free a slot do not take the project lock. They only lower the count, so the worst race outcome is a conservative refusal, and the next heartbeat assigns. The lock is taken even when the limit is null, so a policy change is ordered against every assignment decision.

API: `GET /api/v1/operator/scheduling-policy` and `PUT /api/v1/operator/scheduling-policy` (new module `scheduling_policy.rs`, registered in routes and OpenAPI). Both endpoints authenticate the operator and resolve `sole_project`. A caller in several projects gets 409 `project_required`. The response carries `project_id`, `max_concurrent_jobs` and `active_jobs`. For PUT, `max_concurrent_jobs` is a required field: an explicit null clears the limit and a positive integer sets it. A missing field, zero, a negative number, a non-integer, a string, a value above the PostgreSQL integer range, an unknown field or malformed JSON gets 422 `invalid_request`. PUT authenticates before reading the body. Inside the write transaction it locks the project row `FOR NO KEY UPDATE` and then rechecks the caller's active operator membership. Membership removal holds the same row `FOR UPDATE`, so the recheck cannot race a revocation. The update and a `project.scheduling_policy.updated` audit event (previous and new value) commit atomically. Lowering the limit never preempts active jobs.

Console: the work-queue section shows the limit and active count, an Unlimited toggle, a positive-integer input and a Save button, and reports errors inline. An empty or invalid input is refused in the browser and never sent as a clear. Queued-job explanations are, in priority order:

1. a future earliest start;
2. the project concurrency limit, when the active count is at or above the configured limit;
3. otherwise a generic "waiting for an eligible worker".

No per-worker compatibility diagnosis is claimed. If the policy cannot be loaded, the card says "waiting to be scheduled" rather than guessing a reason.

## Tests

PostgreSQL scheduler tests (`services/control-plane/src/registry/concurrency_tests.rs`):

- `limit_of_one_never_oversubscribes_two_concurrent_workers`: an uncommitted policy update holds the project row. Both schedulers are observed waiting on a lock in `pg_stat_activity` before the update commits. Exactly one attempt, one busy worker and one queued job result.
- `unlimited_project_assigns_both_concurrent_workers`: the same forced overlap with a null limit assigns both jobs.
- `cancelling_job_holds_its_slot_until_acknowledged`: a cancelling job blocks a second worker at limit 1. After the acknowledgement goes through the worker result endpoint, the waiting job is assigned.
- `cap_keeps_replay_and_resumes_a_due_job_after_release`: at the cap the holder still replays its attempt, and a due earliest-start job gets no attempt. After a real success result, that job is assigned.
- `pending_output_delivery_holds_its_slot`: a success report without the mandatory verified output is refused (409). The job stays assigned and keeps its slot.
- `lowering_the_limit_does_not_preempt_active_jobs`: two active jobs stay assigned under a new limit of 1. A third job starts only once the active count falls below the limit.

PostgreSQL API tests (`services/control-plane/src/scheduling_policy/tests.rs`):

- Unlimited default with correct active counting (queued and terminal jobs excluded), set, explicit null clear, and audit detail and actor.
- Missing, zero, negative, fractional, string, out-of-range, unknown-field and malformed bodies are rejected, and the stored limit is unchanged with no audit.
- Missing or invalid token gets 401, including with an invalid body. An unknown non-bootstrap identity and a revoked member get 403.
- Project scoping: a member of another project sees and changes only that project's policy and count. A caller in two projects gets 409.
- A policy update racing a membership revocation that holds the project row fails with 403 and changes nothing.

The OpenAPI test asserts both operations and the required request field. Web tests (`apps/web/src/schedulingPolicy.test.mjs`) cover the payload rules (explicit null, positive integer, rejection of empty, zero, negative, fractional and oversized input), the policy summary, and queue explanation precedence: earliest start over limit, limit when at capacity, generic otherwise, and unknown when the policy is unavailable.

## Status and limitations

All 52 web tests and the production web build pass locally. PostgreSQL scheduler and API tests await CI execution; no server-test pass is claimed yet. No live or deployed acceptance has been performed and no workload has been run. The concurrency tests depend on `pg_stat_activity` showing lock waits for sessions of the same role in the per-test database.

Not implemented: fairness between projects or users, per-user limits, priority, credits, reservations, and preemption. The limit applies to the single-GPU jobs the scheduler currently assigns. The console explanation reads the project-wide count on the five-second refresh, so it can lag the scheduler briefly.
