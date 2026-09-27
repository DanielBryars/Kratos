# Manual takeover checklist

**Prepared:** 2026-09-27

This is the current operating handover for manual work.

## Known-good live state

- [x] THESHED2 is online and idle on the signed PR #81 agent image `ghcr.io/danielbryars/kratos-agent@sha256:b0b979d9e802263483e28b96389c4edf859023fcee271354e2bc72251aa8b39e`.
- [x] The 2,000-step SmolVLA acceptance run completed and its storage-verified 1.23 GiB model archive is preserved.
- [x] Observation recovery accepted 105 protocol 1.2 records with no `dropped.spool_write_failed`.
- [x] Grafana and MLflow are behind IAP; the two small CUDA sample runs are visible in MLflow.
- [x] Projects and invitations are deployed.
- [x] Dataset migrations 027/028, catalogue, folder upload, private Leroboscope preview, curation, and immutable view publication are deployed.
- [x] A real seven-file LeRobot dataset reached Ready, loaded 303 frames and two camera streams for episode 0, and published a one-episode curated view. Full evidence is in [dataset-catalogue-handover.md](dataset-catalogue-handover.md).

## Next work, in order

- [ ] Add exact dataset-version and optional curated-view selection to job specifications and the scheduling screen.
- [ ] Validate the selected version/view at scheduling time and expose a worker-input protocol only for Ready immutable inputs.
- [ ] Stage dataset inputs read-only on the agent with digest-keyed caching and bounded cleanup.
- [ ] Add dataset/version/view lineage to MLflow, then run one curated version through training and verify the complete lineage.
- [ ] Exercise project invitation claiming with two real Google identities.
- [ ] Run the witnessed network-loss exercise in `docs/acceptance/r0.2/network-loss-exercise-runbook.md` when someone can disconnect and reconnect the selected worker.
- [ ] Decide whether to retain or purge incomplete upload rows after reference tracking and a retention policy exist.
- [ ] Implement queue and SkyPilot capacity only after the design is approved. Kratos remains authoritative for fairness, budgets, job state, and result identity; starting cloud capacity requires an explicit spend decision.

## Guardrails

- Preserve existing workload evidence, old agent containers, agent state, and accepted dataset artefacts.
- Keep THESHED2 idle unless intentionally running an accepted workload.
- Do not expose object keys, resumable-session URLs, preview tokens, identity tokens, OAuth secrets, or signed URLs in issues or screenshots.
- Keep uploaded datasets private. Use the short-lived version-scoped preview capability and per-file signed reads.
- Do not delete the earlier incomplete dataset rows until retention and reference tracking are in place.

## Repository state

- `main`: dataset catalogue and live fixes through PR #94 merged.
- Live deployment: run `36354202555` for PR #94.
- `F:\git\Kratos-datasets`: reusable isolated worktree; current documentation branch is `docs/dataset-live-handover`.
- `F:\git\kratos-coordination\COORDINATION.md`: live Codex/Claude coordination board.
