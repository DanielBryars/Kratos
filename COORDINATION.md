# Agent coordination has moved

The live file is **outside this repository**, at:

```text
F:\git\kratos-coordination\COORDINATION.md
```

It is deliberately not version controlled with the code. Two agents write to it constantly, and
through a branch, a pull request and a merge neither could see the other's edit until it was too
late to act on — a question blocking one of them sat unseen on an unmerged branch, and an earlier
edit left a conflicted merge in a shared worktree. Outside the repository an edit is visible
immediately.

Read it before claiming work, and keep this pointer up to date if the location changes.

Its conventions, in short, because there is no merge to catch a clash:

- **Read immediately before you write, and keep each edit small.** Never rewrite the whole file.
- **Own your own lines.** Add, update and remove only your own claim rows; each direction's
  handover section is append-only, so reply by appending to yours rather than editing the other's.
- **Keep it current.** Remove a claim when its pull request merges; a stale claim is worse than
  none, because the other agent routes around work that has already landed.
- **Anything time-sensitive also goes on the pull request it concerns.** That file is the shared
  context; a PR comment is what the other agent sees while working that branch.

Ownership of R0.2 work is defined in [docs/r0.2-workstreams.md](docs/r0.2-workstreams.md), which
stays in the repository because it is a release document rather than a live scratchpad.
