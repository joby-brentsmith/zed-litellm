# zed-litellm

Sync models from a [LiteLLM proxy](https://docs.litellm.ai/docs/simple_proxy)
into [Zed](https://zed.dev)'s `openai_compatible` provider settings.

Zed's OpenAI-compatible providers require models to be listed manually in
`settings.json`, because the standard OpenAI `/models` endpoint only returns
model IDs — no context windows or capability flags (see
[zed#61553](https://github.com/zed-industries/zed/pull/61553)). LiteLLM's
`/model/info` endpoint *does* return that metadata, so this tool queries it
and writes accurate model entries into your Zed settings.

## Prerequisites

- A running [LiteLLM proxy](https://docs.litellm.ai/docs/simple_proxy) with
  `/model/info` accessible (any LiteLLM deployment exposes this)
- An API key with access to `/model/info`
- [Zed](https://zed.dev) installed

## Install

```sh
cargo install --git https://github.com/joby-brentsmith/zed-litellm
```

Or clone and build:

```sh
git clone https://github.com/joby-brentsmith/zed-litellm
cd zed-litellm
cargo install --path .
```

The binary is `zed-litellm` on your PATH.

## Usage

```sh
# Dry run: prints the merged settings.json to stdout
zed-litellm --url http://localhost:4000 --api-key sk-...

# Update ~/.config/zed/settings.json in place (writes a .bak backup first)
zed-litellm --url http://localhost:4000 --api-key sk-... --write
```

The API key can also be provided via `LITELLM_API_KEY`, and the URL via
`LITELLM_URL`. Zed hot-reloads `settings.json`, so new models appear in the
model picker immediately — no restart needed.

Options:

| Flag | Default | Description |
| --- | --- | --- |
| `--url` | `$LITELLM_URL` | LiteLLM proxy base URL; `/v1` suffix ok |
| `--api-key` | `$LITELLM_API_KEY` | Key with access to `/model/info` |
| `--provider` | `litellm` | Provider id under `language_models.openai_compatible` |
| `--settings` | `~/.config/zed/settings.json` | Zed settings file to update |
| `--write` | off | Modify the file (default is dry-run to stdout) |
| `--replace` | off | Regenerate all entries instead of preserving existing ones |
| `--probe` | off | Probe reasoning models for `interleaved_reasoning` support (adds one minimal chat-completions call per reasoning-capable model). Only affects newly-discovered models unless `--replace` is also used |
| `--reasoning-effort` | `medium` | Effort assigned to models with `supports_reasoning` |

## Field mapping

| LiteLLM `/model/info` | Zed `available_models` |
| --- | --- |
| `model_name` | `name` |
| `max_input_tokens` (fallback `max_tokens`, then 128000) | `max_tokens` |
| `max_output_tokens` (omitted when >= `max_tokens` — LiteLLM often reports it as the context window, which causes overflow errors) | `max_output_tokens` |
| `supports_function_calling` (default true) | `capabilities.tools` |
| `supports_vision` (default false) | `capabilities.images` |
| `supports_parallel_function_calling` (default false) | `capabilities.parallel_tool_calls` |
| `supports_prompt_caching` (default false) | `capabilities.prompt_cache_key` |
| `supports_reasoning` | `reasoning_effort` (value from `--reasoning-effort`) |
| `interleaved_reasoning` (probed) | `capabilities.interleaved_reasoning` — only when `--probe` confirms the model both accepts and emits `reasoning_content` |

Non-chat models (`mode` of `embedding`, `image_generation`, ...) are skipped,
and duplicate deployments of the same `model_name` are collapsed.

`interleaved_reasoning` defaults to `false` because LiteLLM's `/model/info`
cannot report it. Pass `--probe` to send a minimal test request to each
reasoning-capable model: it sets `interleaved_reasoning: true` only when the
model both accepts `reasoning_content` in input *and* emits it in its own
response — distinguishing real support from silent accept-and-ignore. The
probe adds one near-zero-cost chat-completions call (`max_tokens: 1`) per
reasoning model, so it's opt-in.

## Notes

- **Existing model entries are preserved verbatim** — manual tuning like
  `display_name`, `reasoning_effort`, or capabilities LiteLLM can't report
  (`interleaved_reasoning`, `chat_completions`, ...) survives a sync. Only
  models not already listed are appended; nothing is removed. Pass
  `--replace` to regenerate every entry from `/model/info` instead.
- Settings are edited via a JSONC CST, so comments and formatting in your
  `settings.json` are preserved. Within the target provider block, only
  `api_url` and `available_models` are touched — other keys such as
  `custom_headers` are left alone.
- `--write` creates a `settings.json.bak` backup next to the settings file
  before modifying it.

## Build

```sh
cargo build --release
# binary at target/release/zed-litellm
```

## License

MIT — see [LICENSE](LICENSE).
