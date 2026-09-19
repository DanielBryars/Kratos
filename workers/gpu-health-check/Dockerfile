FROM ghcr.io/astral-sh/uv:0.11.19@sha256:b46b03ddfcfbf8f547af7e9eaefdf8a39c8cebcba7c98858d3162bd28cf536f6 AS uv

FROM nvidia/cuda:12.8.1-runtime-ubuntu24.04@sha256:ebef3c171eeef0298e4eb2e4be843105edf3b8b0ac45e0b43acee358e8046867

COPY --from=uv /uv /uvx /bin/

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
        ca-certificates \
        python3.12 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system kratos \
    && useradd --system --gid kratos --home-dir /var/lib/kratos-health --create-home kratos

WORKDIR /app
COPY workers/gpu-health-check/pyproject.toml workers/gpu-health-check/uv.lock ./
RUN uv sync --locked --no-dev
COPY workers/gpu-health-check/health_check.py ./

ENV HOME=/var/lib/kratos-health
USER kratos
ENTRYPOINT ["/app/.venv/bin/python", "/app/health_check.py"]
