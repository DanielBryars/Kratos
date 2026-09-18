resource "google_project_service" "platform" {
  for_each = toset([
    "artifactregistry.googleapis.com",
    "iam.googleapis.com",
    "logging.googleapis.com",
    "monitoring.googleapis.com",
    "run.googleapis.com",
    "secretmanager.googleapis.com",
    "serviceusage.googleapis.com",
  ])

  project            = var.project_id
  service            = each.value
  disable_on_destroy = false
}

resource "google_artifact_registry_repository" "kratos" {
  project       = var.project_id
  location      = var.region
  repository_id = "kratos"
  description   = "Kratos application images"
  format        = "DOCKER"

  cleanup_policies {
    id     = "retain-recent"
    action = "KEEP"

    most_recent_versions {
      keep_count = 20
    }
  }

  depends_on = [google_project_service.platform]
}

resource "google_service_account" "control_plane" {
  project      = var.project_id
  account_id   = "kratos-control-plane"
  display_name = "Kratos control plane"

  depends_on = [google_project_service.platform]
}
