# Manual takeover checklist

**Prepared:** 2026-09-21

This is the current operating handover while automated work is paused.

## Known-good live state

- THESHED2 is online and idle on the signed PR #81 agent image
  `ghcr.io/danielbryars/kratos-agent@sha256:b0b979d9e802263483e28b96389c4edf859023fcee271354e2bc72251aa8b39e`.
- The 2,000-step SmolVLA acceptance run is complete and its storage-verified 1.23 GiB model archive
  must be preserved.
- Observation recovery acceptance is complete with 105 accepted protocol 1.2 records and no
  `dropped.spool_write_failed`.
- Grafana and MLflow are healthy behind IAP. Two small CUDA sample runs are visible in MLflow.
- Projects and invitations merged in PR #87 after the complete CI matrix passed.
- Queue and SkyPilot capacity are design only. No SkyPilot infrastructure has been created and no
  cloud capacity should be started without an explicit spend decision.

## Open work

- [ ] Review draft PR [#88](https://github.com/DanielBryars/Kratos/pull/88). It preserves the
  Leroboscope integration, ADR-018, migration 027, and the project-scoped dataset catalogue APIs.
- [ ] Follow the detailed dataset checklist in
  `docs/dataset-catalogue-handover.md` on PR #88. Do not deploy migration 027 until its console,
  private-preview session, and real upload path pass end-to-end acceptance.
- [ ] Exercise project invitation claiming with two real Google identities. The automated tests use
  a fake Identity Platform verifier, so this is still the missing production acceptance case.
- [ ] Run the witnessed network-loss exercise in
  `docs/acceptance/r0.2/network-loss-exercise-runbook.md` when someone can physically disconnect
  and reconnect the selected worker.
- [ ] Choose the first useful real model and authorised dataset. The smoke, soak, and sample CUDA
  jobs prove the mechanism but do not complete this product decision.
- [ ] After dataset staging exists, run one exact-version dataset through training and confirm its
  dataset ID, immutable revision or manifest hash, curated view, job ID, attempt ID, metrics, and
  model artefact all meet in MLflow.

## Guardrails

- Preserve existing workload evidence, old agent containers, and agent state.
- Keep THESHED2 idle unless intentionally running an accepted workload.
- Do not expose GCS resumable-session URLs, object keys, identity tokens, or OAuth secrets in logs,
  screenshots, issues, or pull requests.
- Do not serve uploaded datasets with bucket-wide credentials. Use a short-lived version-scoped
  preview session and per-file signed reads.
- Keep Kratos as the authoritative queue. SkyPilot may supply bounded capacity later; it does not
  own fairness, budgets, job state, or result identity.

## Repository state

- `main`: PR #87 merged and clean.
- `feature/dataset-catalogue`: pushed and represented by draft PR #88.
- `F:\git\Kratos-datasets`: the isolated worktree for PR #88.
- `F:\git\kratos-coordination\COORDINATION.md`: the live Codex/Claude coordination board.
