variable "project_id" {
  description = "GCP project hosting the development control plane."
  type        = string
}

variable "region" {
  description = "Primary GCP region."
  type        = string
  default     = "europe-west2"
}

variable "enable_database" {
  description = "Create the continuously billed development Cloud SQL instance and its IAM access."
  type        = bool
  default     = false
}

variable "database_deletion_protection" {
  description = "Protect an enabled Cloud SQL instance from accidental Terraform deletion."
  type        = bool
  default     = true
}

variable "artifact_browser_origins" {
  description = "Exact browser origins allowed to upload and preview dataset objects directly."
  type        = list(string)
  default     = ["https://kratos.bryars.com"]

  validation {
    condition = alltrue([
      for origin in var.artifact_browser_origins : startswith(origin, "https://") && !endswith(origin, "/")
    ])
    error_message = "Artifact browser origins must be exact HTTPS origins without a trailing slash."
  }
}
