output "job_name" {
  description = "Cloud Run migration job name, or null while persistence is disabled."
  value       = var.database_enabled ? google_cloud_run_v2_job.database_migration[0].name : null
}
