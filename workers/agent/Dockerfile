FROM ghcr.io/astral-sh/uv:0.11.19 AS uv

FROM nvidia/cuda:12.8.1-base-ubuntu24.04

COPY --from=uv /uv /uvx /bin/

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
        ca-certificates \
        python3.12 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system kratos \
    && useradd --system --gid kratos --home-dir /var/lib/kratos-agent --create-home kratos

WORKDIR /app
COPY workers/agent/pyproject.toml workers/agent/uv.lock workers/agent/README.md ./
COPY workers/agent/src ./src
RUN uv sync --locked --no-dev

ENV PATH="/app/.venv/bin:${PATH}"
USER kratos

ENTRYPOINT ["kratos-agent"]
CMD ["inspect"]
