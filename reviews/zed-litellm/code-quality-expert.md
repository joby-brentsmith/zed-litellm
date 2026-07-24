# Code Quality Expert Report

## Verified claims
- Claim 2 (field mappings): ✓ — all six mappings verified against `to_zed_models` lines 319-358.
- Claim 9 (max_output_tokens guard): ✓ — line 336 `if max_output < max_tokens` correctly omits untrustworthy values.
- Claim 10 (probe logic): ✓ — `probe_one_model` checks `reasoning_content.is_some()` in the response, not just HTTP 200.

## Findings

### CQ1: `mode: "responses"` models get wrong `chat_completions` default (Bug)
`src/main.rs:303-306` — the mode filter allows `"responses"` through, but `to_zed_models` never sets `chat_completions` in the capabilities object. Zed's `OpenAiCompatibleModelCapabilities` defaults `chat_completions` to `true` (via `#[serde(default = "default_true")]`). A `responses`-mode model that only supports the Responses API would get `chat_completions: true`, causing Zed to send it to `/chat/completions` — the wrong endpoint.

**Fix**: When `mode == Some("responses")`, emit `chat_completions: false` in capabilities. This matches Zed's own docs: "If a model only works with the Responses API, set `capabilities.chat_completions` to `false`."

### CQ2: No validation of `--reasoning-effort` value (Minor)
`src/main.rs:62` — `reasoning_effort` is a free-form `String`. Passing `--reasoning-effort foo` writes `reasoning_effort: "foo"` to settings, which Zed rejects at load time. Valid values per Zed docs: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.

**Fix**: Add a value parser or at minimum a documented note. Low priority since the default (`medium`) is valid.

### CQ3: Repeated `--write` overwrites the only backup (Minor)
`src/main.rs:141` — the backup path is always `settings.json.bak`. A second `--write` run overwrites the first backup. If the first run was correct and the second had a bug, the good backup is lost.

**Not a blocker** for a personal tool, but could rotate to `.bak1`/`.bak2` if desired.

### CQ4: `max_tokens` f64→u64 cast is safe but unguarded (Pass, noting)
`src/main.rs:321` — `tokens as u64` on an `f64`. Negative or NaN values would cast to 0 or garbage. LiteLLM won't send these in practice. Not a blocker.

### CQ5: Variable shadowing in `interleaved_support` check (Style)
`src/main.rs:313-315` — the closure parameter `name` shadows the outer loop's `name` variable (line 298). Works correctly but is slightly confusing on first read.

## Verdict
CQ1 is a latent bug worth fixing. The rest are minor.
