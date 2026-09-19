output "artifact_registry_repository" {
  description = "Docker repository prefix."
  value       = "${var.region}-docker.pkg.dev/${var.project_id}/${google_artifact_registry_repository.kratos.repository_id}"
}

output "runtime_service_account" {
  description = "Cloud Run runtime service account."
  value       = google_service_account.control_plane.email
}
