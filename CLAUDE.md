# Bellows

Rust orchestrator that dispatches AFK Claude Code agents to work on labeled GitHub issues inside Docker sandboxes. See `RESEARCH.md` for the full briefing.

## Agent skills

### Issue tracker

Issues live in GitHub Issues; use the `gh` CLI for all operations. See `docs/agents/issue-tracker.md`.

### Triage labels

Canonical label strings (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`) — no overrides. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
