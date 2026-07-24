# Test Expert Report

## Verified claims
- Claim 13 (all 5 required capability fields): ✓ — `generated_capabilities_include_all_zed_required_fields` test asserts all five fields are present. Confirmed the code emits all five (tools, images, parallel_tool_calls, prompt_cache_key, interleaved_reasoning).
- 12 tests total: covers model mapping, capabilities, non-chat filtering, dedup, reasoning effort, max_output_tokens omission, interleaved mapping, token fallbacks, merge preservation, existing model reading, comment preservation, structure creation, URL normalization.

## Findings

### T1: No test for probe response parsing (Gap)
`probe_one_model` (src/main.rs:231-267) and the `ProbeResponse`/`ProbeChoice`/`ProbeMessage` deserialization structs are untested. The probe makes a network call, so `probe_one_model` itself can't be unit tested without a mock HTTP server — but the response parsing logic (checking `reasoning_content.is_some()`) can be tested by deserializing a mock JSON response into `ProbeResponse`.

**Fix**: Add a test that deserializes a mock probe response with and without `reasoning_content` and verifies the presence check works.

### T2: No test for `--replace` path (Gap)
The `main` function's `--replace` branch (lines 122-127) vs the merge branch (lines 128-136) is not tested. The merge logic IS tested via `merge_models_preserves_existing_entries_verbatim`, but the `--replace` path (which skips merge and uses discovered models directly) has no test.

**Acceptable**: `--replace` is a trivial branch (use discovered models as-is). The model generation is already tested.

### T3: No test for `max_tokens` f64→u64 edge cases (Minor)
The `as u64` cast on `f64` token counts isn't tested with edge cases (0, negative, fractional). Low priority.

### T4: `omits_max_output_tokens_when_untrustworthy` test is thorough (Pass)
Tests both the untrustworthy case (max_output == max_input → omit) and the trustworthy case (max_output < max_input → keep). ✓

### T5: `merge_preserves_comments_and_other_settings` test is good (Pass)
Verifies comments, other settings keys, and `custom_headers` within the provider block survive. ✓

## Verdict
T1 is actionable — add a probe response parsing test. T2/T3 are acceptable gaps.
