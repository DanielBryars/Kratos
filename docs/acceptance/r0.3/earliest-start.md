# Earliest-start scheduling

Implements SCH-002, SCH-012 and SCH-015 for the existing single-worker queue.

The console accepts an optional date/time in the browser's local timezone and submits an RFC3339 UTC instant. The API accepts an optional earliest_start_at timestamp with an explicit offset; omitted/null means immediate eligibility. Past times are already eligible. PostgreSQL stores timestamptz and the scheduler checks database time before allocating an attempt. Existing clients and jobs retain immediate eligibility.

A queued future job allocates neither a worker nor an attempt and does not block a later eligible job. Once eligible, existing compatibility/project predicates and submitted-time ordering apply (UUID breaks equal-time ties). This does not implement project fairness, guaranteed reservations, priority, credits or ETA. Runtime/lease clocks start at assignment, not submission. The queue card displays the future threshold in the viewer's locale; the field is immutable through the API and appears in job.queued audit evidence.

Validation: web submission tests cover explicit offset conversion and invalid input; existing default submission assertions are unchanged. PostgreSQL tests cover future-job exclusion without attempt allocation, skipping it for an immediate job, assigning an already-due job, and API round-trip plus cancellation of a future job. CI runs the Rust/PostgreSQL tests and full compatibility checks. No live workload has been run for this change.
