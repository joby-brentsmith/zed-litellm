# Engineering Manager Synthesis

## Claims verified by experts
All 14 claims verified against code (not narrative). Key confirmations:
- Field mappings (CQ-expert): all 7 verified line-by-line ✓
- Probe checks emission not just acceptance (CQ-expert): ✓
- CST preserves comments (Arch-expert): ✓
- All 5 required capability fields present (Test-expert): ✓

## Critical issues to fix (blocking production-readiness)

### Fix 1: `mode: "responses"` models get wrong `chat_completions` (CQ1)
The mode filter allows `"responses"` through, but `to_zed_models` never emits `chat_completions` in capabilities. Zed defaults it to `true`, sending a Responses-only model to the wrong endpoint. Not currently triggerable (no `responses`-mode models in LiteLLM data) but is a latent bug that would silently break on first encounter.

**Fix**: Emit `chat_completions: false` when `mode == Some("responses")`.

### Fix 2: No test for probe response parsing (T1)
`ProbeResponse`/`ProbeChoice`/`ProbeMessage` deserialization is untested. The probe makes network calls so `probe_one_model` can't be unit tested, but the response parsing can be.

**Fix**: Add a test deserializing mock responses with/without `reasoning_content`.

### Fix 3: `--probe` + `--replace` interaction undocumented (D2)
A user running `--probe --write` (without `--replace`) expects existing models' `interleaved_reasoning` to be updated, but existing entries are preserved verbatim. This is by design but undocumented.

**Fix**: Add a note to README.

## Non-blocking (noted, not fixed)
- S1: API key in `--api-key` flag visible in `ps` — env var is the documented primary path
- CQ2: No `--reasoning-effort` validation — default is valid, invalid values fail at Zed load
- CQ3: Backup overwrite on repeated runs — acceptable for a personal tool
- A4: Probe hardcodes `reasoning_effort: "medium"` — safe fallback on rejection
