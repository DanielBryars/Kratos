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

## Development database cost gate

The platform configuration enables the Cloud SQL and Identity Platform APIs, but does not create a
database by default. This keeps an ordinary deployment from silently adding continuous database
cost. Review [ADR-011](../docs/architecture/decisions/011-metadata-and-human-identity.md), then set
`enable_database=true` only in the environment that should own the development database.

The deployment workflow reads the persistent `GCP_ENABLE_DATABASE` GitHub Actions variable and
defaults it to `false`. Before changing it to `true`, reapply the bootstrap root so the federated
deployment identity receives its Cloud SQL administrator role. To remove an instance, first apply
with `database_deletion_protection=false`; only then set `GCP_ENABLE_DATABASE=false` and apply again.

When enabled, Terraform creates the zonal development PostgreSQL instance, database, IAM database
user and the two narrow Cloud SQL IAM grants for the Cloud Run service account. The application root
then adds a digest-pinned Cloud SQL Auth Proxy v2 sidecar and supplies non-secret connection metadata
to the Rust container. It does not create or store a database password. Schema migrations and the
Rust persistence layer are tested in CI. The deployment workflow does not yet apply migrations to
the persistent instance, so the cost gate SHALL remain off until that migration job is added.

## Human authentication and the first operator

Configure Google sign-in in Identity Platform for the development project and add the deployed
domain as an authorised domain. Set these GitHub Actions variables together:

| Variable | Value |
|---|---|
| `KRATOS_IDENTITY_PLATFORM_API_KEY` | The development project's browser API key. This identifies the project and is not treated as a secret. |
| `KRATOS_BOOTSTRAP_OPERATOR_EMAIL` | The verified Google account allowed to establish the first operator identity. |

The browser sends its short-lived Identity Platform ID token to the Rust API. The API submits that
token to Identity Platform's account lookup endpoint and accepts only one enabled account with a
verified email. On the first successful operator request, the configured bootstrap email is bound
to the provider's stable subject and stored with the `operator` role. Subsequent authorisation uses
that stored subject and role. Changing the configured email does not transfer an existing role.

`POST /api/v1/operator/worker-enrolments` returns a 15-minute, single-use worker credential by
default. The plaintext is returned only in that response; PostgreSQL stores its Argon2id verifier.
Human authentication and persistence both fail closed with `503` while their configuration is
absent.
