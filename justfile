set shell := ["bash", "-cu"]

bootstrap:
    pnpm install --frozen-lockfile
    cargo fetch
    cd workers/agent && uv sync --locked
    cd workers/gpu-health-check && uv sync --locked --python 3.12
    cd workers/training-example && uv sync --locked --python 3.12

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

build:
    cargo build --workspace
    pnpm web:build
