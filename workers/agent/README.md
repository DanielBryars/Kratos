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

No enrolment credential is accepted on the command line because command arguments can be exposed
by process inspection. Enrolment and credential storage will be added with the authenticated worker
API.

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
