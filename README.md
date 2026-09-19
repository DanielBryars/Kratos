# Kratos
AI model training platform

## Requirements

See the [requirements overview](requirements/README.md) for the project scope and links to the individual requirements chapters.

See the [release plan](requirements/09-releases-and-acceptance.md) for milestones and the [technology decision register](docs/architecture/README.md) for accepted choices and options under discussion.

The immediate timebox is captured in the [weekend MVP plan](docs/weekend-plan.md).

## Local development

Install Docker Desktop, VS Code and the Dev Containers extension, then open this repository and choose **Dev Containers: Reopen in Container**. When setup completes, select the **Kratos** debug configuration and press F5. This starts the Rust API under LLDB, starts the React development server and opens the web interface.

The same checks used by CI can be run in the development container:

```shell
just check
```

The initial endpoints are:

- Web interface: <http://localhost:5173>
- API health: <http://localhost:8080/healthz>
- Swagger UI: <http://localhost:8080/swagger-ui/>
