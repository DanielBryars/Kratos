FROM node:24-bookworm-slim AS web-builder
WORKDIR /src
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps/web/package.json apps/web/package.json
RUN pnpm install --frozen-lockfile
COPY apps/web apps/web
RUN pnpm web:build

FROM rust:1.90-bookworm AS api-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY services/control-plane/Cargo.toml services/control-plane/Cargo.toml
COPY services/control-plane/src services/control-plane/src
RUN cargo build --release --locked --package kratos-control-plane

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --uid 10001 kratos
COPY --from=api-builder /src/target/release/kratos-control-plane /usr/local/bin/kratos-control-plane
COPY --from=web-builder /src/apps/web/dist /app/web
USER kratos
ENV PORT=8080
ENV KRATOS_WEB_ROOT=/app/web
EXPOSE 8080
ENTRYPOINT ["kratos-control-plane"]
