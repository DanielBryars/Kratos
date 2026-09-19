variable "project_id" {
  description = "GCP project hosting the development control plane."
  type        = string
}

variable "region" {
  description = "Primary GCP region."
  type        = string
  default     = "europe-west2"
}

variable "container_image" {
  description = "Immutable Kratos container image reference."
  type        = string
}

variable "runtime_service_account" {
  description = "Service account email used by the Cloud Run revision."
  type        = string
}

variable "domain_name" {
  description = "Public DNS name served by the HTTPS load balancer."
  type        = string

  validation {
    condition     = can(regex("^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+$", var.domain_name))
    error_message = "domain_name must be a lowercase fully qualified DNS name without a trailing dot."
  }
}

variable "allow_unauthenticated" {
  description = "Allow public requests through the HTTPS load balancer; application authentication is required before worker data is exposed."
  type        = bool
  default     = true
}

variable "identity_platform_api_key" {
  description = "Public browser API key identifying the Identity Platform project; empty disables human authentication."
  type        = string
  default     = ""

  validation {
    condition     = (var.identity_platform_api_key == "") == (var.bootstrap_operator_email == "")
    error_message = "identity_platform_api_key and bootstrap_operator_email must be set together."
  }
}

variable "bootstrap_operator_email" {
  description = "Verified Google email allowed to create the first stable operator identity; empty disables human authentication."
  type        = string
  default     = ""

  validation {
    condition     = var.bootstrap_operator_email == "" || can(regex("^[^@\\s]+@[^@\\s]+\\.[^@\\s]+$", var.bootstrap_operator_email))
    error_message = "bootstrap_operator_email must be empty or a valid email address."
  }
}

variable "database_enabled" {
  description = "Attach the passwordless Cloud SQL Auth Proxy sidecar and database connection metadata."
  type        = bool
  default     = false
}

variable "database_connection_name" {
  description = "Cloud SQL instance connection name when database_enabled is true."
  type        = string
  default     = ""

  validation {
    condition     = !var.database_enabled || length(trimspace(var.database_connection_name)) > 0
    error_message = "database_connection_name must be set when database_enabled is true."
  }
}

variable "database_iam_username" {
  description = "PostgreSQL IAM username when database_enabled is true."
  type        = string
  default     = ""

  validation {
    condition     = !var.database_enabled || length(trimspace(var.database_iam_username)) > 0
    error_message = "database_iam_username must be set when database_enabled is true."
  }
}

variable "cloud_sql_proxy_image" {
  description = "Immutable multi-architecture Cloud SQL Auth Proxy v2 image."
  type        = string
  default     = "gcr.io/cloud-sql-connectors/cloud-sql-proxy@sha256:88501f0a695a586988add1b8a206fdf3f29f9a1a3deeb9b45ef2b1481ea6be83"

  validation {
    condition     = can(regex("@sha256:[0-9a-f]{64}$", var.cloud_sql_proxy_image))
    error_message = "cloud_sql_proxy_image must use an immutable SHA256 digest."
  }
}
