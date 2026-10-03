-- Protocol 1.4: job parameters, and runtimes up to 24 hours.
--
-- Both are gated to workers that advertise protocol 1.4 (registry/fairness.rs). That gate is not
-- optional for either. An agent below 1.4 rejects an assignment whose timeout exceeds an hour --
-- its model bounds the field and forbids surprises -- and an agent that predated parameters
-- would run the job without them, silently, which is worse than not running it.

-- Up to 24 hours. A worker that dies mid-job is recovered only when the attempt's lease expires,
-- and the lease is the job's own timeout plus two minutes, so a long limit is also a long wait
-- after a crash. That is per job: a job submitted with a one-hour limit still recovers in an hour.
ALTER TABLE jobs DROP CONSTRAINT jobs_timeout_seconds_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_timeout_seconds_check
    CHECK (timeout_seconds BETWEEN 30 AND 86400);

-- A flat object of named scalars, delivered to the workload as the KRATOS_PARAMETERS environment
-- variable. Not secret: stored here, shown in the console and recorded in MLflow. The handler
-- validates keys, value types and a 4 KiB compact size (under MLflow's 8,000-character tag limit,
-- since the object is recorded as one run tag). The database holds the shape and a looser size
-- backstop, because PostgreSQL's text form of jsonb adds spaces the compact form does not.
ALTER TABLE jobs ADD COLUMN parameters jsonb
    CHECK (parameters IS NULL
           OR (jsonb_typeof(parameters) = 'object' AND octet_length(parameters::text) <= 8192));

COMMENT ON COLUMN jobs.parameters IS
    'Named scalar parameters for the workload, delivered as KRATOS_PARAMETERS. Never secret.';
