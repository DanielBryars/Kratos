variable "bootstrap_project_id" {
  description = "Existing GCP project that owns Terraform state and deployment identity."
  type        = string
}

variable "development_project_id" {
  description = "Existing GCP development project managed by the deployment identity."
  type        = string
}

variable "state_bucket_name" {
  description = "Globally unique GCS bucket name for Terraform state."
  type        = string
}

variable "region" {
  description = "Primary GCP region."
  type        = string
  default     = "europe-west2"
}

variable "github_owner" {
  description = "GitHub repository owner used in workload identity conditions."
  type        = string
  default     = "DanielBryars"
}

variable "github_repository" {
  description = "GitHub repository name used in workload identity conditions."
  type        = string
  default     = "Kratos"
}
