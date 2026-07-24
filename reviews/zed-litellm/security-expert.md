# Security Expert Report

## Verified claims
- Claim 11 (env vars): ✓ — `LITELLM_URL` and `LITELLM_API_KEY` with `hide_env_values = true` on the key.
- API key sent via `Authorization: Bearer` header — standard, correct.

## Findings

### S1: API key visible in process list via --api-key flag (Low)
`src/main.rs:30` — the `--api-key` flag accepts the key as a CLI argument. Unlike the env var (which has `hide_env_values = true`), the flag value is visible in `ps aux` / `/proc/*/cmdline` while the process runs. The README documents env vars as the primary method, so this is low risk, but a user who copies the example `--api-key sk-...` literally would expose their key.

**Recommendation**: No code change needed for a personal tool; document the env-var preference more prominently (README already says "can also be provided via" — could say "prefer env var to avoid leaking in process list").

### S2: No TLS pinning / verification concerns (Pass)
`ureq` uses `rustls` by default (verified in Cargo.lock). No custom certificate handling. ✓

### S3: No injection risk in settings path (Pass)
`--settings` path is user-controlled and used directly with `fs::read_to_string`/`fs::write`. No shell expansion or symlink resolution issues for a local CLI tool. ✓

## Verdict
No blocking security issues. S1 is informational.
