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

The executor disables networking, uses a read-only root filesystem, drops Linux capabilities,
applies CPU, memory and process limits, assigns only the requested GPU and always removes the health
container after collecting its bounded JSON result.

GitHub Actions publishes `edge`, `sha-<commit>` and `agent-v*` release tags with provenance and an
SBOM. After the first publication, the repository owner must set the GHCR package visibility to
public once; image contents and subsequent publishing remain automated.
