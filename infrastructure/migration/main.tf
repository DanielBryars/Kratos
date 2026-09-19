resource "google_cloud_run_v2_job" "database_migration" {
  count = var.database_enabled ? 1 : 0

  project             = var.project_id
  name                = "kratos-database-migration"
  location            = var.region
  deletion_protection = false

  template {
    task_count  = 1
    parallelism = 1

    template {
      service_account = var.migration_service_account
      max_retries     = 0
      timeout         = "600s"

      containers {
        name    = "migration"
        image   = var.container_image
        command = ["/usr/local/bin/kratos-migrate-with-proxy"]

        resources {
          limits = {
            cpu    = "1"
            memory = "512Mi"
          }
        }

        env {
          name  = "KRATOS_DATABASE_CONNECTION_NAME"
          value = var.database_connection_name
        }

        env {
          name  = "KRATOS_DATABASE_HOST"
          value = "127.0.0.1"
        }

        env {
          name  = "KRATOS_DATABASE_PORT"
          value = "5432"
        }

        env {
          name  = "KRATOS_DATABASE_NAME"
          value = "kratos"
        }

        env {
          name  = "KRATOS_DATABASE_USER"
          value = var.migration_database_iam_username
        }

        env {
          name  = "KRATOS_APPLICATION_DATABASE_USER"
          value = var.application_database_iam_username
        }
      }
    }
  }
}
