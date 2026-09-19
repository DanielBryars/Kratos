set shell := ["bash", "-cu"]

bootstrap:
    pnpm install --frozen-lockfile
    cargo fetch
    cd workers/agent && uv sync --locked

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

test:
    cargo test --workspace
    cd workers/agent && uv run pytest

build:
    cargo build --workspace
    pnpm web:build
