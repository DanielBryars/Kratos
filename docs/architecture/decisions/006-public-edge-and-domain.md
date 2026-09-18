# ADR 006: Public edge and domain

- Status: accepted
- Date: 2026-09-18

## Decision

Kratos SHALL use `kratos.bryars.com` as its initial public origin. A GCP global external Application Load Balancer SHALL terminate HTTPS and route requests through a regional serverless network endpoint group to the Cloud Run control plane.

Terraform SHALL reserve the public IPv4 address, provision and renew the managed certificate, configure TLS 1.2 or newer, and redirect HTTP to HTTPS. Cloud Run ingress SHALL reject direct requests from the public internet and accept external traffic only through Cloud Load Balancing.

The DNS provider remains external to the current Terraform scope. The deployment SHALL output the required `A` record. DNS SHALL be added only after Terraform reserves the address.

## Rationale

Cloud Run direct domain mapping is a preview feature, is not available in `europe-west2`, and is not recommended by GCP for production services. The load balancer supports the selected region and provides a stable edge for later request controls, Cloud Armor policy and additional backends.

Serving the React interface and Rust API from one origin avoids browser cross-origin configuration. Users and workers use the same stable endpoint while application authentication and authorization remain enforced by the control plane.

## Consequences

- The external load balancer adds fixed cloud cost and more infrastructure than the default Cloud Run URL.
- A certificate cannot become active until public DNS points the domain at the reserved address.
- Changing the domain requires coordinated certificate and DNS changes.
