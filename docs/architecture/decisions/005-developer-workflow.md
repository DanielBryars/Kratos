# ADR-005 — VS Code development workflow

Status: Accepted

## Decision

VS Code SHALL be the documented project editor. A Linux development container SHALL provide the canonical Rust, Node and Terraform toolchains. Docker Compose SHALL provide local dependencies. Repository tasks SHALL remain usable without the editor.

The default debug action SHALL start the Rust API under a debugger, start the React development server and open the application. Routine commands SHALL be exposed through a `justfile` and reused by CI where practical.

## Rationale

VS Code supports Rust through rust-analyzer and CodeLLDB while also supporting React, TypeScript, containers and Terraform in one workspace. A Linux dev container matches the cloud runtime and avoids requiring each toolchain on the Windows host.

## Consequences

- `.devcontainer`, `.vscode` and task definitions are version controlled.
- PostgreSQL and later local dependencies run separately from the application processes.
- Developers can debug application code while dependencies remain containerised.
- Build caches use named volumes to avoid placing all compilation output on the Windows bind mount.
- Real GPU and LAN validation remain host integration tests and SHALL NOT be simulated as accepted results.
- RustRover MAY be used personally, but checked-in workflows SHALL NOT require it.

## Alternatives

| Option | Assessment |
|---|---|
| VS Code and a dev container | Selected for the mixed Rust, React, Terraform and container workspace. |
| RustRover | Strong Rust IDE and acceptable personal alternative, but not the shared workflow. |
| Visual Studio | Does not provide an Aspire-equivalent Rust workflow for this project. |

## References

- [Rust in VS Code](https://code.visualstudio.com/docs/languages/rust)
- [Development Containers](https://containers.dev/)
