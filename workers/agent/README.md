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
