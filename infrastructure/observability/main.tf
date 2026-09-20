# The ADR-009 observability stack on one Compute Engine instance.
#
# Everything here is behind var.enable_observability, which defaults to false, because an instance
# and a persistent disk bill continuously. Applying with the gate closed creates nothing.
#
# Only Grafana, MLflow and the OTLP gateway are reachable, all through an HTTPS load balancer.
# Grafana and MLflow sit behind Identity-Aware Proxy so a human must sign in before reaching the
# service itself. Kratos itself authenticates humans through Cloud Identity Platform rather than
# IAP; the two are different mechanisms, and the principal authorised here is the same Google
# account either way. Prometheus, Loki and Tempo have no route from outside the VPC, which is what
# ADR-009 requires of the backends.

locals {
  enabled = var.enable_observability ? 1 : 0
  # A budget is managed only when one is asked for and there is an account to bill against. The
  # API it needs is keyed off the same condition, so the two can never disagree.
  budget    = var.enable_budget && var.billing_account != "" ? 1 : 0
  mlflow_db = var.enable_observability && var.enable_mlflow_database ? 1 : 0
  # OTLP ingestion is published only when explicitly enabled, because nothing authenticates a
  # worker yet: ADR-009 requires a scoped revocable credential and that decision is open.
  otlp           = var.enable_observability && var.enable_otlp_ingress ? 1 : 0
  name           = "kratos-observability"
  grafana_host   = "grafana.${var.domain_name}"
  mlflow_host    = "mlflow.${var.domain_name}"
  otel_host      = "otel.${var.domain_name}"
  human_services = { grafana = 3000, mlflow = 5000 }
  mlflow_instance = var.enable_mlflow_database ? (
    "${var.project_id}:${var.region}:${var.mlflow_database_instance}"
  ) : ""
  # IAM database authentication: the proxy supplies the credential, so no password exists.
  # With no database the store is a file on the persistent disk, and the startup script leaves the
  # Cloud SQL overlay out entirely, so nothing references a proxy that was never created.
  mlflow_database_uri = var.enable_mlflow_database ? (
    "postgresql://${local.mlflow_database_user}@cloud-sql-proxy:5432/mlflow"
  ) : "sqlite:////var/lib/mlflow/mlflow.db"
  mlflow_database_user = var.enable_mlflow_database ? (
    trimsuffix(google_service_account.observability[0].email, ".gserviceaccount.com")
  ) : ""
}

# The budget is deliberately not behind enable_observability: it should exist before, and outlive,
# anything that spends. Terraform refuses to plan an enabled stack without one.
resource "terraform_data" "budget_required" {
  count = local.enabled

  lifecycle {
    precondition {
      condition = !var.enable_budget || var.billing_account != ""
      error_message = join(" ", [
        "enable_budget is true but billing_account is empty, so no budget would be created.",
        "Supply the billing account, or set enable_budget=false to accept unwatched spend.",
      ])
    }
  }
}

# The Billing Budgets API is enabled whenever a budget is managed, and never as part of the
# observability gate: the budget exists before and outlives the stack, so an API enabled only with
# the stack would leave the closed-gate apply calling a disabled service.
resource "google_project_service" "budget" {
  count = local.budget

  project            = var.project_id
  service            = "billingbudgets.googleapis.com"
  disable_on_destroy = false
}

resource "google_billing_budget" "observability" {
  count = local.budget

  # Enabling a service is not synchronous, so this is an ordering edge and not decoration: without
  # it an enabled apply can create the budget while the API is still activating.
  depends_on = [google_project_service.budget]

  billing_account = var.billing_account
  display_name    = "Kratos ${var.project_id}"

  budget_filter {
    projects = ["projects/${var.project_id}"]
  }

  amount {
    specified_amount {
      # No currency_code: the budget then uses the billing account's own currency.
      units = tostring(var.monthly_budget)
    }
  }

  dynamic "threshold_rules" {
    for_each = [0.5, 0.9, 1.0]
    content {
      threshold_percent = threshold_rules.value
    }
  }

  # Tells you before it happens, not only after.
  threshold_rules {
    threshold_percent = 1.0
    spend_basis       = "FORECASTED_SPEND"
  }

  # Alerts go by email to the billing account's administrators and billing account users, which
  # is Google's default set and includes whoever owns the account. No notification channel is
  # created here: a channel is a project resource needing the Monitoring API, and the budget has
  # to work with the observability gate closed. Add channels here if that set is ever too narrow.
  all_updates_rule {
    monitoring_notification_channels = []
    disable_default_iam_recipients   = false
  }
}

resource "google_project_service" "observability" {
  for_each = var.enable_observability ? toset([
    "compute.googleapis.com",
    "iap.googleapis.com",
    "oslogin.googleapis.com",
  ]) : toset([])

  project            = var.project_id
  service            = each.value
  disable_on_destroy = false
}

# The instance identity. It reads its configuration bundle and writes telemetry object data; it has
# no permission to deploy anything or to read another environment's data.
resource "google_service_account" "observability" {
  count = local.enabled

  project      = var.project_id
  account_id   = "kratos-observability"
  display_name = "Kratos observability instance"
}

resource "google_storage_bucket" "config" {
  count = local.enabled

  project                     = var.project_id
  name                        = "${var.project_id}-observability-config"
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"

  versioning { enabled = true }
}

# Loki chunks, Tempo blocks and MLflow artefacts. Retention here is the object-lifecycle half of
# MON-019; the services apply their own retention to what they index.
resource "google_storage_bucket" "telemetry" {
  count = local.enabled

  project                     = var.project_id
  name                        = "${var.project_id}-observability-telemetry"
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"

  lifecycle_rule {
    condition {
      age            = 30
      matches_prefix = ["loki/"]
    }
    action { type = "Delete" }
  }

  lifecycle_rule {
    condition {
      age            = 14
      matches_prefix = ["tempo/"]
    }
    action { type = "Delete" }
  }
}

resource "google_storage_bucket_iam_member" "config_read" {
  count = local.enabled

  bucket = google_storage_bucket.config[0].name
  role   = "roles/storage.objectViewer"
  member = google_service_account.observability[0].member
}

resource "google_storage_bucket_iam_member" "telemetry_write" {
  count = local.enabled

  bucket = google_storage_bucket.telemetry[0].name
  role   = "roles/storage.objectAdmin"
  member = google_service_account.observability[0].member
}

# MLflow metadata on the existing Cloud SQL instance, so it inherits the backups the platform root
# already configures rather than living on this instance's disk.
resource "google_sql_database" "mlflow" {
  count = local.mlflow_db

  project  = var.project_id
  name     = "mlflow"
  instance = var.mlflow_database_instance
}

resource "google_sql_user" "mlflow" {
  count = local.mlflow_db

  project  = var.project_id
  name     = trimsuffix(google_service_account.observability[0].email, ".gserviceaccount.com")
  instance = var.mlflow_database_instance
  type     = "CLOUD_IAM_SERVICE_ACCOUNT"
}

resource "google_project_iam_member" "cloud_sql_client" {
  for_each = var.enable_observability && var.enable_mlflow_database ? toset([
    "roles/cloudsql.client",
    # Without instanceUser the proxy connects but IAM database login is refused.
    "roles/cloudsql.instanceUser",
  ]) : toset([])

  project = var.project_id
  role    = each.value
  member  = google_service_account.observability[0].member
}

resource "google_compute_network" "observability" {
  count = local.enabled

  project                 = var.project_id
  name                    = local.name
  auto_create_subnetworks = false
  depends_on              = [google_project_service.observability]
}

resource "google_compute_subnetwork" "observability" {
  count = local.enabled

  project       = var.project_id
  name          = local.name
  region        = var.region
  network       = google_compute_network.observability[0].id
  ip_cidr_range = "10.80.0.0/24"
  # Required so the instance can reach Cloud SQL and Cloud Storage without a public address.
  private_ip_google_access = true
}

# The instance has no external address. Egress for image pulls goes through Cloud NAT.
resource "google_compute_router" "observability" {
  count = local.enabled

  project = var.project_id
  name    = local.name
  region  = var.region
  network = google_compute_network.observability[0].id
}

resource "google_compute_router_nat" "observability" {
  count = local.enabled

  project                            = var.project_id
  name                               = local.name
  router                             = google_compute_router.observability[0].name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"
}

# Only the load balancer's own ranges may reach the service ports. There is no other ingress rule,
# so Prometheus, Loki and Tempo are unreachable from outside the VPC by construction.
resource "google_compute_firewall" "from_load_balancer" {
  count = local.enabled

  project                 = var.project_id
  name                    = "${local.name}-allow-load-balancer"
  network                 = google_compute_network.observability[0].name
  direction               = "INGRESS"
  source_ranges           = ["130.211.0.0/22", "35.191.0.0/16"]
  target_service_accounts = [google_service_account.observability[0].email]

  allow {
    protocol = "tcp"
    # Grafana and MLflow, plus the OTLP ports and the collector health endpoint only when OTLP
    # ingestion is enabled. Nothing else is reachable, from anywhere.
    ports = concat(
      ["3000", "5000"],
      var.enable_otlp_ingress ? ["4317", "4318", "13133"] : [],
    )
  }
}

resource "google_compute_firewall" "deny_other_ingress" {
  count = local.enabled

  project       = var.project_id
  name          = "${local.name}-deny-ingress"
  network       = google_compute_network.observability[0].name
  direction     = "INGRESS"
  priority      = 65000
  source_ranges = ["0.0.0.0/0"]

  deny { protocol = "all" }
}

resource "google_compute_disk" "data" {
  count = local.enabled

  project = var.project_id
  name    = "${local.name}-data"
  type    = "pd-balanced"
  zone    = var.zone
  size    = var.data_disk_gb
}

resource "google_compute_instance" "observability" {
  count = local.enabled

  project                   = var.project_id
  name                      = local.name
  machine_type              = var.machine_type
  zone                      = var.zone
  deletion_protection       = var.instance_deletion_protection
  allow_stopping_for_update = true

  boot_disk {
    initialize_params {
      # Container-Optimized OS: a read-only root and automatic security updates, with Docker and
      # docker compose already present.
      image = "cos-cloud/cos-stable"
      size  = 20
      type  = "pd-balanced"
    }
  }

  attached_disk {
    source      = google_compute_disk.data[0].id
    device_name = "observability-data"
    mode        = "READ_WRITE"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.observability[0].id
    # No access_config block, so the instance has no public address.
  }

  service_account {
    email  = google_service_account.observability[0].email
    scopes = ["cloud-platform"]
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  metadata = {
    # Sign-in to the instance itself is separate from IAP access to the dashboards.
    enable-oslogin         = "TRUE"
    block-project-ssh-keys = "TRUE"
    google-logging-enabled = "TRUE"
    config-bundle          = "gs://${google_storage_bucket.config[0].name}/${var.config_bundle_object}"
    telemetry-bucket       = google_storage_bucket.telemetry[0].name
    compose-url            = var.compose_url
    compose-sha256         = var.compose_sha256
    grafana-secret         = google_secret_manager_secret.grafana_password[0].secret_id
    grafana-user           = var.grafana_admin_user
    grafana-host           = local.grafana_host
    mlflow-host            = local.mlflow_host
    mlflow-instance        = local.mlflow_instance
    mlflow-database-uri    = local.mlflow_database_uri
    startup-script         = file("${path.module}/files/startup.sh")
  }

  tags = ["kratos-observability"]
}

# The password is created empty: Terraform never holds its value, and the user adds a version with
# `gcloud secrets versions add`. The instance reads the latest version at boot.
resource "google_secret_manager_secret" "grafana_password" {
  count = local.enabled

  project   = var.project_id
  secret_id = "kratos-observability-grafana-admin"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_iam_member" "grafana_password" {
  count = local.enabled

  project   = var.project_id
  secret_id = google_secret_manager_secret.grafana_password[0].secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = google_service_account.observability[0].member
}

resource "google_compute_instance_group" "observability" {
  count = local.enabled

  project   = var.project_id
  name      = local.name
  zone      = var.zone
  instances = [google_compute_instance.observability[0].id]

  dynamic "named_port" {
    for_each = merge(
      local.human_services,
      var.enable_otlp_ingress ? { otlp = 4318 } : {},
    )
    content {
      name = named_port.key
      port = named_port.value
    }
  }
}

resource "google_compute_health_check" "observability" {
  for_each = var.enable_observability ? merge(
    {
      grafana = { port = 3000, path = "/api/health" }
      mlflow  = { port = 5000, path = "/health" }
    },
    # The collector's health endpoint, which the cloud overlay publishes and the firewall admits.
    var.enable_otlp_ingress ? { otlp = { port = 13133, path = "/" } } : {},
  ) : {}

  project = var.project_id
  name    = "${local.name}-${each.key}"

  http_health_check {
    port         = each.value.port
    request_path = each.value.path
  }
}

resource "google_compute_backend_service" "human" {
  for_each = var.enable_observability ? local.human_services : {}

  project               = var.project_id
  name                  = "${local.name}-${each.key}"
  protocol              = "HTTP"
  port_name             = each.key
  load_balancing_scheme = "EXTERNAL_MANAGED"
  health_checks         = [google_compute_health_check.observability[each.key].id]

  backend {
    group = google_compute_instance_group.observability[0].id
  }

  # Human access is IAP-only, with a Google-managed OAuth client.
  #
  # A custom client is possible -- the IAP OAuth Admin *API* was shut down on 19 March 2026, but
  # one can still be created by hand in the console. It is not wanted here. Browser access is a
  # single principal inside this project's own organisation, and there is no need for custom
  # consent branding or for users outside it. The managed client is therefore the simpler choice,
  # and it leaves no secret to create, rotate, or hold in Terraform state.
  #
  # `enabled` is what turns IAP on, and it is unconditional. Omitting an OAuth client does not
  # disable it.
  iap {
    enabled = true
  }

  log_config {
    enable      = true
    sample_rate = 1
  }
}

# Workers post OTLP with a Kratos credential, so this backend is not behind IAP. Until the worker
# telemetry credential is decided, it is reachable but authenticated by nothing; see the README.
resource "google_compute_backend_service" "otlp" {
  count = local.otlp

  project               = var.project_id
  name                  = "${local.name}-otlp"
  protocol              = "HTTP"
  port_name             = "otlp"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  health_checks         = [google_compute_health_check.observability["otlp"].id]

  backend {
    group = google_compute_instance_group.observability[0].id
  }

  log_config {
    enable      = true
    sample_rate = 1
  }
}

resource "google_compute_global_address" "observability" {
  count = local.enabled

  project = var.project_id
  name    = local.name
}

resource "google_compute_managed_ssl_certificate" "observability" {
  count = local.enabled

  project = var.project_id
  name    = local.name

  managed {
    domains = concat(
      [local.grafana_host, local.mlflow_host],
      local.otlp == 1 ? [local.otel_host] : [],
    )
  }
}

resource "google_compute_url_map" "observability" {
  count = local.enabled

  project         = var.project_id
  name            = local.name
  default_service = google_compute_backend_service.human["grafana"].id

  host_rule {
    hosts        = [local.grafana_host]
    path_matcher = "grafana"
  }

  host_rule {
    hosts        = [local.mlflow_host]
    path_matcher = "mlflow"
  }

  dynamic "host_rule" {
    for_each = local.otlp == 1 ? [local.otel_host] : []
    content {
      hosts        = [host_rule.value]
      path_matcher = "otlp"
    }
  }

  path_matcher {
    name            = "grafana"
    default_service = google_compute_backend_service.human["grafana"].id
  }

  path_matcher {
    name            = "mlflow"
    default_service = google_compute_backend_service.human["mlflow"].id
  }

  dynamic "path_matcher" {
    for_each = local.otlp == 1 ? [google_compute_backend_service.otlp[0].id] : []
    content {
      name            = "otlp"
      default_service = path_matcher.value
    }
  }
}

resource "google_compute_ssl_policy" "observability" {
  count = local.enabled

  project         = var.project_id
  name            = local.name
  profile         = "MODERN"
  min_tls_version = "TLS_1_2"
}

resource "google_compute_target_https_proxy" "observability" {
  count = local.enabled

  project          = var.project_id
  name             = local.name
  url_map          = google_compute_url_map.observability[0].id
  ssl_certificates = [google_compute_managed_ssl_certificate.observability[0].id]
  ssl_policy       = google_compute_ssl_policy.observability[0].id
}

resource "google_compute_global_forwarding_rule" "https" {
  count = local.enabled

  project               = var.project_id
  name                  = "${local.name}-https"
  target                = google_compute_target_https_proxy.observability[0].id
  ip_address            = google_compute_global_address.observability[0].id
  port_range            = "443"
  load_balancing_scheme = "EXTERNAL_MANAGED"
}

resource "google_iap_web_backend_service_iam_member" "human_access" {
  for_each = var.enable_observability ? local.human_services : {}

  project             = var.project_id
  web_backend_service = google_compute_backend_service.human[each.key].name
  role                = "roles/iap.httpsResourceAccessor"
  member              = var.iap_member
}
