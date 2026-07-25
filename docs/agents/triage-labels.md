# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual label strings used in this repo's issue tracker.

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label string from this table.

Edit the right-hand column to match whatever vocabulary you actually use.

## Correction attribution labels

A **Correction** is an issue filed to fix something a prior run already
shipped (see `CONTEXT.md`). These three labels record which part of the
system is at fault. They are orthogonal to the triage roles above — a
Correction still moves through `needs-triage` → `ready-for-agent` like
any other issue.

| Label | Means | Response |
| --- | --- | --- |
| `agent-fault` | The brief was clear and the harness behaved; the shipped work still did not do the job. | Chain config, model choice, possibly a gate. |
| `harness-fault` | A Bellows prompt, gate, or policy misbehaved, and would misbehave identically under any engine. | Fix Bellows. |
| `brief-fault` | The engine faithfully built what the brief asked for; the asking was wrong. | Triage quality. |

Exactly one applies. Carrying one is what makes an issue a Correction —
there is no separate `correction` label.

### Why the split matters

A raw count of corrections is misleading. #154 and #161 are both defects
in Bellows' own prompts and policy: #154's mega-commit check fires on
essentially every run because `commit_all` makes the red→green commit
shape it demands architecturally impossible to produce. Counting those
against the runs that faithfully executed them would make the engines
look unreliable when the defect is ours — and would send someone tuning
`cli_chain` to fix a prompt bug.

**Only `agent-fault` is evidence about ADR-0011's trade** of subjective
merge gates for occasional bad merges. `harness-fault` measures Bellows'
own defect rate; `brief-fault` measures triage quality.

### Naming what it corrects

- **`agent-fault` and `brief-fault`** carry a `Corrects: #<PR>` line in
  the issue body. It must be the **PR**, not the issue: one issue can
  produce several runs (a failed run, a re-label, a retry) and each gets
  its own PR, so the PR is what identifies a *run*.
- **`harness-fault`** carries `Observed in: #<PR>` instead, and it is
  optional. The fault is in Bellows, not in any particular run, so there
  is often no run that is honestly "at fault" — the pointer is evidence,
  not attribution.

### Reading the data

Deliberately no tooling. At current volume a label query answers the
whole question:

```bash
gh issue list --state all --label agent-fault --json number,title,body
```

A `bellows postmortem` subcommand would spend most of its life printing
zeroes. Revisit when eyeballing the query stops working.

The `agent-fault` set is also the answer key #163 needs: each one points
at a PR, and a PR rehydrates into an eval fixture (diff, brief, verdict)
straight from GitHub.
