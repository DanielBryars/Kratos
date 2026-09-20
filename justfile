set shell := ["bash", "-cu"]

bootstrap:
    pnpm install --frozen-lockfile
    cargo fetch
    cd workers/agent && uv sync --locked
    cd workers/gpu-health-check && uv sync --locked --python 3.12
    cd workers/training-example && uv sync --locked --python 3.12
    cd workers/soak-workload && uv sync --locked --python 3.12

api:
    cargo run --package kratos-control-plane

web:
    pnpm web:dev

format:
    cargo fmt --all

check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace
    pnpm web:test
    pnpm web:typecheck
    pnpm web:build
    cd workers/agent && uv run ruff format --check .
    cd workers/agent && uv run ruff check .
    cd workers/agent && uv run mypy src tests
    cd workers/agent && uv run pytest
    cd workers/gpu-health-check && uv run ruff format --check .
    cd workers/gpu-health-check && uv run ruff check .
    cd workers/gpu-health-check && uv run mypy health_check.py
    cd workers/training-example && uv run ruff format --check .
    cd workers/training-example && uv run ruff check .
    cd workers/training-example && uv run mypy train.py tests
    cd workers/training-example && uv run pytest
    cd workers/soak-workload && uv run ruff format --check .
    cd workers/soak-workload && uv run ruff check .
    cd workers/soak-workload && uv run mypy soak.py tests
    cd workers/soak-workload && uv run pytest

test:
    cargo test --workspace
    cd workers/agent && uv run pytest

gpu-health-image:
    docker build --file workers/gpu-health-check/Dockerfile --tag kratos-gpu-health-check .

gpu-health: gpu-health-image
    docker run --rm --gpus device=0 kratos-gpu-health-check

training-image:
    docker build --file workers/training-example/Dockerfile --tag kratos-training-example .

training: training-image
    docker run --rm --gpus device=0 --network none --read-only --tmpfs /tmp:rw,noexec,nosuid,size=1g kratos-training-example

soak-image:
    docker build --file workers/soak-workload/Dockerfile --tag kratos-soak-workload .

soak: soak-image
    docker run --rm --gpus device=0 --network none --read-only --cap-drop ALL --security-opt no-new-privileges --memory 8g --cpus 4 --pids-limit 512 --tmpfs /tmp:rw,noexec,nosuid,size=1g -e KRATOS_SOAK_SECONDS=30 -e KRATOS_JOB_ID=22222222-2222-4222-8222-222222222222 -e KRATOS_ATTEMPT_ID=11111111-1111-4111-8111-111111111111 kratos-soak-workload

build:
    cargo build --workspace
    pnpm web:build
