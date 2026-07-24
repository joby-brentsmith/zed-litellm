# Documentation Expert Report

## Verified claims
- README field mapping table: ✓ — all 7 mappings in the table match the code (verified each field in `to_zed_models`).
- README usage examples: ✓ — `--url`, `--api-key`, `--write` flags match clap args.
- README env var documentation: ✓ — `LITELLM_URL` and `LITELLM_API_KEY` match code.

## Findings

### D1: `mode: None` treated as chat is not documented (Minor)
README says "Non-chat models (`mode` of `embedding`, `image_generation`, ...) are skipped" but doesn't mention that `mode: None` (unset) is treated as chat. This is the correct behavior (LiteLLM chat models often don't set `mode`), but a user looking at the filter logic would be surprised.

### D2: `--probe` interaction with `--replace` not documented (Minor)
README documents `--probe` and `--replace` separately but doesn't explain that `--probe` alone only affects newly-discovered models (existing entries preserved). A user who runs `--probe --write` expecting their existing models' `interleaved_reasoning` to be updated would be surprised.

**Fix**: Add a note to the `--probe` description.

### D3: README says "Discovered models default to a 128k context window" (Pass)
This matches `DEFAULT_MAX_TOKENS = 128_000` on line 13. ✓

### D4: `--replace` description says "existing entries are preserved" by default (Pass)
README and code comment both describe the preserve-by-default behavior. ✓

### D5: Probe "near-zero-cost" claim is accurate (Pass)
`max_tokens: 1` with `reasoning_effort: medium` — the model generates at most 1 token of output. The reasoning overhead is small. ✓

## Verdict
D2 is worth a doc fix. D1 is minor. No blocking documentation issues.
