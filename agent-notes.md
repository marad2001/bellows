

<!-- bellows implement-crash recovery appended this entry because the implement-phase agent exited non-zero AND produced no commits in the workspace. Without this entry the workspace would have no changes to commit, the agent branch would never be pushed, and the source issue would silently stay at agent-in-progress. The presence of this entry lets the rest of the pipeline run through to a draft PR + agent-failed label. -->

## Implement phase crashed

Bellows-synthesised entry. The implement-phase agent exited with code `1` and produced no commits in the workspace; no agent-authored changes survived. A captured prefix of the agent's stderr/stdout tail follows so the operator can diagnose the failure without fetching the container's logs.

```
API Error: 500 {"type":"error","error":{"type":"api_error","message":"Internal server error"},"request_id":"req_011Cb6gzGGuPUgvijXCRjYUR"}

```
