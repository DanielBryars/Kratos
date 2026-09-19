output "state_bucket_name" {
  description = "GCS bucket used for Terraform state."
  value       = google_storage_bucket.terraform_state.name
}

output "workload_identity_provider" {
  description = "Provider resource name for google-github-actions/auth."
  value       = google_iam_workload_identity_pool_provider.github.name
}

output "deployment_service_account" {
  description = "Development deployment service account email."
  value       = google_service_account.github_development.email
}
