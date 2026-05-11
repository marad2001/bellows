# SSH deploy keys, per-repo opt-in, for private cross-repo Rust deps in the sandbox

The bellows agent and cargo-checks gate containers run `cargo build`
and `cargo test` against the cloned target repo. When that repo's
Cargo.toml references private git deps via SSH URLs (e.g. workboard-*
repos referencing `ssh://git@github.com/marad2001/workboard-core.git`),
cargo cannot fetch those deps from inside the sandbox: the container
has no SSH client and no credentials. We extend the sandbox with
per-repo opt-in SSH deploy keys, stored in a new bellows-managed
Docker volume (parallel to `bellows-claude-credentials`), populated
via `bellows setup-deploy-keys add/list/remove`, and mounted read-only
at `/home/bellows/.ssh/` only into containers spawned for `[[repo]]`
entries whose config declares `deploy_keys = [...]`.

## Considered alternatives

- **HTTPS PAT with `insteadOf` URL rewriting** (reusing `BELLOWS_GITHUB_TOKEN`). Rejected: a leak from the sandbox would expose write access across every repo in the PAT's scope; the convention across the org is SSH deploy keys (matches existing CI); Cargo.toml URLs stay unchanged with SSH.
- **Skipping in-sandbox compile entirely, relying on GitHub CI as the only gate.** Rejected: the agent loses its fast feedback loop — every iteration becomes a push-and-wait. The token cost of blind iteration against CI is likely higher than the engineering cost of solving credentials.
- **Mounting the operator's personal `~/.ssh/` into the container.** Rejected: grants the sandboxed agent the operator's full GitHub identity (read + write to every repo they collaborate on) — the worst-case blast radius of any option considered.
- **Host-path bind mount instead of bellows-managed volume.** Rejected: operator failure modes for SSH config are silent and subtle (wrong file mode, missing `known_hosts`, malformed `Host` stanza); bellows handling them via guided commands eliminates a class of operator pain.
- **Global mount across all repos** instead of per-repo `deploy_keys = [...]`. Rejected: would mount deploy keys into bellows-on-bellows runs (and any other no-private-deps repo) for no reason. Per-repo opt-in matches the rest of bellows's "no creds in sandbox by default" posture.
- **Free-form interactive setup shell** (parallel to `bellows setup-auth`). Rejected for the same reason as host-path: SSH config failure modes are silent; guided commands (`add`, `list`, `remove`) reduce operator skill required.
- **GitHub Machine User account** with org-wide SSH access. Rejected: more overhead than warranted at the user's current scale; would consume a paid seat. Worth revisiting if many shared crates emerge.

## Consequences

- Agent and gate containers gain a read-only mount at `/home/bellows/.ssh/` for opted-in repos. No credentials enter the sandbox by default — the per-repo opt-in is the explicit, visible activation.
- Each operator carries an extra one-time setup step `bellows setup-deploy-keys add <name>` per shared private crate, plus registering the public half as a deploy key on the shared repo's GitHub settings. Documented in the README.
- Cargo.toml URLs in consuming repos stay as `ssh://git@github.com/...`. No source-tree edits required across consuming repos.
- The deploy-keys volume is parallel to but distinct from `bellows-claude-credentials`: separate lifecycle, separate setup command, separate purpose.
- Multi-key support is structurally available — multiple `add` calls layer `Host` stanzas in `~/.ssh/config`. Multiple keys for the same `Host github.com` would require Host aliases + Cargo.toml URL rewriting; not solved by this ADR.
- A future operator who prefers HTTPS PAT can be served by a separate, additive config path (`url.insteadOf` rewriting via cargo config) — not blocked by this design.
