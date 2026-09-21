FROM node:24-bookworm-slim AS web-builder
WORKDIR /src
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps/web/package.json apps/web/package.json
COPY apps/leroboscope/package.json apps/leroboscope/package.json
RUN pnpm install --frozen-lockfile
COPY apps/web apps/web
COPY apps/leroboscope apps/leroboscope
RUN pnpm web:build && pnpm leroboscope:build && mkdir -p apps/web/dist/leroboscope && cp -R apps/leroboscope/dist/. apps/web/dist/leroboscope/

FROM rust:1.90-bookworm AS api-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY services/control-plane/Cargo.toml services/control-plane/Cargo.toml
COPY services/control-plane/src services/control-plane/src
COPY services/control-plane/migrations services/control-plane/migrations
RUN cargo build --release --locked --package kratos-control-plane

FROM gcr.io/cloud-sql-connectors/cloud-sql-proxy@sha256:88501f0a695a586988add1b8a206fdf3f29f9a1a3deeb9b45ef2b1481ea6be83 AS cloud-sql-proxy

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --uid 10001 kratos
COPY --from=api-builder /src/target/release/kratos-control-plane /usr/local/bin/kratos-control-plane
COPY --from=api-builder /src/target/release/kratos-migrate /usr/local/bin/kratos-migrate
COPY --from=cloud-sql-proxy /cloud-sql-proxy /usr/local/bin/cloud-sql-proxy
COPY --chmod=0555 services/control-plane/scripts/migrate-with-proxy.sh /usr/local/bin/kratos-migrate-with-proxy
COPY --from=web-builder /src/apps/web/dist /app/web
USER kratos
ENV PORT=8080
ENV KRATOS_WEB_ROOT=/app/web
EXPOSE 8080
ENTRYPOINT ["kratos-control-plane"]
