# A cross-run memory store for lessons learned

**Rejected because:** every durable lesson already has a home that is
versioned, reviewed, and cannot rot silently. A parallel store would be
a second place to look, with worse curation than the ones we have.

| Kind of lesson | Where it already belongs |
| --- | --- |
| Repo-specific fact ("tests here need a running Postgres") | that repo's own `CLAUDE.md` — cloned every run, auto-read by the engine, agent-writable via PR |
| Repo-specific domain language | that repo's `CONTEXT.md` |
| Something Bellows does wrong | a `harness-fault` **Correction** → fix Bellows (ADR-0013) |
| Engine operational state (cooldowns) | `bellows-state.json` |
| Something about one issue | the agent brief, or a `brief-fault` **Correction** |
| The target repo's real CI clippy scope | derived fresh every run by parsing the workflow (ADR-0004) |

That last row is the argument in miniature. The most obvious
"remember this per repo" case was solved *better* by deriving it on
every run than by remembering it. **Derived beats remembered whenever
derivation is possible, because derived facts cannot go stale.**

Note also that `bellows-agent-notes.md` cannot serve as the vehicle:
ADR-0006 makes it explicitly ephemeral — captured, posted as a PR
comment, removed from the workspace, and committed-deleted before the
final push. It is a per-run channel by design.

**The cost we accepted:** the target-repo route needs a human to review
each lesson, so it is not the automatic accumulation the "memory"
context type usually describes. But the automatic version is precisely
a context-rot generator — a file that grows every run, loads every
turn, and degrades output while looking like it is helping. Given the
operator reviews PRs in the morning anyway (ADR-0011), one line added
to a repo's `CLAUDE.md` through the normal flow fits how the system is
actually used, and inherits curation, provenance and retention for
free.

**What this does not reject:** telling agents that the target repo's
own `CLAUDE.md` is where a durable finding belongs. Today the policy
image points them only at `bellows-agent-notes.md`, which is deleted
before the push, so an agent that learns something lasting has no
instruction pointing anywhere permanent. That gap is tracked
separately — it makes an existing capability usable rather than adding
a new store.

**Originating issue:** #165 — No cross-run memory: every run rediscovers
repo-specific hazards from scratch

**See also:**
- [ADR-0004](../docs/adr/0004-bellows-gate-mirrors-target-ci.md) — derive-don't-remember, applied to CI scope
- [ADR-0006](../docs/adr/0006-agent-notes-informational-vs-escalation.md) — the ephemeral agent-notes contract
- [ADR-0013](../docs/adr/0013-corrective-follow-up-not-reverts-measures-the-adr-0011-trade.md) — `harness-fault` Corrections as the route for Bellows' own defects
