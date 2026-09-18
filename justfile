set shell := ["bash", "-cu"]

bootstrap:
    pnpm install --frozen-lockfile
    cargo fetch

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

test:
    cargo test --workspace

build:
    cargo build --workspace
    pnpm web:build
