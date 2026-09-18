# ADR-004 — Rust control plane with Axum

Status: Accepted

## Context

Kratos requires a cloud control plane for worker registration, scheduling, permissions, leases, accounting and audit. GPU training workloads remain Python applications running in isolated worker containers.

## Decision

The control-plane API and scheduler SHALL use Rust. The HTTP API SHALL use Axum and expose a versioned OpenAPI contract. Training workloads SHALL use Python and PyTorch independently of the control-plane implementation.

## Rationale

Rust provides strong static guarantees for state transitions, identifiers, resource quantities and accounting values. Its enums and exhaustive matching suit the worker and job state machines. A native Linux binary also produces a compact runtime container.

Python remains the appropriate language for model training and ML-library integration. Keeping the network contract language-neutral prevents either side from depending on the other's implementation language.

## Alternatives

| Option | Assessment |
|---|---|
| Rust with Axum | Selected for its type system, explicit state modelling, native deployment and value as a project learning goal. |
| Kotlin with Ktor | Strong, modern alternative with a mature JVM ecosystem and faster initial development. |
| C# with ASP.NET Core | Capable but intentionally not selected for this project. |
| TypeScript backend | Shares a language with the UI but does not provide the desired degree of type safety at runtime and across untrusted inputs. |
| Python backend | Close to ML tooling but does not provide the desired control-plane type guarantees. |

## Consequences

- Domain types SHALL distinguish worker IDs, job IDs, credits, durations and resource quantities.
- State transitions SHOULD use closed enums and exhaustive matching.
- External input SHALL be deserialised and validated before entering the domain layer.
- Request handlers SHALL delegate domain work rather than owning scheduling or persistence logic.
- API and background scheduling MAY share Rust crates but SHALL remain independently runnable components.
- CI SHALL run formatting, compilation, Clippy and tests on the pinned toolchain.
- OpenAPI generation SHALL be checked in CI and used to produce the TypeScript browser client.
- Compiler-level custom lints MAY be introduced for valuable architecture rules, with their pinned-toolchain maintenance cost documented.

## Initial libraries

- Axum and Tokio for HTTP and async execution.
- Serde for serialisation.
- Utoipa with Axum and Swagger UI integration for OpenAPI 3.1.
- SQLx is the leading PostgreSQL candidate and remains subject to the persistence decision.

## Reconsider when

Measured delivery or operating constraints cannot be met without disproportionate complexity. Individual components MAY use another language through a separate decision without rewriting the entire control plane.

## References

- [Axum](https://github.com/tokio-rs/axum)
- [Utoipa](https://github.com/juhaku/utoipa)
- [Clippy](https://doc.rust-lang.org/clippy/)
