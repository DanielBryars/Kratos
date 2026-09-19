FROM ghcr.io/astral-sh/uv:0.11.19@sha256:b46b03ddfcfbf8f547af7e9eaefdf8a39c8cebcba7c98858d3162bd28cf536f6 AS uv

FROM nvidia/cuda:12.8.1-base-ubuntu24.04@sha256:133c78a0575303be34164d0b90137a042172bdf60696af01a3c424ab402d86e2

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
