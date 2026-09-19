output "service_url" {
  description = "Cloud Run service URI; ingress rejects direct requests from the public internet."
  value       = google_cloud_run_v2_service.control_plane.uri
}

output "public_url" {
  description = "Public HTTPS address for Kratos."
  value       = "https://${var.domain_name}"
}

output "load_balancer_ip" {
  description = "IPv4 address to use for the domain's DNS A record."
  value       = google_compute_global_address.control_plane.address
}

output "required_dns_record" {
  description = "DNS record that must exist before the managed certificate can become active."
  value       = "${var.domain_name} A ${google_compute_global_address.control_plane.address}"
}
