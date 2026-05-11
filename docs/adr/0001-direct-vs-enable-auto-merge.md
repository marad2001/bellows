# Auto-merge workflow uses workflow_run trigger, not GitHub native auto-merge

Bellows-authored PRs (head branch starts with `agent/`) auto-merge to master once `ci.yml` passes. The auto-merge workflow triggers on `workflow_run` (i.e. fires after `ci.yml` completes with `conclusion == success`) and then calls the merge API directly. We chose this over GitHub's native auto-merge feature (`enable-pull-request-automerge`) because native auto-merge depends on branch protection rules to define what "merge-ready" means — without branch protection requiring the `ci` check, native auto-merge would merge bellows PRs immediately without waiting for CI. The `workflow_run` approach makes the workflow itself the CI gate, so the design works without any branch-protection configuration.

## Considered alternatives

- **Native GitHub auto-merge** via `peter-evans/enable-pull-request-automerge`. Rejected because it requires branch protection to be configured before it's safe, and bellows v1 doesn't assume the operator has done that one-time GitHub-settings setup yet (it's documented but operator-driven). Native auto-merge can be revisited in a future ADR once branch protection is the canonical posture.
- **Bellows binary calling the merge API itself** after opening Success PRs. Rejected because it puts merge authority inside the agent runtime — a worse governance posture as bellows grows, and harder to change merge policy without a binary release. Keeping merge in a workflow file means policy edits are YAML, not Rust.

## Consequences

- Branch protection becomes optional infrastructure rather than a prerequisite.
- The workflow is the canonical "what counts as merge-ready" rule for bellows PRs. Any future merge gating beyond CI must go in the workflow, not in branch protection.
- The repo-level "Allow auto-merge" setting is irrelevant — we never call GitHub's native auto-merge feature.
