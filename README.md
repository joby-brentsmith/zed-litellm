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
| `--reasoning-effort` | `high` | Effort assigned to reasoning-capable models without a card-set `default_reasoning_effort`. Default `high` because llaminar's GLM/Qwen SGLang rigs only treat `low`/`high` as faithful tiers — `medium` silently coerces to maximum reasoning burn |
| `--exclude` | off | Skip discovered models whose name contains any given substring (repeatable). For internal capacity a broad key can see but shouldn't land in a picker (llaminar: `--exclude baseten --exclude auto --exclude upg-test`) |
| `--check-liveness` | off | Send a 1-token completion to each discovered chat model and report which are live vs returning an HTTP error (catches stale aliases / dead backends after a cutover). Report-only — dead models are still synced |
| `-f`, `--full` | off | Shorthand for `--write --replace --probe --check-liveness` — the one-shot "make my Zed config accurate" sync. Regenerates every model entry from `/model/info`, probes reasoning models for interleaved_reasoning, checks liveness, and writes the result |

## Field mapping

| LiteLLM `/model/info` | Zed `available_models` |
| --- | --- |
| `model_name` | `name` |
| `max_input_tokens` (fallback `max_tokens`, then 128000) | `max_tokens` |
| `max_output_tokens` | a sane cap — LiteLLM's value is unreliable (often equal to the context window — the impossible `out==in` value), so the tool honors the card value only when it's a deliberate sub-window cap, and otherwise emits `DEFAULT_OUTPUT_TOKENS` (65536) clamped just *below* the context window (a cap exactly at the advertised window is the llaminar empty-generation class: the backend collapses the budget to ~1 and returns an empty 200 — joby-software/llaminar#94) |
| `supports_function_calling` (default true) | `capabilities.tools` |
| `supports_vision` (default false) | `capabilities.images` |
| `supports_parallel_function_calling` (default false) | `capabilities.parallel_tool_calls` |
| `supports_prompt_caching` (default false) | `capabilities.prompt_cache_key` |
| `supports_reasoning` | `reasoning_effort` — the card's `default_reasoning_effort` (when all deployments agree) wins; otherwise the `--reasoning-effort` value |
| `interleaved_reasoning` (probed) | `capabilities.interleaved_reasoning` — only when `--probe` confirms the model both accepts and emits `reasoning_content` |

Non-chat models (`mode` of `embedding`, `image_generation`, ...) are skipped,
and duplicate deployments of the same `model_name` are merged into one conservative card per the semantics established for the same endpoint in vscode-llaminar ([joby-software/vscode-llaminar#61](https://github.com/joby-software/vscode-llaminar/issues/61), [PR #63](https://github.com/joby-software/vscode-llaminar/pull/63)): capability booleans are AND-merged (any-false → false, all-true → true, else unknown→default-with-warning), and token limits are the minimum across the reporting deployments. First-wins dedupe would let entry ordering decide your model picker.

`interleaved_reasoning` defaults to `false` because LiteLLM's `/model/info`
cannot report it. Pass `--probe` to send a minimal test request to each
reasoning-capable model: it sets `interleaved_reasoning: true` only when the
model both accepts `reasoning_content` in input *and* emits it in its own
response — distinguishing real support from silent accept-and-ignore. The
probe adds one near-zero-cost chat-completions call (`max_tokens: 1`) per
reasoning model, so it's opt-in.

## Notes

### Known llaminar (moose gateway) facts encoded here

These are live-verified behaviors of Joby's llaminar gateway
(`llaminar.joby.aero`, LiteLLM ~1.102 on moose k3s):

- **`/model/info` requires key access.** A key without it gets 401/403; the
  tool exits with a hint pointing at the gateway key UI. The downstream
  client symptom of an access-less key is “placeholder token limits.”
- **The v-less `/model/info` is the picker-safe list.** It returns the
  team-accessible cards and already excludes internal failover capacity
  (the DB-only Baseten twins have no team access). `/v2/model/info` returns
  *all* cards, including internal ones — don't point bulk syncs there. A
  team-scoped virtual key is the intended credential; an admin key sees
  extras (e.g. `baseten/muse-spark-open`), so pass `--exclude baseten` (and
  `auto`/`upg-test-*` if visible) on admin-key runs.
- **Effort ladder on the GLM/Qwen SGLang rigs:** only `low` and `high` are
  faithful; `medium` silently coerces to maximum burn (gateway-side ladder
  rewrite pending deploy). Hence the `high` default and the card-
  `default_reasoning_effort` override — card defaults are operator-curated
  and always win.
- **`max_output_tokens == window` is a real failure mode** on the GLM 5.3
  family — the empty-200 budget collapse ([llaminar#94](https://github.com/joby-software/llaminar/issues/94)) — so generated caps
  always leave reserve headroom below the advertised window.

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

## Null-field warnings

If any chat model on the gateway has `null` capability fields (the recurring
bug where sbatch register payloads omit them), the sync prints a warning to
stderr listing each affected model and which fields are null. The sync still
runs (falling back to defaults), but the warning surfaces the gap so you know
the gateway cards need fixing — not the Zed settings. This is the same class
of bug that `llaminar/tools/lint-register-payloads.py` catches at the source.

## Liveness check

Pass `--check-liveness` to send a 1-token chat completion to each discovered
chat model before syncing. Models that return a 2xx are reported as live;
non-2xx (e.g. 403 from an upstream rate-limit, 500 from a dead backend) or
network errors are reported as `NOT LIVE` with the error detail. This catches
stale aliases pointing at rolled-back backends before they land in your Zed
model picker. Dead models are still synced — the check is report-only so you
can decide whether to remove them.

## Zed task integration

Add a task to `~/.config/zed/tasks.json` so you can sync from the command
palette (`task::spawn` → "sync litellm models") or a keybinding:

```json
[
  {
    "label": "sync litellm models",
    "command": "zed-litellm -f",
    "hide": "on_success"
  }
]
```

Zed hot-reloads `settings.json`, so new models appear in the picker
immediately after the task finishes. To bind it to a key, add to
`keymap.json`:

```json
{
  "context": "Workspace",
  "bindings": {
    "cmd-shift-l": ["task::Spawn", { "task_name": "sync litellm models" }]
  }
}
```

## Build

```sh
cargo build --release
# binary at target/release/zed-litellm
```

## License

MIT — see [LICENSE](LICENSE).
