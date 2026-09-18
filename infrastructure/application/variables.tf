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

variable "allow_unauthenticated" {
  description = "Temporary public ingress for the scaffold; application authentication is required before worker data is exposed."
  type        = bool
  default     = true
}
