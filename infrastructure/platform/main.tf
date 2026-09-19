resource "google_project_service" "platform" {
  for_each = toset([
    "artifactregistry.googleapis.com",
    "cloudresourcemanager.googleapis.com",
    "compute.googleapis.com",
    "iam.googleapis.com",
    "identitytoolkit.googleapis.com",
    "logging.googleapis.com",
    "monitoring.googleapis.com",
    "run.googleapis.com",
    "secretmanager.googleapis.com",
    "serviceusage.googleapis.com",
    "sqladmin.googleapis.com",
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

resource "google_service_account" "database_migration" {
  project      = var.project_id
  account_id   = "kratos-db-migration"
  display_name = "Kratos database migration"

  depends_on = [google_project_service.platform]
}

resource "google_sql_database_instance" "kratos" {
  count = var.enable_database ? 1 : 0

  project             = var.project_id
  name                = "kratos"
  region              = var.region
  database_version    = "POSTGRES_16"
  deletion_protection = var.database_deletion_protection

  settings {
    edition           = "ENTERPRISE"
    tier              = "db-f1-micro"
    availability_type = "ZONAL"
    disk_type         = "PD_SSD"
    disk_size         = 10
    disk_autoresize   = true

    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
    }

    database_flags {
      name  = "cloudsql.iam_authentication"
      value = "on"
    }

    ip_configuration {
      ipv4_enabled = true
    }
  }

  depends_on = [google_project_service.platform]
}

resource "google_sql_database" "kratos" {
  count = var.enable_database ? 1 : 0

  project  = var.project_id
  name     = "kratos"
  instance = google_sql_database_instance.kratos[0].name
}

resource "google_sql_user" "control_plane" {
  count = var.enable_database ? 1 : 0

  project  = var.project_id
  name     = trimsuffix(google_service_account.control_plane.email, ".gserviceaccount.com")
  instance = google_sql_database_instance.kratos[0].name
  type     = "CLOUD_IAM_SERVICE_ACCOUNT"
}

resource "google_sql_user" "database_migration" {
  count = var.enable_database ? 1 : 0

  project        = var.project_id
  name           = trimsuffix(google_service_account.database_migration.email, ".gserviceaccount.com")
  instance       = google_sql_database_instance.kratos[0].name
  type           = "CLOUD_IAM_SERVICE_ACCOUNT"
  database_roles = ["cloudsqlsuperuser"]
}

resource "google_project_iam_member" "control_plane_cloud_sql_client" {
  count = var.enable_database ? 1 : 0

  project = var.project_id
  role    = "roles/cloudsql.client"
  member  = "serviceAccount:${google_service_account.control_plane.email}"
}

resource "google_project_iam_member" "control_plane_cloud_sql_user" {
  count = var.enable_database ? 1 : 0

  project = var.project_id
  role    = "roles/cloudsql.instanceUser"
  member  = "serviceAccount:${google_service_account.control_plane.email}"
}

resource "google_project_iam_member" "database_migration_cloud_sql_client" {
  count = var.enable_database ? 1 : 0

  project = var.project_id
  role    = "roles/cloudsql.client"
  member  = "serviceAccount:${google_service_account.database_migration.email}"
}

resource "google_project_iam_member" "database_migration_cloud_sql_user" {
  count = var.enable_database ? 1 : 0

  project = var.project_id
  role    = "roles/cloudsql.instanceUser"
  member  = "serviceAccount:${google_service_account.database_migration.email}"
}
