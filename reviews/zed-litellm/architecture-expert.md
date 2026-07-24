# Architecture Expert Report

## Verified claims
- Claim 5 (preserve existing): ✓ — `merge_models` keeps existing entries verbatim in `models = existing`, only appends discovered models whose names aren't already present.
- Claim 7 (CST preservation): ✓ — `merge_into_settings` uses `CstRootNode::parse` + `root.to_string()`, preserving comments. `existing_models` uses serde parsing (strips comments) but only extracts model values, so this is safe — the CST path operates on raw text.
- Claim 12 (api_url includes /v1): ✓ — `format!("{base_url}/v1")` on line 100, written to settings on line 444.

## Findings

### A1: `--probe` alone can't update interleaved_reasoning on existing entries (Design limitation)
When `--probe` is used WITHOUT `--replace`, the probe discovers interleaved support for a model, but `merge_models` preserves the existing entry verbatim (existing wins). So `--probe --write` (without `--replace`) won't update `interleaved_reasoning` on any existing model — it only applies to newly-discovered models.

This is consistent with the "preserve manual tuning" design, but a user might expect `--probe` to update existing entries. The README doesn't explicitly call out this interaction.

**Recommendation**: Document that `--probe` only affects new (appended) models unless `--replace` is also used. Not a code change.

### A2: Two parsing paths for the same file (Acceptable)
`existing_models` uses `parse_to_serde_value` (serde, strips comments) while `merge_into_settings` uses `CstRootNode::parse` (CST, preserves comments). Both parse the same settings text. This is intentional — the serde path extracts data, the CST path modifies text. No inconsistency risk since both are standard parsers for valid JSONC.

### A3: `mode: None` treated as chat without explicit handling (Design choice, undocumented)
`src/main.rs:303` — `if let Some(mode) = mode { ... }` means `mode: None` falls through and the model is treated as chat. This matches LiteLLM's behavior (chat models often don't set `mode`), but it's an implicit assumption. See CQ1 for the flip side: `mode: "responses"` is also allowed through but gets the wrong `chat_completions` default.

### A4: Probe sends `reasoning_effort: "medium"` unconditionally (Edge case)
`src/main.rs:239` — the probe hardcodes `reasoning_effort: "medium"`. A model that only supports, say, `high` (LiteLLM reports per-level support flags) would reject the probe request, resulting in `interleaved_reasoning: false` — a false negative. The fallback is safe (model gets `false`, not a crash), but the probe would miss a genuinely-interleaved model.

**Recommendation**: Low priority. The probe could omit `reasoning_effort` and let the model use its default, but some models require it to trigger reasoning at all.

## Verdict
A1 should be documented. A3/CQ1 is the only code-level architectural issue (the `responses` mode handling). No blocking architecture issues.
