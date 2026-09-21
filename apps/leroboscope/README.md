# Kratos Leroboscope

This app embeds the Leroboscope LeRobot episode viewer in Kratos. It can still open a public
Hugging Face dataset directly:

```text
/leroboscope/?dataset=namespace/name&revision=<exact-commit-sha>&episode=0
```

Kratos always supplies an exact revision for a registered Hugging Face dataset. A floating branch
such as `main` is useful for manual exploration but is not a training input identity.

## Curation bridge

The Kratos console embeds the viewer from the same origin. When the viewer posts
`{ type: "kratos:review-ready" }`, the parent sends the current in-memory identity token and review
state:

```ts
iframe.contentWindow?.postMessage({
  type: "kratos:review-context",
  versionId,
  idToken,
  decisions: { "0": "included", "1": "needs_review" },
}, window.location.origin);
```

The token is not placed in the URL, persisted by the viewer, or sent to another origin. Curation
buttons call the project-authorised Kratos API. The allowed decisions are `included`, `excluded` and
`needs_review`.

For an uploaded dataset, the context also carries its validated `info.json` value and a short-lived
review-session file base URL:

```ts
source: {
  kind: "uploaded",
  label: "Warehouse picks",
  revision: manifestSha256,
  info,
  fileBaseUrl: `/api/v1/dataset-review-sessions/${reviewToken}/files`,
}
```

The file route must authorise only that dataset version, support byte ranges for Parquet and video,
and expire quickly. It must not expose a bucket credential.

## Development

From the repository root:

```text
pnpm leroboscope:typecheck
pnpm leroboscope:build
pnpm leroboscope:dev
```

The production control-plane image builds the app into `/leroboscope/` under the main console's
static root.
