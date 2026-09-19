FROM mcr.microsoft.com/devcontainers/rust:1-1-bookworm

RUN cargo install just --locked
RUN curl -LsSf https://astral.sh/uv/0.11.19/install.sh | env UV_INSTALL_DIR=/usr/local/bin sh
