# Scoring agent-brief quality at claim time

**Rejected because:** A refuse-to-claim on brief quality parks the issue
until a human rewrites the brief — and the whole premise of AFK is that
the human is away. Per ADR-0012, a control inside the run pipeline may
not create a state only an absent human can clear. This is a *stronger*
violation than the merge gates ADR-0011 removed: a parked PR at least
holds finished work, whereas a refused claim holds nothing at all.

It is also largely redundant. The quality judgement already happens
upstream, in the right place: the triage agent's decision tree routes
issues with missing or vague acceptance criteria to `needs-info` *before*
a brief is written, and defaults to `needs-info` when unsure. Triage is
unconstrained by ADR-0012 because deciding whether a human is needed is
its purpose, not a failure mode.

The narrow residue that *is* worth closing — the triage agent does not
verify the brief it just authored — is a `TRIAGE_PROMPT` change, not a
new gate, and is tracked separately.

Note also that hand-written briefs are deliberately left unchecked. An
operator applying `ready-for-agent` by hand has made an explicit
judgement; gating it would second-guess a deliberate human decision and
summon that human back to argue with it.

**Originating issue:** #166 — Agent-brief gate checks presence, not quality

**See also:**
- [ADR-0012](../docs/adr/0012-gates-are-judged-by-whether-they-summon-an-absent-human.md) — the general rule
- [ADR-0011](../docs/adr/0011-mechanical-only-merge-gating-merger-advisory.md) — the merge-gating decision this generalises
