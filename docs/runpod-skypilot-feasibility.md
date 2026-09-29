# RunPod via SkyPilot: feasibility review

**Status:** desk research. **Nothing here provisioned, paid for, or authenticated anything.** No
cloud resources were created, no credentials were read or printed, no IAM or quota was changed, and
no Terraform was run. Both paid runs still require a concrete final scope and a separate spend
decision.

**Method.** Every claim about SkyPilot is read from the **installed source of the pinned release**,
`skypilot==0.13.0` (`sky/__init__.py`: `__version__ = '0.13.0'`), in a throwaway container, rather
than from documentation prose. This project has repeatedly paid for asserting what a component does
without reading it, so file and symbol references are given for anything load-bearing. Claims about
RunPod itself come from RunPod's own documentation and are cited at the end.

Each claim below is marked **[evidenced]**, **[inferred]** or **[unknown]**. The unknowns are the
useful part; please read that section before planning a paid run.

---

## Verdict

RunPod is a reasonable place to run a **standalone** short GPU training job through SkyPilot, and
the pinned release supports the one cost control that matters (`autodown`, which terminates).

**It cannot run the existing Kratos agent as it stands.** That is not a tuning problem; it is two
independent architectural mismatches, both evidenced below. Anything that presents RunPod as a
drop-in host for our current worker is wrong.

Separately, **SkyPilot's RunPod provisioning is not idempotent.** It reconciles by pod *name* with a
list-then-create, and sends no idempotency token. That is precisely the property the merged capacity
boundary (PR #105) documents as the single most important thing a real provider must get right, and
the acceptance note for it explicitly says it was never tested against a real provider. This is the
answer: the first real provider does not offer it, so Kratos would have to supply the guarantee
itself.

---

## 1. Pod execution versus our Docker agent — the decisive finding

**A RunPod Pod *is* the container.** There is no VM and no Docker daemon. Our agent's execution
model is an agent container that talks to a Docker socket and runs each job as a sibling container
(`DockerExecutor`, `remove_job_container`). There is no socket to talk to on a Pod. **[evidenced by
absence: nothing in `sky/provision/runpod/` mounts or provisions a Docker socket, and the pod
parameters in `utils.launch` contain no such option.]**

**SkyPilot replaces the image's entrypoint.** This is the part most likely to be assumed away.
`sky/provision/runpod/utils.py::launch` builds `docker_args` as:

```
bash -c 'echo <base64 of setup_cmd> | base64 --decode > init.sh; bash init.sh'
```

and `setup_cmd` is an SSH bootstrap ending in `sleep infinity`: `apt update`, `apt install
openssh-server rsync curl patch -y`, `mkdir -p /var/run/sshd`, permit root login, `ssh-keygen -A`,
append the cluster public key to `authorized_keys`, `service ssh restart`, then `sleep infinity`.
SkyPilot then runs the task over SSH. **[evidenced]**

Three consequences, all of which matter more than the "custom images are supported" headline:

- **Our image's own `CMD`/`ENTRYPOINT` never runs.** Handing SkyPilot
  `ghcr.io/danielbryars/kratos-agent@sha256:…` would start the image and then *not* start the agent.
  So "RunPod accepts custom images as Pod images" is true and does **not** imply our agent runs.
- **The image must be `apt`-based and installable as root.** A distroless, Alpine, or non-root image
  fails during bootstrap. **[evidenced]**
- **Work arrives by SSH, not by our worker protocol.** Nothing polls the Kratos control plane, so
  nothing heartbeats, leases an attempt, or reports a result.

`sky/clouds/runpod.py::get_image_size` returns `0.0` with the comment *"We should change this to
return the docker image size"* — a small corroboration that the image is a container image, not a
machine image. **[evidenced]**

### What this means for integration

There are two coherent designs, and they are very different sizes of job. Neither is proposed here;
both are open decisions.

1. **Treat a Pod as the job, not as a worker.** SkyPilot launches a training image directly; Kratos
   never has an agent there. This discards the agent model for cloud capacity and needs a different
   way to get lineage, observations and artefacts back.
2. **Make the agent run as PID 1 of the Pod and execute jobs in-process.** This needs a non-Docker
   executor — a real change to the agent's execution model, not configuration.

**Do not plan on option 3, "run our agent container and let it use Docker",** because there is no
Docker socket.

---

## 2. Codex's two findings, verified

Both hold, and the autodown question has a precise answer.

**`STOP` is unsupported. [evidenced, twice over]**
`sky/clouds/runpod.py::_CLOUD_UNSUPPORTED_FEATURES` contains
`CloudImplementationFeatures.STOP: 'Stopping not supported.'`, and
`sky/provision/runpod/instance.py::stop_instances` is literally `raise NotImplementedError()`.

**Custom images are accepted as Pod images. [evidenced]** — with the entrypoint caveat in §1, which
is the part that decides whether this is useful to us.

**`autodown` works; `autostop` does not. [evidenced]** This is worth stating precisely because the
reason is not the obvious one. In `sky/core.py`:

- `sky stop` requires `STOP` → refused, with a message directing the user to `sky down`
  (`core.py:1062-1071`).
- `autostop` requires **both** `STOP` and `AUTOSTOP` (`core.py:1176-1179`) → refused because `STOP`
  is unsupported.
- `autodown` requires only `AUTODOWN` (`core.py:1172-1174`), and **`AUTODOWN` is absent from
  RunPod's unsupported list** — the string `AUTODOWN` does not appear in `sky/clouds/runpod.py` at
  all. So it is supported.

**So the only automatic cost control is "idle for N minutes, then terminate".** For spend safety
that is the better of the two: terminate stops GPU billing outright. The cost is that there is no
"park the disk, release the GPU" mode.

### The consequence nobody has costed yet

Terminate destroys the pod's disk (§5). Our protocol 1.3 staging cache is digest-keyed local
storage, so **every run on fresh capacity re-downloads its dataset.** For the current bounded smoke
model that is trivial. For anything real it is a per-run time and egress cost that needs a number
before it is designed around. The obvious fix — a RunPod network volume as a persistent cache — is
also a standing charge that no teardown removes (§5). That trade is a decision, not a detail.

---

## 3. Resource discovery after a lost response — not idempotent

`sky/provision/runpod/instance.py::run_instances` reconciles like this. **[all evidenced]**

1. Poll until no pods matching this cluster are in `CREATED` or `RESTARTING`.
2. List pods in `RUNNING` whose `name` is `{cluster_name_on_cloud}-head` or `-worker`
   (`_filter_instances` matches on the `name` field).
3. `to_start_count = config.count - len(exist_instances)`; create only the shortfall.

And `utils.launch` sends these pod parameters: `name`, `image_name`, `container_disk_in_gb`,
`country_code`, `data_center_id`, `ports`, `support_public_ip`, `docker_args`, `template_id`, plus
GPU fields. **There is no idempotency token of any kind.**

So the safety property is *client-side, name-based, read-then-create*. The pending-status wait in
step 1 genuinely mitigates the common case and deserves credit: a pod created by a lost response
will usually be found while still `CREATED`. But that is not the same promise as an idempotent
create, and it is weaker than the contract `CapacityProvider::provision` states — *"called twice
with one key it returns the same `external_id` and creates nothing further"*.

Known gaps in that reconciliation, from the code:

- A second concurrent provisioner sees the same list and computes the same shortfall. Nothing
  serialises them provider-side. **[evidenced]** Kratos already handles this with an advisory lock
  per request, so our boundary covers it — but SkyPilot alone does not.
- If a created pod is in neither the pending nor `RUNNING` set when step 2 runs, it is not counted,
  and another is created. **[inferred from the status filters; I have not enumerated every RunPod pod
  status, so I cannot say how reachable this is — see Unknowns.]**
- Pod `name` is not a unique key on RunPod, so two pods can carry the same name. **[inferred: no
  uniqueness constraint appears in the create path, and `_filter_instances` returns a dict keyed by
  instance id, implying multiple matches are expected and tolerated.]**

**Implication for Kratos.** Our boundary was built for exactly this and mostly fits: the
attempt-derived idempotency key would become the SkyPilot cluster name, giving stable name-based
reconciliation, and `external_id` would be the pod id. The `unreconciled` state added in #105 is
what covers the residual risk — a lost response whose pod we cannot name. **That state stops being
theoretical here**, so the operator surfaces (`ambiguous_provisions`, `outstanding_releases`) need
the console and alerting that the acceptance note records as missing *before* a paid run, not after.

---

## 4. GPU naming, region and security selection

**Instance-type naming. [evidenced]** `utils.launch` parses `instance_type` as
`{count}x_{GPU}_{CLOUD_TYPE}` — `instance_type.split('_')[0].replace('x','')` is the GPU count,
`[1]` indexes `GPU_NAME_MAP`, `[2]` is the cloud type. So a request looks like
`1x_A100-80GB_SECURE`.

`GPU_NAME_MAP` maps short names to RunPod's exact strings, e.g. `A100-80GB` →
`NVIDIA A100 80GB PCIe`, `A100-80GB-SXM` → `NVIDIA A100-SXM4-80GB`, `RTX4090` →
`NVIDIA GeForce RTX 4090`, `B200` → `NVIDIA B200`, `MI300X` → `AMD Instinct MI300X OAM`.

**`RTX5090` is in the map. [evidenced]** The map has 41 keys in 0.13.0 and includes `RTX5090`,
`RTX5080` and `RTX5000-Ada`, alongside `H100`, `H100-SXM`, `H100-NVL`, `H200-SXM` and `B200`. So
matching THESHED2's GPU class in the cloud is expressible — `1x_RTX5090_SECURE` — which makes a
like-for-like comparison against the local worker possible in principle. Whether any host actually
offers it is a separate, dynamic question I did not query (see Unknowns).

**Region is a country code; zone is a data centre. [evidenced]** `utils.launch` passes
`country_code=region` and `data_center_id=zone`. SkyPilot's "region" for RunPod is therefore
coarser than for GCP, which matters if data residency ever does.

**Security selection is the third field of the instance type. [evidenced]**
`sky/provision/runpod/api/commands.py:75` validates `cloud_type not in ['ALL', 'COMMUNITY',
'SECURE']`.

Per RunPod's own documentation: Secure Cloud is "T3/T4 data centers" with "High redundancy" and is
"Best for Production, sensitive data"; Community Cloud is "Peer-to-peer providers" with "Variable"
reliability, suited to "Cost-sensitive workloads", and RunPod "is no longer accepting new hosts for
Community Cloud".

**Recommendation:** pin `SECURE` explicitly in any configuration we write. Not for our current
synthetic data, which is worthless, but because the selector sits inside an instance-type string
where a default or a copied example is easy to get wrong, and the same string will later be used
with real data.

---

## 5. Autodown, teardown, and what keeps charging

**What SkyPilot's teardown does. [evidenced]** `instance.py::terminate_instances` lists all pods for
the cluster, calls `utils.remove(inst_id)` on each, then — and this corrects an assumption I made
before reading it — **deletes the pod template and the container registry auth** it created
(`delete_pod_template`, `delete_register_auth`). Those do not accumulate on a clean teardown. They
would only be left behind if terminate never ran.

**What RunPod charges, from RunPod's documentation:**

| Storage | While running | While stopped | **After terminate** |
|---|---|---|---|
| Container disk | $0.10/GB/month | not billed | **cleared, no charge** |
| Volume disk | $0.10/GB/month | $0.20/GB/month | **deleted, no charge** |
| Network volume | $0.07/GB/month | $0.07/GB/month | **retained, still billed** |
| Global volume | $0.09/GB/month + IOPS | same | **retained, still billed** |

Two things follow.

**Good news: a default `sky down` leaves no residual storage charge.** SkyPilot only attaches a
network volume when `VolumeMounts` is configured (`run_instances`), and nothing configures it by
default, so container and volume disk go with the pod. **[evidenced + cited]**

**The spend trap: a network volume is a standing charge that no teardown removes.** It is the
obvious answer to the re-download cost in §2, and RunPod's own documentation says it is "retained
independently" after termination. SkyPilot's terminate does not delete it. So the moment anyone
adds a dataset cache volume, we are paying monthly for it whether or not a single job runs, and
nothing in our tooling would show that. **If we ever add one, deleting it needs to be somebody's
explicit job with a named owner.**

Note also that the doubled stopped-volume rate is unreachable through SkyPilot, since it cannot
stop pods — an incidental benefit of the `STOP` gap.

---

## 6. Credentials

**SkyPilot copies the RunPod API key onto the Pod. [evidenced]**
`sky/clouds/runpod.py`: `_CREDENTIAL_FILE = 'config.toml'`, and `get_credential_file_mounts` returns
`{'~/.runpod/config.toml': '~/.runpod/config.toml'}` — local path to remote path.

So the key that can create and destroy pods is present inside the Pod, which is also where untrusted
training code runs. For a synthetic experiment on a throwaway key that is acceptable. As a pattern
for anything Kratos runs on a user's behalf it is not, and it should be an explicit constraint on any
design: **the capacity credential must not be reachable from the workload.** Our architecture already
separates these — the worker telemetry credential (ADR-016) exists for this reason — so this is a
reason to keep provisioning outside the workload Pod rather than a reason to avoid RunPod.

**Private images require storing a registry credential in RunPod. [evidenced]**
`_create_template_for_docker_login` calls `runpod.runpod.create_container_registry_auth(name=…,
username=…, password=…)` and creates a per-cluster template. Pulling our private GHCR image would
put a GHCR credential into our RunPod account for the pod's lifetime. It is deleted on clean teardown
(§5). A public image, or an image copied to a registry we do not mind exposing to RunPod, avoids the
question entirely — worth preferring for the first experiment.

**API key capabilities.** RunPod offers three levels: **All**, **Restricted** (per-endpoint: None /
Restricted / Read-Write / Read-Only), and **Read Only**.

SkyPilot needs to create pods, terminate pods, list pods, read GPU types, and — for private images —
create and delete container registry auths and templates. **Read Only is therefore insufficient.**
Whether **Restricted** can express exactly this is **[unknown]**: RunPod's documentation describes
restricted scoping in terms of *Serverless endpoints*, while SkyPilot drives Pods through the
GraphQL API, so I cannot claim a minimal scope exists for our use. Plan on a key with write access,
**created fresh for this experiment and revoked afterwards**, and treat narrowing it as an open
question rather than a solved one. RunPod also notes that keys issued before 11 November 2024 are
legacy and have full AI-API access regardless — so use a new key, not an old one.

---

## 7. A single short synthetic workload

The shape I would propose for the first paid run, deliberately minimal and touching no Kratos
production path:

- **One `sky launch`** of a standalone SkyPilot task, `--cloud runpod`, a single GPU, `SECURE`, with
  `--down` and a short `--idle-minutes-to-autostop` so the pod terminates on its own even if the
  session dies. Since `autostop` is unsupported, `--down` is not optional — it is the only working
  form.
- **A public PyTorch image**, so no registry credential reaches RunPod (§6).
- **Synthetic data generated on the pod.** No dataset staging, no GCS, no private data — which also
  sidesteps `STORAGE_MOUNTING` being unsupported (§8).
- **A few hundred steps**, writing a loss curve and a small checkpoint, then exit.
- **Success is operational, not scientific:** it provisions, runs, terminates by itself, and the
  RunPod console shows no surviving pod and no network volume afterwards.

What that run would *establish* is narrow and worth stating: that our account, key, quota and region
selection work, and that autodown terminates. It would tell us **nothing** about agent integration,
because the agent is not involved. It would also not test the idempotency gap in §3 — doing that
deliberately means provoking a lost response, which I would not do on a paid provider without
deciding in advance how we clean up an orphaned pod.

---

## 8. Other constraints from the pinned release

All **[evidenced]** from `_CLOUD_UNSUPPORTED_FEATURES`:

- **`MULTI_NODE`** — not supported, *"as the interconnection among nodes are non-trivial on
  RunPod"*. Single node only.
- **`STORAGE_MOUNTING`** — object stores cannot be mounted: *"To read data from object stores on
  RunPod, use `mode: COPY` to copy the data to local disk."* Our dataset path is GCS, so any real
  integration copies rather than mounts. That is compatible with our digest-keyed staging design,
  which already copies, but it removes "just mount the bucket" as an option.
- **`LOCAL_DISK`**, **`CUSTOM_DISK_TIER`**, **`CUSTOM_NETWORK_TIER`**, **`CUSTOM_MULTI_NETWORK`**,
  **`HIGH_AVAILABILITY_CONTROLLERS`** — all unsupported.

---

## Unknowns, and what I did not verify

I would rather name these than let the document read as more complete than it is.

1. **Whether any host actually offers an RTX 5090**, and at what price. The name is expressible
   (§4); availability is dynamic and I did not query it.
2. **Whether a `Restricted` API key can express SkyPilot's Pod operations** (§6). Documented
   granularity is described for Serverless endpoints; SkyPilot uses GraphQL.
3. **The full RunPod pod status enumeration**, which bounds how reachable the §3 reconciliation gap
   really is. I know the filters SkyPilot uses, not the set of states RunPod can report.
4. **Actual GPU availability and price for any specific type**, which is dynamic and which I did not
   query — no account, and querying availability was outside the read-only scope I was given.
5. **Whether the SSH bootstrap succeeds on a CUDA base image** in practice. It requires `apt` and
   root; most PyTorch/CUDA images satisfy that, but I have not run it.
6. **Nothing about GCP.** Codex owns the GCP preflight, and the note that global GPU quota remains
   zero is theirs, not independently checked by me.
7. **Whether any of this survives a SkyPilot upgrade.** Every code claim is pinned to 0.13.0. The
   `STOP` gap in particular is an adapter limitation, not a RunPod one, so it could change.

---

## Sources

**Primary, read directly:** installed source of `skypilot==0.13.0` —
`sky/__init__.py`, `sky/clouds/runpod.py`, `sky/clouds/cloud.py`, `sky/core.py`,
`sky/provision/runpod/instance.py`, `sky/provision/runpod/utils.py`,
`sky/provision/runpod/api/commands.py`.

**RunPod documentation:**

- API key permission levels — <https://docs.runpod.io/get-started/api-keys>
- Storage types and billing — <https://docs.runpod.io/pods/storage/types>
- Secure Cloud versus Community Cloud — <https://docs.runpod.io/pods/choose-a-pod>
