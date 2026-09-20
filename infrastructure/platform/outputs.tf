output "artifact_registry_repository" {
  description = "Docker repository prefix."
  value       = "${var.region}-docker.pkg.dev/${var.project_id}/${google_artifact_registry_repository.kratos.repository_id}"
}

output "runtime_service_account" {
  description = "Cloud Run runtime service account."
  value       = google_service_account.control_plane.email
}

output "migration_service_account" {
  description = "Cloud Run identity used only by the schema migration job."
  value       = google_service_account.database_migration.email
}

output "artifact_bucket" {
  description = "Private bucket used for durable job artifacts."
  value       = google_storage_bucket.artifacts.name
}

output "artifact_upload_signer_service_account" {
  description = "Create-only identity used through IAM signBlob; it has no persistent key."
  value       = google_service_account.artifact_upload_signer.email
}

output "database_enabled" {
  description = "Whether this platform state manages a billable Cloud SQL instance."
  value       = var.enable_database
}

output "database_connection_name" {
  description = "Cloud SQL connection name, or null while the cost gate is disabled."
  value       = var.enable_database ? google_sql_database_instance.kratos[0].connection_name : null
}

output "database_iam_username" {
  description = "Passwordless PostgreSQL IAM username, or null while the cost gate is disabled."
  value       = var.enable_database ? google_sql_user.control_plane[0].name : null
}

output "migration_database_iam_username" {
  description = "Passwordless PostgreSQL migration username, or null while the cost gate is disabled."
  value       = var.enable_database ? google_sql_user.database_migration[0].name : null
}
