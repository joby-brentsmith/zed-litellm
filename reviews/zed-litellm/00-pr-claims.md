# PR Claims — Verification Agenda

Extracted from README.md, commit messages, and code comments. Each claim lists the evidence file(s) to verify against.

1. **Tool queries LiteLLM `/model/info` endpoint** — verify `fetch_model_info` builds `{base_url}/model/info` (src/main.rs:172)
2. **Field mappings**: max_input_tokens→max_tokens, supports_function_calling→tools(default true), supports_vision→images(default false), supports_parallel_function_calling→parallel_tool_calls(default false), supports_prompt_caching→prompt_cache_key, supports_reasoning→reasoning_effort — verify each in `to_zed_models` (src/main.rs:319-358)
3. **Non-chat models are skipped** — verify mode filter (src/main.rs:302-306)
4. **Duplicate deployments collapsed** — verify dedup (src/main.rs:308-310)
5. **Existing hand-tuned entries preserved verbatim; only new appended** — verify `merge_models` (src/main.rs:398-425)
6. **--replace regenerates all entries** — verify main flow (src/main.rs:122-127)
7. **Comments and formatting preserved via JSONC CST** — verify `merge_into_settings` uses CST (src/main.rs:437)
8. **--write creates .bak backup** — verify backup logic (src/main.rs:141-145)
9. **max_output_tokens omitted when >= max_tokens** — verify guard (src/main.rs:334-338)
10. **--probe checks accept AND emit reasoning_content** — verify `probe_one_model` (src/main.rs:231-267)
11. **Env vars: LITELLM_URL and LITELLM_API_KEY** — verify clap attributes (src/main.rs:26,30)
12. **api_url in output includes /v1** — verify format string (src/main.rs:100)
13. **All 5 Zed-required capability fields present** (tools, images, parallel_tool_calls, prompt_cache_key, interleaved_reasoning) — verify capabilities object (src/main.rs:343-359)
14. **mode: None treated as chat** (implicit, not documented) — verify mode filter allows None through (src/main.rs:303)
