resource "google_cloud_run_v2_service" "control_plane" {
  project             = var.project_id
  name                = "kratos"
  location            = var.region
  deletion_protection = false
  ingress             = "INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER"

  template {
    service_account = var.runtime_service_account

    scaling {
      min_instance_count = 0
      max_instance_count = 2
    }

    containers {
      name       = "control-plane"
      image      = var.container_image
      depends_on = var.database_enabled ? ["cloud-sql-proxy"] : []

      resources {
        limits = {
          cpu    = "1"
          memory = "512Mi"
        }
        cpu_idle = true
      }

      ports {
        container_port = 8080
      }

      env {
        name  = "RUST_LOG"
        value = "kratos_control_plane=info"
      }

      env {
        name  = "KRATOS_GRAFANA_URL"
        value = "https://grafana.${var.domain_name}"
      }

      env {
        name  = "KRATOS_MLFLOW_URL"
        value = "https://mlflow.${var.domain_name}"
      }

      env {
        name  = "KRATOS_ARTIFACT_BUCKET"
        value = var.artifact_bucket
      }

      env {
        name  = "KRATOS_ARTIFACT_SIGNER_SERVICE_ACCOUNT"
        value = var.artifact_upload_signer_service_account
      }

      dynamic "env" {
        for_each = var.identity_platform_api_key == "" ? {} : {
          KRATOS_IDENTITY_PLATFORM_API_KEY    = var.identity_platform_api_key
          KRATOS_IDENTITY_PLATFORM_PROJECT_ID = var.project_id
          KRATOS_BOOTSTRAP_OPERATOR_EMAIL     = var.bootstrap_operator_email
        }

        content {
          name  = env.key
          value = env.value
        }
      }

      dynamic "env" {
        for_each = var.database_enabled ? {
          KRATOS_DATABASE_HOST = "127.0.0.1"
          KRATOS_DATABASE_PORT = "5432"
          KRATOS_DATABASE_NAME = "kratos"
          KRATOS_DATABASE_USER = var.database_iam_username
        } : {}

        content {
          name  = env.key
          value = env.value
        }
      }

      startup_probe {
        initial_delay_seconds = 0
        timeout_seconds       = 2
        period_seconds        = 2
        failure_threshold     = 15

        http_get {
          path = "/healthz"
          port = 8080
        }
      }

      liveness_probe {
        timeout_seconds   = 2
        period_seconds    = 30
        failure_threshold = 3

        http_get {
          path = "/healthz"
          port = 8080
        }
      }
    }

    dynamic "containers" {
      for_each = var.database_enabled ? [var.database_connection_name] : []

      content {
        name  = "cloud-sql-proxy"
        image = var.cloud_sql_proxy_image
        args = [
          "--address=0.0.0.0",
          "--port=5432",
          "--auto-iam-authn",
          "--structured-logs",
          containers.value,
        ]

        resources {
          limits = {
            cpu    = "0.25"
            memory = "128Mi"
          }
          cpu_idle = true
        }

        startup_probe {
          initial_delay_seconds = 0
          timeout_seconds       = 2
          period_seconds        = 2
          failure_threshold     = 15

          tcp_socket {
            port = 5432
          }
        }
      }
    }
  }
}

resource "google_cloud_run_v2_service_iam_member" "public" {
  count = var.allow_unauthenticated ? 1 : 0

  project  = google_cloud_run_v2_service.control_plane.project
  location = google_cloud_run_v2_service.control_plane.location
  name     = google_cloud_run_v2_service.control_plane.name
  role     = "roles/run.invoker"
  member   = "allUsers"
}

resource "google_compute_global_address" "control_plane" {
  project = var.project_id
  name    = "kratos"
}

resource "google_compute_region_network_endpoint_group" "control_plane" {
  project               = var.project_id
  name                  = "kratos-cloud-run"
  region                = var.region
  network_endpoint_type = "SERVERLESS"

  cloud_run {
    service = google_cloud_run_v2_service.control_plane.name
  }
}

resource "google_compute_backend_service" "control_plane" {
  project               = var.project_id
  name                  = "kratos"
  protocol              = "HTTP"
  load_balancing_scheme = "EXTERNAL_MANAGED"

  backend {
    group = google_compute_region_network_endpoint_group.control_plane.id
  }
}

resource "google_compute_url_map" "control_plane" {
  project         = var.project_id
  name            = "kratos"
  default_service = google_compute_backend_service.control_plane.id
}

resource "google_compute_managed_ssl_certificate" "control_plane" {
  project = var.project_id
  name    = "kratos"

  managed {
    domains = [var.domain_name]
  }
}

resource "google_compute_ssl_policy" "control_plane" {
  project         = var.project_id
  name            = "kratos"
  profile         = "MODERN"
  min_tls_version = "TLS_1_2"
}

resource "google_compute_target_https_proxy" "control_plane" {
  project          = var.project_id
  name             = "kratos"
  url_map          = google_compute_url_map.control_plane.id
  ssl_certificates = [google_compute_managed_ssl_certificate.control_plane.id]
  ssl_policy       = google_compute_ssl_policy.control_plane.id
}

resource "google_compute_global_forwarding_rule" "https" {
  project               = var.project_id
  name                  = "kratos-https"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  ip_address            = google_compute_global_address.control_plane.id
  port_range            = "443"
  target                = google_compute_target_https_proxy.control_plane.id
}

resource "google_compute_url_map" "http_redirect" {
  project = var.project_id
  name    = "kratos-http-redirect"

  default_url_redirect {
    https_redirect = true
    strip_query    = false
  }
}

resource "google_compute_target_http_proxy" "http_redirect" {
  project = var.project_id
  name    = "kratos-http-redirect"
  url_map = google_compute_url_map.http_redirect.id
}

resource "google_compute_global_forwarding_rule" "http" {
  project               = var.project_id
  name                  = "kratos-http"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  ip_address            = google_compute_global_address.control_plane.id
  port_range            = "80"
  target                = google_compute_target_http_proxy.http_redirect.id
}
