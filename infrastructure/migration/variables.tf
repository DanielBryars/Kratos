variable "project_id" {
  description = "GCP project hosting the development control plane."
  type        = string
}

variable "region" {
  description = "Primary GCP region."
  type        = string
  default     = "europe-west2"
}

variable "database_enabled" {
  description = "Create the migration job only when the persistent database cost gate is enabled."
  type        = bool
  default     = false
}

variable "container_image" {
  description = "Immutable Kratos image containing the migration binary and Cloud SQL Auth Proxy."
  type        = string
}

variable "migration_service_account" {
  description = "Service account used only by the database migration job."
  type        = string

  validation {
    condition     = !var.database_enabled || length(trimspace(var.migration_service_account)) > 0
    error_message = "migration_service_account must be set when database_enabled is true."
  }
}

variable "database_connection_name" {
  description = "Cloud SQL instance connection name."
  type        = string
  default     = ""

  validation {
    condition     = !var.database_enabled || length(trimspace(var.database_connection_name)) > 0
    error_message = "database_connection_name must be set when database_enabled is true."
  }
}

variable "migration_database_iam_username" {
  description = "Passwordless PostgreSQL username used to own and migrate the schema."
  type        = string
  default     = ""

  validation {
    condition     = !var.database_enabled || length(trimspace(var.migration_database_iam_username)) > 0
    error_message = "migration_database_iam_username must be set when database_enabled is true."
  }
}

variable "application_database_iam_username" {
  description = "Passwordless PostgreSQL runtime username receiving least-privilege data grants."
  type        = string
  default     = ""

  validation {
    condition     = !var.database_enabled || length(trimspace(var.application_database_iam_username)) > 0
    error_message = "application_database_iam_username must be set when database_enabled is true."
  }
}
