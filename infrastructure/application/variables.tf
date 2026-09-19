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
