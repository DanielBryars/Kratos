-- Null preserves unlimited project concurrency for existing projects; there is no default quota.
ALTER TABLE projects
    ADD COLUMN max_concurrent_jobs integer CHECK (max_concurrent_jobs > 0);

-- The scheduler counts a project's active jobs while holding the project row lock, so the count
-- is an index lookup rather than a scan of the project's whole history.
CREATE INDEX jobs_project_active_idx ON jobs (project_id)
    WHERE status IN ('assigned', 'running', 'cancelling');

COMMENT ON COLUMN projects.max_concurrent_jobs IS
    'Maximum jobs assigned, running or cancelling at once; null means unlimited.';
