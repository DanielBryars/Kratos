# Manual takeover checklist

**Prepared:** 2026-09-28

This is the current operating handover for manual work.

## Known-good live state

- [x] THESHED2 is online and idle on the signed protocol 1.3 agent image `ghcr.io/danielbryars/kratos-agent@sha256:fb04c8bc1992117d3bd3a1e0403b35c2b73bf895de0e853466aec3326bbe2f30`, with its existing identity and state volume preserved. The older agent containers remain available for rollback. **This image predates the heartbeat replay fix in PR #109 (`7847ded`), so the running worker still carries that defect** — see the next-work item below.
- [x] The 2,000-step SmolVLA acceptance run completed and its storage-verified 1.23 GiB model archive is preserved.
- [x] Observation recovery accepted 105 protocol 1.2 records with no `dropped.spool_write_failed`.
- [x] Grafana and MLflow are behind IAP. The observation outbox projector is live on Cloud Run revision `kratos-00098-c76`, including restart-safe replay and IAP service identity.
- [x] Projects and invitations are deployed.
- [x] Dataset migrations 027/028, catalogue, folder upload, private Leroboscope preview, curation, and immutable view publication are deployed.
- [x] A real seven-file LeRobot dataset reached Ready, loaded 303 frames and two camera streams for episode 0, and published a one-episode curated view. Full evidence is in [dataset-catalogue-handover.md](dataset-catalogue-handover.md).
- [x] Exact dataset-version and curated-view selection, scheduling validation, digest-keyed read-only agent staging, and training lineage are deployed end to end.
- [x] Two fresh CUDA acceptance runs completed on 2026-09-28. Job `576c7e6f-58bc-44d4-8493-1983eb300633` produced MLflow run `45e80d588d0640fabb801bfa0914a984`; curated dataset job `0987191d-e07b-47a1-bbff-30487d59ba6a` produced MLflow run `2f52db973c6047be95336f11fb00dfdd`. Both jobs succeeded and both `model.pt` outputs are storage verified. The curated run records the immutable dataset/version/view identities, selection hashes, episode 0, and train/validation loss.

## Next work, in order

- [ ] **Roll THESHED2 onto an agent image built from `7847ded` or later.** The heartbeat replay wedge is fixed in `main` but not in the running worker, which uses a pinned image digest: a crash between the control plane accepting a heartbeat and the agent persisting the sequence will still stop that worker heartbeating until someone intervenes by hand. Fixed in code is not fixed on the host. Keep the current containers for rollback and preserve the identity and state volume, as the guardrails below require.
- [ ] Exercise project invitation claiming with two real Google identities.
- [ ] Run the witnessed network-loss exercise in `docs/acceptance/r0.2/network-loss-exercise-runbook.md` when someone can disconnect and reconnect the selected worker.
- [ ] Decide whether to retain or purge incomplete upload rows after reference tracking and a retention policy exist.
- [ ] Finish review of the disabled, cost-free capacity-provider boundary in PR #105. A real SkyPilot provider, pre-attempt provisioning, Terraform, and any cloud spend remain later work requiring a separate design and explicit spend decision. Kratos remains authoritative for fairness, budgets, job state, and result identity.
- [ ] Choose the first useful model objective and authorised dataset. The deployed CUDA workload proves reproducible dataset selection, execution, lineage and storage; it is still a bounded smoke model.
- [ ] Bind preview reads to caller identity, deferred by Daniel on 2026-09-27 and detailed under "Marked for revisit" in [dataset-catalogue-handover.md](dataset-catalogue-handover.md). A preview read authorises on the short-lived capability alone, so it proves the caller holds a valid session but not who they are. The viewer frame does already hold an identity token and uses it for curation; the work is carrying an identity alongside the capability on each read. The trigger is any dataset holding material that is not ours to lose. The code carries the same note at `services/control-plane/src/datasets.rs`.

## Guardrails

- Preserve existing workload evidence, old agent containers, agent state, and accepted dataset artefacts.
- Keep THESHED2 idle unless intentionally running an accepted workload.
- Do not expose object keys, resumable-session URLs, preview tokens, identity tokens, OAuth secrets, or signed URLs in issues or screenshots.
- Keep uploaded datasets private. Use the short-lived version-scoped preview capability and per-file signed reads.
- Do not delete the earlier incomplete dataset rows until retention and reference tracking are in place.

## Repository state

- `main`: MLflow projector and all dataset execution work through PR #104 merged at `50cc735`.
- Live deployment: run `36439537379`; Cloud Run revision `kratos-00098-c76` has 100% traffic.
- `F:\git\Kratos-capacity`: Claude's isolated worktree for disabled capacity-provider PR #105.
- `F:\git\Kratos-release-handoff`: Codex's documentation-only handoff worktree.
- `F:\git\kratos-coordination\COORDINATION.md`: live Codex/Claude coordination board.
