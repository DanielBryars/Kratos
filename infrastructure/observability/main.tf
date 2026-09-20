# The ADR-009 observability stack on one Compute Engine instance.
#
# Everything here is behind var.enable_observability, which defaults to false, because an instance
# and a persistent disk bill continuously. Applying with the gate closed creates nothing.
#
# Only Grafana, MLflow and the OTLP gateway are reachable, all through an HTTPS load balancer.
# Grafana and MLflow sit behind Identity-Aware Proxy so a human must sign in before reaching the
# service itself. Prometheus, Loki and Tempo have no route from outside the VPC, which is what
# ADR-009 requires of the backends.

locals {
  enabled        = var.enable_observability ? 1 : 0
  mlflow_db      = var.enable_observability && var.enable_mlflow_database ? 1 : 0
  name           = "kratos-observability"
  grafana_host   = "grafana.${var.domain_name}"
  mlflow_host    = "mlflow.${var.domain_name}"
  otel_host      = "otel.${var.domain_name}"
  human_services = { grafana = 3000, mlflow = 5000 }
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
  count = local.mlflow_db

  project = var.project_id
  role    = "roles/cloudsql.client"
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
    ports    = ["3000", "5000", "4317", "4318"]
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
    user-data              = local.cloud_init
  }

  tags = ["kratos-observability"]

  lifecycle {
    ignore_changes = [metadata["user-data"]]
  }
}

# Mount the data disk, fetch the configuration bundle and start it. The bundle, not this file, holds
# the service configuration, so replacing it does not recreate the instance.
locals {
  cloud_init = <<-EOT
    #cloud-config

    bootcmd:
      - fsck.ext4 -tvy /dev/disk/by-id/google-observability-data || mkfs.ext4 -F /dev/disk/by-id/google-observability-data
      - mkdir -p /mnt/disks/data
      - mount -o discard,defaults /dev/disk/by-id/google-observability-data /mnt/disks/data

    write_files:
      - path: /etc/systemd/system/kratos-observability.service
        content: |
          [Unit]
          Description=Kratos observability stack
          Wants=network-online.target
          After=network-online.target

          [Service]
          Type=oneshot
          RemainAfterExit=true
          WorkingDirectory=/mnt/disks/data/bundle
          ExecStartPre=/bin/mkdir -p /mnt/disks/data/bundle
          ExecStartPre=/bin/sh -c 'docker run --rm -v /mnt/disks/data:/data google/cloud-sdk:slim \
            gcloud storage cp "$(curl -sf -H Metadata-Flavor:Google \
            http://metadata.google.internal/computeMetadata/v1/instance/attributes/config-bundle)" \
            /data/bundle.tar.gz'
          ExecStartPre=/bin/tar -xzf /mnt/disks/data/bundle.tar.gz -C /mnt/disks/data/bundle --strip-components=1
          ExecStart=/usr/bin/docker compose up -d --remove-orphans

          [Install]
          WantedBy=multi-user.target

    runcmd:
      - systemctl daemon-reload
      - systemctl enable --now kratos-observability.service
  EOT
}

resource "google_compute_instance_group" "observability" {
  count = local.enabled

  project   = var.project_id
  name      = local.name
  zone      = var.zone
  instances = [google_compute_instance.observability[0].id]

  dynamic "named_port" {
    for_each = merge(local.human_services, { otlp = 4318 })
    content {
      name = named_port.key
      port = named_port.value
    }
  }
}

resource "google_compute_health_check" "observability" {
  for_each = var.enable_observability ? {
    grafana = { port = 3000, path = "/api/health" }
    mlflow  = { port = 5000, path = "/health" }
    otlp    = { port = 13133, path = "/" }
  } : {}

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

  # Human access is IAP-only. Without an OAuth client the service is created with IAP disabled,
  # which would publish it unauthenticated, so the plan requires the client identifier.
  iap {
    enabled              = true
    oauth2_client_id     = var.oauth_client_id
    oauth2_client_secret = var.oauth_client_secret
  }

  log_config {
    enable      = true
    sample_rate = 1
  }
}

# Workers post OTLP with a Kratos credential, so this backend is not behind IAP. Until the worker
# telemetry credential is decided, it is reachable but authenticated by nothing; see the README.
resource "google_compute_backend_service" "otlp" {
  count = local.enabled

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
    domains = [local.grafana_host, local.mlflow_host, local.otel_host]
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

  host_rule {
    hosts        = [local.otel_host]
    path_matcher = "otlp"
  }

  path_matcher {
    name            = "grafana"
    default_service = google_compute_backend_service.human["grafana"].id
  }

  path_matcher {
    name            = "mlflow"
    default_service = google_compute_backend_service.human["mlflow"].id
  }

  path_matcher {
    name            = "otlp"
    default_service = google_compute_backend_service.otlp[0].id
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
