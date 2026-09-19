# GCP infrastructure

Terraform is separated into three independently applied roots:

- `bootstrap`: state bucket and GitHub workload identity. It begins with local state, then migrates itself to GCS.
- `platform`: project APIs, Artifact Registry and the runtime identity.
- `application`: the Cloud Run service for one immutable image.

This separation lets CI create the image repository before an application image exists. It also prevents routine application releases from refreshing bootstrap identity resources.

## Initial bootstrap

Run these commands inside the development container after authenticating with `gcloud auth application-default login`:

```shell
cp infrastructure/bootstrap/terraform.tfvars.example infrastructure/bootstrap/terraform.tfvars
# Edit the three project/bucket values locally. The file is ignored by Git.

terraform -chdir=infrastructure/bootstrap init -reconfigure
terraform -chdir=infrastructure/bootstrap apply

cp infrastructure/bootstrap/backend_override.tf.example \
  infrastructure/bootstrap/backend_override.tf
terraform -chdir=infrastructure/bootstrap init -migrate-state \
  -backend-config="bucket=YOUR_STATE_BUCKET" \
  -backend-config="prefix=bootstrap"
```

The bootstrap root deliberately starts with Terraform's local backend because the GCS bucket does not exist yet. The ignored `backend_override.tf` switches that same root to GCS only after the first apply, allowing Terraform to migrate the newly created local state. Keep the override file in the working copy for later bootstrap changes.

Copy the three bootstrap outputs into GitHub repository **Actions variables**:

| Variable | Bootstrap output |
|---|---|
| `GCP_TERRAFORM_STATE_BUCKET` | `state_bucket_name` |
| `GCP_WORKLOAD_IDENTITY_PROVIDER` | `workload_identity_provider` |
| `GCP_DEPLOYMENT_SERVICE_ACCOUNT` | `deployment_service_account` |

Also create `GCP_DEVELOPMENT_PROJECT_ID`, `GCP_REGION` and `KRATOS_DOMAIN_NAME` repository variables. These are identifiers, not secret keys. Do not create or upload a service-account JSON key.

The workload identity provider accepts GitHub OIDC tokens only when both the repository and `refs/heads/main` match. Feature branches and pull-request workflows cannot impersonate the deployment service account even if they request GitHub's `id-token: write` permission.

The application deployment reserves a global IPv4 address, provisions a Google-managed certificate, and places an external HTTPS load balancer in front of Cloud Run. After the first application apply, copy the `required_dns_record` output to the DNS provider for the domain. Certificate activation begins after that record resolves to the load balancer address.

Cloud Run accepts internet traffic only through the load balancer. Direct public requests to its default URI are rejected by its ingress policy. Port 80 redirects to HTTPS; the HTTPS frontend requires TLS 1.2 or newer.

The application is temporarily public while it exposes only scaffold health, version and documentation routes. Application authentication SHALL be added before worker registration or any private data endpoint is deployed.
