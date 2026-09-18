output "service_url" {
  description = "Public Cloud Run URL."
  value       = google_cloud_run_v2_service.control_plane.uri
}
