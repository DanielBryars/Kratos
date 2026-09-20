output "enabled" {
  description = "Whether this apply created the continuously billed observability resources."
  value       = var.enable_observability
}

output "required_dns_records" {
  description = <<-EOT
    A records to create once, after the first enabled apply, before certificates issue.

    Exactly the names the managed certificate requests, which is why `otel.` appears only when
    OTLP ingestion is enabled. Listing it unconditionally sent an operator to create a record for
    a host with no backend and no certificate, where the only symptom is a name that resolves and
    then fails.
  EOT
  value = var.enable_observability ? merge(
    {
      "grafana.${var.domain_name}" = google_compute_global_address.observability[0].address
      "mlflow.${var.domain_name}"  = google_compute_global_address.observability[0].address
    },
    var.enable_otlp_ingress ? {
      "otel.${var.domain_name}" = google_compute_global_address.observability[0].address
    } : {},
  ) : {}
}

output "config_bucket" {
  description = "Bucket the instance reads its observability bundle from."
  value       = var.enable_observability ? google_storage_bucket.config[0].name : ""
}

output "telemetry_bucket" {
  description = "Bucket holding Loki chunks, Tempo blocks and MLflow artefacts."
  value       = var.enable_observability ? google_storage_bucket.telemetry[0].name : ""
}

output "instance_service_account" {
  description = "Identity the instance runs as."
  value       = var.enable_observability ? google_service_account.observability[0].email : ""
}
