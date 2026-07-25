# Agent notes — issue #164

**Bind-mount assumption verified.** The brief asked me to confirm
`/workspace` is a host directory bind-mounted into the container before
building on it. It is: `src/sandbox.rs` `run_agent` builds a
`Mount { target: "/workspace", source: <host workspace path>, typ:
MountType::BIND }`, so the host can run git against the same directory
the agent writes to. Nothing in the design had to change.

**The brief's ADR pointer.** The brief cites "ADR-0012" as the settled
design. In this repo `docs/adr/0012-*` is
`gates-are-judged-by-whether-they-summon-an-absent-human`, which is a
different subject. I built to the `CONTEXT.md` **Stall** / **Oscillation**
/ **Idleness** / **Advance** definitions and to the brief's own
prose, both of which are consistent and complete on the points that
mattered (Oscillation advances, Idleness never does, one shared
allowance, the advance is never a verdict on quality). No new ADR was
written — the brief did not ask for one and the terminology it depends
on is already in `CONTEXT.md`.

**Window size reconciliation.** The brief says to keep the last 10
samples *and* that Idleness is N=15 consecutive identical samples. A
bare 10-sample window can never witness a 15-sample idle run, so
`policy::stall_window_len` retains `max(10, idleness_samples)` and both
questions are answered from one retained sequence. The oscillation
scan's behaviour on the last 10 is unaffected.

**One guard is applied twice, deliberately.** The container has to be
killed for an oscillation to be actionable, but killing a run that
could not advance anyway would cost it the rest of its budget for
nothing. So the guards that are knowable before the container starts
(the shared `advances_used` allowance, the engine-forced bypass, the
budget floor) are applied up front by
`chain_walker::oscillation_kill_window`, the at-base-SHA guard is
checked by the sampler on the tick it reports an oscillation, and the
full decision is then re-made after the container exits by
`decide_oscillation_advance_action`. The post-run decision is the
single source of truth; the pre-launch window is an optimisation that
keeps the brief's "log the Oscillation and let the run continue to its
existing terminal state" promise true in every case.

**What is not directly unit-tested.** The sampling loop itself
(`sandbox::run_container`'s third `select!` branch and `observe_stall`)
needs a live docker daemon and a running container, which this repo's
test suite does not have. Everything that loop *decides* was extracted
into pure functions that are tested — `classify_stall`, `StallTracker`,
`oscillation_kill_window`, `decide_oscillation_advance_action`, the
sampler against a real git repo — so what remains untested is the
plumbing between them, not any judgement.

**Security fixes: why the sampler got a whole isolated git dir.** The
security review asked for `git diff --no-ext-diff --no-textconv HEAD`
plus `-c` overrides. I checked each vector against real git (2.47) and
those flags are not sufficient: they close `diff.external`,
`diff.<driver>.command` and `.textconv`, and `-c core.fsmonitor=false`
closes fsmonitor, but *any* command that compares worktree content
against the index still runs the clean filter named by
`.git/info/attributes` — which the agent can write, and which
`GIT_ATTR_SOURCE`/`core.attributesFile` overrides do not reach. There
is no supported way to make git ignore a repository's own
`.git/config`, so the only complete answer was the review's other
option: an isolated environment. The sampler now reads the worktree
through a bellows-owned scratch git dir that borrows the workspace's
object store via `objects/info/alternates` and nothing else.
