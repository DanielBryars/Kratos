# Kratos worker agent

The worker agent runs inside the Linux execution environment on a GPU host. It reports detected
host and GPU capabilities without opening an inbound network port.

From this directory, inspect the current environment with:

```shell
uv run kratos-agent inspect
```

The command uses `nvidia-smi` when it is available. Detection proves that the device is visible;
it deliberately reports GPU computation health as `unverified`. A later controlled health-check
container must execute a real GPU computation before the control plane can mark the worker healthy.

The normal first-run flow requires no copied credential. The agent generates its device key in the
persistent state volume, radios in, and prints a short code. Compare that code with the pending
machine in the authenticated operator console before approving it:

On a Windows GPU host with Docker Desktop and NVIDIA GPU support, run the checked-in installer from
the repository root in PowerShell:

```powershell
.\workers\agent\install-windows.ps1 -DisplayName "Home GPU 2" -AgentHostname "HOME-GPU-02"
```

The installer checks Docker, refuses to replace an existing agent, pulls the public image, resolves
the mutable `edge` tag to its immutable digest, verifies that an NVIDIA GPU is visible inside the
Linux container, creates the persistent state volume, starts the agent and prints its radio-in code.
It does not create, copy or accept a Kratos secret.

After the health-check package has been published, pass its full immutable digest to report a real
CUDA computation result with each heartbeat:

```powershell
.\workers\agent\install-windows.ps1 `
  -DisplayName "Home GPU 2" `
  -AgentHostname "HOME-GPU-02" `
  -HealthCheckImage "ghcr.io/danielbryars/kratos-gpu-health-check@sha256:<digest>"
```

This option mounts the Docker Engine socket only into the trusted agent. The agent launches the
health image without networking, with a read-only root filesystem, dropped capabilities, bounded
CPU, memory and process count, and access only to the selected GPU. The health container never
receives the Docker socket.

The equivalent manual command is:

```shell
docker run --detach --restart unless-stopped --gpus all \
  --name kratos-agent \
  --mount type=volume,source=kratos-agent-state,target=/var/lib/kratos-agent \
  ghcr.io/danielbryars/kratos-agent:edge
docker logs kratos-agent
```

The public image contains no Kratos credential. The device private key and issued worker credential
are written atomically to the mode-`0600` state file. The state volume must be retained across
container replacement.

Automated provisioners may instead mount a one-time enrolment credential. No enrolment credential
is accepted on the command line because command arguments can be exposed by process inspection.

Start the long-running agent with persistent state and a read-only enrolment secret mount:

```shell
docker run --detach --restart unless-stopped --gpus all \
  --name kratos-agent \
  --mount type=volume,source=kratos-agent-state,target=/var/lib/kratos-agent \
  --mount type=bind,source=/secure/path/enrolment,target=/run/secrets/kratos-enrolment,readonly \
  ghcr.io/danielbryars/kratos-agent:edge run \
  --display-name "Home GPU 1" \
  --state-volume kratos-agent-state \
  --enrolment-credential-file /run/secrets/kratos-enrolment
```

After enrolment, the bootstrap credential is consumed and the mounted file may be removed. Restarts
load the worker credential from the persistent state volume. Heartbeat sequence numbers are advanced
only after the control plane accepts them, so a network retry cannot replace a newer observation.

Build and run the Linux GPU container from the repository root:

```shell
docker build --file workers/agent/Dockerfile --tag kratos-agent .
docker run --rm --gpus all kratos-agent inspect
```

The trusted agent can invoke the controlled health image as a constrained sibling through the Docker
Engine socket. Use an immutable registry digest or local image ID:

```shell
docker run --rm \
  --group-add "$(stat -c '%g' /var/run/docker.sock)" \
  --mount type=bind,source=/var/run/docker.sock,target=/var/run/docker.sock \
  kratos-agent health-check --image sha256:<64-hex-character-image-id>
```

Docker Desktop currently presents the socket as group `0`; a PowerShell launch can therefore use
`--group-add 0`. Native Linux installation must use the actual socket group rather than assuming a
fixed identifier.

## Manual Windows update

The identity volume is independent of the replaceable agent container. Confirm that the volume is
present, resolve the new image to a digest, then replace only the container:

```powershell
docker volume inspect kratos-agent-state
docker pull ghcr.io/danielbryars/kratos-agent:edge
$agentImage = docker image inspect ghcr.io/danielbryars/kratos-agent:edge --format "{{index .RepoDigests 0}}"
docker stop kratos-agent
docker rm kratos-agent
.\workers\agent\install-windows.ps1 -Image $agentImage -DisplayName "Home GPU 2" -AgentHostname "HOME-GPU-02"
```

The installer reuses `kratos-agent-state`; it SHALL NOT delete or recreate that volume. A successful
replacement resumes heartbeats under the existing worker identity and does not require operator
approval. If startup fails, rerun the installer with the previous immutable digest and the same
parameters.

The executor disables networking, uses a read-only root filesystem, drops Linux capabilities,
applies CPU, memory and process limits, assigns only the requested GPU and always removes the health
container after collecting its bounded JSON result.

GitHub Actions publishes `edge`, `sha-<commit>` and `agent-v*` release tags with provenance and an
SBOM. After the first publication, the repository owner must set the GHCR package visibility to
public once; image contents and subsequent publishing remain automated.

## Scheduled work

An enrolled agent polls for at most one assignment in each signed heartbeat. Assigned images MUST be
immutable digest references. The agent runs them as named sibling containers with one selected GPU,
no network, a read-only root filesystem, dropped Linux capabilities, `no-new-privileges`, and bounded
CPU, memory, process count and runtime.

The stopped container remains present until the control plane acknowledges its bounded stdout,
stderr and exit status. This lets a restarted agent report the same attempt instead of knowingly
starting it twice. The agent removes the container after acknowledgement. The Docker socket therefore
remains an explicit host-administrative trust boundary.

The agent heartbeats while it pulls a job's image. A cold multi-gigabyte pull takes minutes on
a home link, and without this the worker sends nothing for all of it: it reads `stale` and then
`offline` exactly as a job begins, and cannot observe a cancellation until the pull ends. If the
control plane stops holding the attempt during the pull, the agent abandons it before any
container is created.

The agent records each attempt in its state volume before creating the container, so a missing
container is reported as a failure rather than run again. It **kills** a container at the earlier of
its runtime bound and lease deadline, without needing the control plane and without a grace period,
so a workload that ignores `SIGTERM` cannot run past its authority. Nothing is signalled: a job
stopped by Kratos produces no interruption record. Losing the control plane, or a temporary Docker
or registry error, is logged as `{"status": "retrying"}` and does not end the agent: heartbeats
continue and the retained result is delivered once the link returns. Heartbeats also continue while
a job runs, and the agent kills the container if the control plane stops returning that attempt.
See the
[worker protocol](../../docs/protocol/worker-v1.md#execution-authority-and-network-loss).

## Output manifests

`kratos_agent.outputs.build_manifest` prepares the manifest that ADR-014 and the protocol 1.1
durable-output extension require. Given a finished attempt's output directory and the job's
declared output requirements, it records each file's byte length, SHA-256 and base64 CRC32C. It
walks the tree through directory descriptors opened without following symbolic links, so swapping
an inspected directory or file for a link cannot lead it outside the tree. It refuses the whole
tree if it finds a symbolic link, a hard-link alias, a device, socket or named pipe, a path the
control plane would reject, an undeclared file, a file above its declared size, or a missing
mandatory output.

Each visited directory and file is made read-only where this agent is permitted to, which it often
is not, because a job image may write its outputs as any user. Sealing is therefore defence in
depth rather than the guarantee. A file whose device, inode, size, link count
or modification or change time differs after hashing is rejected. The builder returns that identity
with every manifest entry, and the uploader re-checks it on the descriptor it sends, so the uploaded
bytes cannot differ from the manifest.

## Durable outputs

A job may declare output requirements. The agent advertises protocol `1.1` **only when it could
actually deliver them**: it needs `--state-volume`, a reachable Docker Engine 26 or later for
volume subpath support, that volume to exist, and the pinned cleanup image already local. The
cleanup image is fetched during that check, while the worker is still free to decline the
capability, because cleanup runs *after* a job has succeeded and must not depend on a registry
being reachable then. When any of those is missing the agent says so on startup and advertises
`1.0`, so the scheduler never assigns work the worker would have to reject.

Before the container starts, the agent mounts the subpath `attempts/<attempt-id>/outputs` of its
own state volume at `/kratos/outputs`, writable. Only that subdirectory is exposed: the volume root
holds the worker credential and the device private key, and the job never sees it. This needs
Docker Engine 26 or later for volume subpath support, and the agent must know its volume name,
which `--state-volume` supplies and the installer passes. A job that declares outputs fails with an
actionable message if that name is missing, rather than falling back to a weaker mount.

Only a **successful** workload waits on delivery. A failed or timed-out result is reported
immediately, because the control plane gates mandatory outputs on success alone; storage being
unavailable must never hide a workload failure or leave the worker occupied. If the control plane
withdraws the attempt while the container runs, the agent abandons delivery, reports the terminal
result and cleans up.

After a successful container stops, the agent builds the manifest described above, submits it, and
for each artefact requests an upload session, sends the bytes, and reports the generation Cloud
Storage returned. Only then does it report the job result, because the control plane refuses a
successful result while a mandatory artefact is unverified. A job whose outputs cannot be collected
is reported as a failure rather than as a success that would be refused.

Transfers resume: the agent asks Cloud Storage what it already holds and continues from the
acknowledged offset, so an interrupted upload does not restart. Each file is reopened and
re-checked against the identity recorded during hashing, so a file altered between hashing and
transfer is never sent under the manifest's checksums. A session that is refused, or that stops
making progress, is abandoned through `abandon-upload`, which carries only a SHA-256 fingerprint of
the URI, and a replacement is requested. If the manifest already carries a `storage_generation` the
bytes arrived and only the acknowledgement was lost, so the agent completes rather than uploading
again. Session URIs are credentials and never reach a log. Retained outputs are discarded only once the control plane has
acknowledged the result.
