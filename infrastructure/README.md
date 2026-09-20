# GCP infrastructure

Terraform is separated into three independently applied roots:

- `bootstrap`: state bucket and GitHub workload identity. It begins with local state, then migrates itself to GCS.
- `platform`: project APIs, Artifact Registry and the runtime identity.
- `migration`: the separately deployed and executed schema migration job.
- `application`: the Cloud Run service for one immutable image.
- `observability`: the ADR-009 telemetry stack, gated off by default.

This separation lets CI create the image repository before an application image exists. It also prevents routine application releases from refreshing bootstrap identity resources.

The platform root also creates the private durable-artifact bucket and a dedicated create-only
upload signer identity. Cloud Run may invoke IAM Credentials `signBlob` for that identity and may
read object metadata from the bucket; it cannot use the signer as a general runtime identity. The
signer has no JSON key. Reapply the bootstrap root once so the federated deployment identity gains
the Storage administrator and IAM custom-role administrator roles needed to create these resources.

Unverified objects are subject to a seven-day lifecycle deletion rule. The control plane can read,
place a temporary hold on a verified generation, and delete a rejected generation through a narrow
custom bucket role. It cannot create objects directly; upload creation remains isolated to the
dedicated signer identity.

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
users and narrow Cloud SQL IAM grants. The migration identity alone receives the PostgreSQL
`cloudsqlsuperuser` role. The deployment creates and executes its Cloud Run job before applying the
application revision. After applying versioned migrations, the job grants the runtime identity
connect, schema usage, table data and sequence access without schema ownership or alteration rights.

The application root adds a digest-pinned Cloud SQL Auth Proxy v2 sidecar and supplies non-secret
connection metadata to the Rust container. The migration image contains the same pinned proxy and
runs it only for the job's lifetime. Neither path creates or stores a database password.

## Human authentication and the first operator

Configure Google sign-in in Identity Platform for the development project and add the deployed
domain as an authorised domain. Set these GitHub Actions variables together:

| Variable | Value |
|---|---|
| `KRATOS_IDENTITY_PLATFORM_API_KEY` | The development project's browser API key. This identifies the project and is not treated as a secret. |
| `KRATOS_BOOTSTRAP_OPERATOR_EMAIL` | The verified Google account allowed to establish the first operator identity. |

Terraform derives `KRATOS_IDENTITY_PLATFORM_PROJECT_ID` from `GCP_DEVELOPMENT_PROJECT_ID`; it does
not require another GitHub variable. The public `GET /api/v1/auth/config` endpoint exposes only the
browser API key, auth domain and project ID needed by the Firebase browser SDK. It returns `503`
when human authentication is not completely configured.

The browser sends its short-lived Identity Platform ID token to the Rust API. The API submits that
token to Identity Platform's account lookup endpoint and accepts only one enabled account with a
verified email. On the first successful operator request, the configured bootstrap email is bound
to the provider's stable subject and stored with the `operator` role. Subsequent authorisation uses
that stored subject and role. Changing the configured email does not transfer an existing role.

`POST /api/v1/operator/worker-enrolments` returns a 15-minute, single-use worker credential by
default. The plaintext is returned only in that response; PostgreSQL stores its Argon2id verifier.
Human authentication and persistence both fail closed with `503` while their configuration is
absent.

The web console supports 15, 30 and 60 minute enrolments. It displays the credential once and keeps
it only in browser memory; reloading the page requires the operator to issue a replacement.

## Observability cost gate

The `observability` root builds the [ADR-009](../docs/architecture/decisions/009-observability-and-mlflow.md)
stack — Grafana, Prometheus, Loki, Tempo, an OpenTelemetry Collector gateway and MLflow — on one
Compute Engine instance. Its configuration is the bundle in
[`observability/`](../observability/README.md), applied with `compose.cloud.yaml`, which is the
overlay that makes that bundle serve a load balancer and keep its data off the boot disk.

`enable_observability` defaults to `false`, and with the gate closed the root plans **no resources
at all**. Unlike Cloud Run, an instance and a persistent disk bill continuously whether or not
anyone opens a dashboard, so enabling it is the user's decision and no workflow sets it. CI only
formats and validates.

### Who applies this

A human operator applies this root with their own credentials. The federated deployment identity
used by CI is deliberately **not** granted the Compute, Secret Manager, IAP and Storage roles this
root needs: an automated pipeline that could create billable infrastructure or alter an
authentication boundary is a larger blast radius than the convenience is worth.

### Before the first enabled apply

1. **Decide the spend.** One `e2-standard-2`, a 50 GB balanced disk, a load balancer, NAT and
   egress bill continuously. Take the current figures from GCP's price list for your region rather
   than from this file, and set a budget alert first.
2. **Create the IAP OAuth client** in the console and keep its identifier and secret. Terraform does
   not create it, and an enabled plan **fails** without it rather than creating backend services
   with IAP disabled, which would publish Grafana and MLflow unauthenticated.
3. **Decide where the OAuth secret lives.** Terraform holds `oauth_client_secret` in state, so the
   state bucket is exactly as sensitive as the secret. It is the bucket created by the bootstrap
   root, which is private and versioned; treat access to it accordingly.
4. **Decide about MLflow metadata.** `enable_mlflow_database` is a separate gate and needs the
   platform database enabled first.

### Applying

Run from the repository root. The backend prefix keeps this root's state beside the others and must
be unique to it:

```shell
terraform -chdir=infrastructure/observability init \
  -backend-config="bucket=YOUR_STATE_BUCKET" \
  -backend-config="prefix=observability"

terraform -chdir=infrastructure/observability apply \
  -var project_id=YOUR_PROJECT \
  -var domain_name=kratos.bryars.com \
  -var 'iap_member=user:you@example.com' \
  -var enable_observability=true \
  -var oauth_client_id=YOUR_CLIENT_ID
```

Leave `oauth_client_secret` off the command line: Terraform prompts for it, so it does not reach
the shell history or the process list. In PowerShell the same commands work with the quoting
reversed — use `--%` or double quotes around `iap_member`, for example
`terraform -chdir=infrastructure/observability apply -var "iap_member=user:you@example.com"`.

Then, once:

```shell
# The password Terraform never sees.
"YOUR_PASSWORD" | gcloud secrets versions add kratos-observability-grafana-admin --data-file=-

# The bundle the instance runs. Re-upload this to update the stack.
tar -czf observability.tar.gz observability
gcloud storage cp observability.tar.gz \
  "gs://$(terraform -chdir=infrastructure/observability output -raw config_bucket)/bundles/observability-current.tar.gz"
```

Copy the three `required_dns_records` addresses to the DNS provider. Certificate issuance begins
once those names resolve.

**The order above is safe.** Terraform is applied before the bundle and the secret exist, so the
first boot finds neither. The startup script installs a systemd timer, exits cleanly when either
is missing, and the timer retries every five minutes, so the stack starts by itself once you have
uploaded them — no reboot and no second apply. That same timer is what picks up a replaced bundle:
it compares the object's generation and restarts the stack when it changes, so re-uploading is how
the configuration is updated and the instance is never recreated.

### What this root commits to

- The instance has **no public address**. Egress uses Cloud NAT, and the only ingress rule admits
  Google's load-balancer ranges to the published service ports, with a default deny behind it. So
  Prometheus, Loki and Tempo have no route from outside the VPC.
- **Containers cannot reach the instance metadata server.** The startup script rejects traffic to
  `169.254.169.254` from the Docker bridges, so nothing running a workload or a public service can
  mint the instance's token. The Cloud SQL Auth Proxy is the one component that legitimately needs
  that identity, for IAM database login, and it is the one component placed on the **host network**
  so the rule does not cover it. Nothing else is granted the exception, and MLflow reaches it
  through an explicit host-gateway route rather than by reopening metadata to the bridges.
- **The Cloud SQL overlay is applied only when a database exists.** With `enable_mlflow_database`
  false the startup script omits `compose.cloudsql.yaml` entirely, so the stack never references a
  proxy that was not created and MLflow keeps a SQLite store on the persistent disk. `validate.sh`
  asserts that the default rendering contains no proxy at all.
- **Each data directory is owned by the user its image runs as** — Prometheus 65534, Loki and Tempo
  10001, Grafana 472 — because these services are not root and a root-owned bind mount would leave
  their data path unwritable.
- Grafana and MLflow sit behind IAP, restricted to `iap_member`. **OTLP ingestion is a separate
  gate, `enable_otlp_ingress`, and is off**: nothing authenticates a worker sending telemetry yet,
  so publishing it would expose unauthenticated ingestion. Until ADR-009's scoped worker credential
  is decided, leave it off and send telemetry from inside the VPC.
- The instance runs Container-Optimized OS with Secure Boot, vTPM, integrity monitoring and OS
  Login, as a service account that can read its configuration bucket, write telemetry objects and
  log in to Cloud SQL, and deploy nothing.
- Its data disk is only formatted when it carries **no filesystem signature**. `fsck` exiting
  non-zero because it corrected errors never triggers a reformat.
- Compose is fetched as a pinned binary and verified against a recorded SHA-256 before it runs,
  because Container-Optimized OS ships no Compose plugin. The Google Cloud CLI helper, which runs
  on the host network and can read the secret and mint the token, is pinned by digest for the same
  reason.
- Prometheus, Loki, Tempo and Grafana keep their state on the persistent disk; Loki chunks, Tempo
  blocks and MLflow artefacts go to Cloud Storage; MLflow metadata goes to Cloud SQL over the Auth
  Proxy with IAM authentication, so no database password exists.
