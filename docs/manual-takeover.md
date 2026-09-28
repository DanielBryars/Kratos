# Manual takeover checklist

**Prepared:** 2026-09-28

This is the current operating handover for manual work.

## Known-good live state

- [x] THESHED2 is online and idle on the signed protocol 1.3 agent image `ghcr.io/danielbryars/kratos-agent@sha256:18bf47ca5e201de78537f7873e9b9bb906a6429c49c282ccfc945f8a3b048157`, built from the heartbeat replay fix in PR #109 (`7847ded`). Its existing worker identity and `kratos-agent-state` volume are preserved. The immediately previous container is retained as `kratos-agent-pre109-20260928`, and all older agent containers remain available for rollback.
- [x] The 2,000-step SmolVLA acceptance run completed and its storage-verified 1.23 GiB model archive is preserved.
- [x] Observation recovery accepted 105 protocol 1.2 records with no `dropped.spool_write_failed`.
- [x] Grafana and MLflow are behind IAP. The observation outbox projector is live on Cloud Run revision `kratos-00102-6xq`, including restart-safe replay and IAP service identity.
- [x] Projects and invitations are deployed.
- [x] Dataset migrations 027/028, catalogue, folder upload, private Leroboscope preview, curation, and immutable view publication are deployed.
- [x] A real seven-file LeRobot dataset reached Ready, loaded 303 frames and two camera streams for episode 0, and published a one-episode curated view. Full evidence is in [dataset-catalogue-handover.md](dataset-catalogue-handover.md).
- [x] Exact dataset-version and curated-view selection, scheduling validation, digest-keyed read-only agent staging, and training lineage are deployed end to end.
- [x] Two fresh CUDA acceptance runs completed on 2026-09-28. Job `576c7e6f-58bc-44d4-8493-1983eb300633` produced MLflow run `45e80d588d0640fabb801bfa0914a984`; curated dataset job `0987191d-e07b-47a1-bbff-30487d59ba6a` produced MLflow run `2f52db973c6047be95336f11fb00dfdd`. Both jobs succeeded and both `model.pt` outputs are storage verified. The curated run records the immutable dataset/version/view identities, selection hashes, episode 0, and train/validation loss.

## Next work, in order

- [ ] Exercise project invitation claiming with two real Google identities.
- [ ] Run the witnessed network-loss exercise in `docs/acceptance/r0.2/network-loss-exercise-runbook.md` when someone can disconnect and reconnect the selected worker.
- [ ] Decide whether to retain or purge incomplete upload rows after reference tracking and a retention policy exist.
- [ ] Design and review a real SkyPilot capacity provider, pre-attempt provisioning and teardown before enabling the merged boundary from PR #105. The boundary is currently unwired and disabled, and creates no infrastructure or spend. Terraform changes and any cloud spend require a separate explicit decision. Kratos remains authoritative for fairness, budgets, job state, and result identity.
- [ ] Choose the first useful model objective and authorised dataset. The deployed CUDA workload proves reproducible dataset selection, execution, lineage and storage; it is still a bounded smoke model.
- [ ] Bind preview reads to caller identity, deferred by Daniel on 2026-09-27 and detailed under "Marked for revisit" in [dataset-catalogue-handover.md](dataset-catalogue-handover.md). A preview read authorises on the short-lived capability alone, so it proves the caller holds a valid session but not who they are. The viewer frame does already hold an identity token and uses it for curation; the work is carrying an identity alongside the capability on each read. The trigger is any dataset holding material that is not ours to lose. The code carries the same note at `services/control-plane/src/datasets.rs`.

## Guardrails

- Preserve existing workload evidence, old agent containers, agent state, and accepted dataset artefacts.
- Keep THESHED2 idle unless intentionally running an accepted workload.
- Do not expose object keys, resumable-session URLs, preview tokens, identity tokens, OAuth secrets, or signed URLs in issues or screenshots.
- Keep uploaded datasets private. Use the short-lived version-scoped preview capability and per-file signed reads.
- Do not delete the earlier incomplete dataset rows until retention and reference tracking are in place.

## Repository state

- `main`: dataset execution, the disabled capacity boundary and durable heartbeat recovery through PR #109 (`7847ded`) are merged.
- Live deployment: run `36472751143`; Cloud Run revision `kratos-00102-6xq` is Ready and has 100% traffic.
- Live agent publication: run `36472751107`; immutable digest `sha256:18bf47ca5e201de78537f7873e9b9bb906a6429c49c282ccfc945f8a3b048157` passed its security scan and is running on THESHED2.
- `F:\git\kratos-coordination\COORDINATION.md`: live Codex/Claude coordination board.
