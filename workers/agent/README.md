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

The agent records each attempt in its state volume before creating the container, so a missing
container is reported as a failure rather than run again. It stops a container at the earlier of its
runtime bound and lease deadline without needing the control plane. Losing the control plane, or a
temporary Docker or registry error, is logged as `{"status": "retrying"}` and does not end the
agent: heartbeats continue and the retained result is delivered once the link returns. See the
[worker protocol](../../docs/protocol/worker-v1.md#execution-authority-and-network-loss).
