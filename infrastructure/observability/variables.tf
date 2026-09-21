variable "project_id" {
  description = "GCP project hosting the development control plane."
  type        = string
}

variable "region" {
  description = "Primary GCP region."
  type        = string
  default     = "europe-west2"
}

variable "zone" {
  description = "Zone for the single observability instance."
  type        = string
  default     = "europe-west2-a"
}

variable "enable_observability" {
  description = <<-EOT
    Create the continuously billed observability instance, its disk, buckets and load balancer.
    Left false, this root plans and applies to nothing. Review the cost note in the README before
    enabling it: the instance and its disk bill whether or not anyone is looking at a dashboard.
  EOT
  type        = bool
  default     = false
}

variable "instance_deletion_protection" {
  description = "Protect an enabled observability instance from accidental Terraform deletion."
  type        = bool
  default     = true
}

variable "machine_type" {
  description = <<-EOT
    Instance size. The measured stack idles at about 1 GiB across six services, so 8 GiB leaves
    room for query load and retention growth. A 4 GiB machine is not sufficient.
  EOT
  type        = string
  default     = "e2-standard-2"
}

variable "data_disk_gb" {
  description = "Persistent disk for Prometheus, and for Loki and Tempo local state."
  type        = number
  default     = 50

  validation {
    condition     = var.data_disk_gb >= 20
    error_message = "The disk must hold the configured Prometheus retention with headroom."
  }
}

variable "domain_name" {
  description = "Parent domain. The stack is published under grafana., mlflow. and otel. of it."
  type        = string
}

variable "iap_member" {
  description = <<-EOT
    The single principal allowed through Identity-Aware Proxy to Grafana and MLflow, as an IAM
    member string such as "user:someone@example.com". Human access is IAP-only; no service
    publishes an unauthenticated entry point.
  EOT
  type        = string
}


variable "enable_otlp_ingress" {
  description = <<-EOT
    Publish the OTLP ingestion endpoint. Separate from enable_observability and off by default:
    nothing authenticates a worker sending telemetry yet. ADR-009 requires a scoped, revocable
    worker credential and ADR-015 records that decision as open, so enabling this would expose an
    unauthenticated ingestion endpoint to the internet.
  EOT
  type        = bool
  default     = false
}

variable "grafana_admin_user" {
  description = "Grafana administrator account name. Its password lives in Secret Manager."
  type        = string
  default     = "kratos-admin"
}

variable "compose_url" {
  description = "Docker Compose binary the instance fetches, since Container-Optimized OS has none."
  type        = string
  default     = "https://github.com/docker/compose/releases/download/v5.5.1/docker-compose-linux-x86_64"
}

variable "compose_sha256" {
  description = <<-EOT
    SHA-256 of that binary. The instance refuses to run a download that does not match, so the
    startup path stays pinned rather than trusting whatever the URL serves that day.
  EOT
  type        = string
  default     = "db1889184726840f75c4f9c001048430d4f25b3be3cb084d3ddd762bc0aed576"
}

variable "config_bundle_object" {
  description = <<-EOT
    Object name, inside the created configuration bucket, of the tarred observability/ bundle the
    instance unpacks and runs. Uploading it is a deployment step, not a Terraform resource, so the
    configuration can be replaced without recreating the instance.
  EOT
  type        = string
  default     = "bundles/observability-current.tar.gz"
}

variable "deployment_service_account" {
  description = <<-EOT
    Email of the CI deployment service account that uploads the configuration bundle, granted
    write access to the configuration bucket. Empty means no automated deployment: the bundle is
    uploaded by hand as infrastructure/README.md describes, and no binding is created.
  EOT
  type        = string
  default     = ""
}

variable "mlflow_database_instance" {
  description = <<-EOT
    Name of the existing Cloud SQL instance that holds MLflow metadata, so it is covered by the
    backups the platform root already configures. Empty means the platform database is not
    enabled and MLflow metadata has nowhere durable to live.
  EOT
  type        = string
  default     = "kratos"
}

variable "enable_mlflow_database" {
  description = "Create the MLflow database and IAM user on the existing Cloud SQL instance."
  type        = bool
  default     = false
}


variable "billing_account" {
  description = <<-EOT
    Billing account that pays for this stack, as the bare identifier such as "012345-6789AB-CDEF01".
    Required when enable_budget is true. Find it with `gcloud billing projects describe PROJECT
    --format='value(billingAccountName)'`.
  EOT
  type        = string
  default     = ""
}

variable "enable_budget" {
  description = <<-EOT
    Create a billing budget covering this project. On by default: a stack that bills continuously
    should not do so unwatched. It raises alerts; it does not stop spending. See the README.
  EOT
  type        = bool
  default     = true
}

variable "monthly_budget" {
  description = <<-EOT
    Budget amount per month, in the billing account's own currency, so it is pounds on a sterling
    account and dollars on a US one. Alerts fire at half, ninety per cent and the whole amount, and
    again when spend is forecast to exceed it.
  EOT
  type        = number
  default     = 300

  validation {
    condition     = var.monthly_budget > 0
    error_message = "A budget of zero would alert immediately and tell you nothing."
  }
}
