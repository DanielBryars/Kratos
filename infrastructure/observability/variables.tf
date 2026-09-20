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

variable "oauth_client_id" {
  description = "IAP OAuth client identifier. Created by the user; not managed by Terraform."
  type        = string
  default     = ""
}

variable "oauth_client_secret" {
  description = "IAP OAuth client secret. Created by the user; not managed by Terraform."
  type        = string
  default     = ""
  sensitive   = true
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
