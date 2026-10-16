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
- An API key — `/model/info` is authenticated but not access-gated; the
  key's **scope** (team/model list) determines which cards are visible
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
| `--api-key` | `$LITELLM_API_KEY` | Gateway key — its scope (team/model list) determines which cards `/model/info` returns |
| `--provider` | `litellm` | Provider id under `language_models.openai_compatible` |
| `--settings` | `~/.config/zed/settings.json` | Zed settings file to update |
| `--write` | off | Modify the file (default is dry-run to stdout) |
| `--replace` | off | Regenerate all entries instead of preserving existing ones |
| `--probe` | off | Probe reasoning models for `interleaved_reasoning` support (adds one minimal chat-completions call per reasoning-capable model). Only affects newly-discovered models unless `--replace` is also used |
| `--reasoning-effort` | `high` | Effort assigned to reasoning-capable models without a card-set `default_reasoning_effort`. Default `high`: the faithful GLM tier (~22% of default burn, and `medium` is ladder-hook-translated to `high` there anyway); inert on qwen3.8 whose cards bake thinking off (see the llaminar notes below for the per-rig ladders) |
| `--exclude` | off | Skip discovered models whose name contains any given substring (repeatable). For internal capacity a broad key can see but shouldn't land in a picker (llaminar: `--exclude baseten --exclude auto --exclude upg-test`) |
| `--check-liveness` | off | Send a 1-token completion to each discovered chat model and report which are live vs returning an HTTP error (catches stale aliases / dead backends after a cutover). Report-only — dead models are still synced |
| `-f`, `--full` | off | Shorthand for `--write --replace --probe --check-liveness` — the one-shot "make my Zed config accurate" sync. Regenerates every model entry from `/model/info`, probes reasoning models for interleaved_reasoning, checks liveness, and writes the result |

## Field mapping

| LiteLLM `/model/info` | Zed `available_models` |
| --- | --- |
| `model_name` | `name` |
| `max_input_tokens` (fallback `max_tokens`, then 128000) | `max_tokens` |
| `max_output_tokens` | a sane cap — llaminar deliberately pins glm-family cards to the out==in 262144 shape (the 2026-09-22 clamp, mirroring the Baseten twins), and other LiteLLM deployments report the same shape through card rot. Either way an out==window cap in Zed guarantees a 400 on summed-gate routes, so the tool honors the card value only when it's a deliberate sub-window cap, and otherwise emits `DEFAULT_OUTPUT_TOKENS` (65536) — always **strictly below** the context window (a reserve slice is carved when the fallback would otherwise equal the window) |
| `supports_function_calling` (default true) | `capabilities.tools` |
| `supports_vision` (default false) | `capabilities.images` |
| `supports_parallel_function_calling` (default false) | `capabilities.parallel_tool_calls` |
| `supports_prompt_caching` (default false) | `capabilities.prompt_cache_key` |
| `supports_reasoning` | `reasoning_effort` — the card's `default_reasoning_effort` wins when **every** deployment sets the same value (an abstaining deployment falls back to the `--reasoning-effort` value, so one backend's curation is not stretched onto a sibling deployment); otherwise the `--reasoning-effort` value |
| `interleaved_reasoning` (probed) | `capabilities.interleaved_reasoning` — only when `--probe` confirms the model both accepts and emits `reasoning_content` |
| (nothing — emitted unconditionally) | `capabilities.max_tokens_parameter: true` — Zed's `openai_compatible` default sends the output cap as `max_completion_tokens`; classic `max_tokens` is what every backend on this fleet accepts (SGLang, vLLM, the Baseten twins), so the classic shape is pinned explicitly. Unknown to older Zed, which ignores it |

Non-chat models (`mode` of `embedding`, `image_generation`, ...) are skipped,
and duplicate deployments of the same `model_name` are merged into one conservative card per the semantics established for the same endpoint in vscode-llaminar ([joby-software/vscode-llaminar#61](https://github.com/joby-software/vscode-llaminar/issues/61), [PR #63](https://github.com/joby-software/vscode-llaminar/pull/63)): capability booleans are AND-merged (any-false → false, all-true → true, else unknown), token limits are the minimum across the reporting deployments, `default_reasoning_effort` is honored only when every deployment sets the same value, and the group's mode must be unanimous — a chat/responses mix is excluded from the sync (a Responses-only deployment cannot serve `/chat/completions`, so no single API serves every member). Note the one visible consequence of the AND-merge: a deployment group with mixed `supports_reasoning` (true + null) merges to *unknown*, which omits `reasoning_effort` (with a null-field warning) rather than guessing — first-wins dedupe would have kept whichever entry came first.

`interleaved_reasoning` defaults to `false` because LiteLLM's `/model/info`
cannot report it. Pass `--probe` to send a minimal test request to each
reasoning-capable model (the probe sends the exact `reasoning_effort` each
generated entry will carry, and neither the probe nor `--check-liveness`
touches models matched by `--exclude` — excluded backends are usually
excluded because they are unreachable): it sets `interleaved_reasoning: true` only when the
model both accepts `reasoning_content` in input *and* emits it in its own
response — distinguishing real support from silent accept-and-ignore. The
probe adds one near-zero-cost chat-completions call (`max_tokens: 1`) per
reasoning model, so it's opt-in.

## Notes

### Known llaminar (moose gateway) facts encoded here

These are live-verified behaviors of Joby's llaminar gateway
(`llaminar.joby.aero`, LiteLLM 1.102.1 on moose k3s):

- **`/model/info` is authenticated but NOT access-gated** (live-verified on
  moose: the route is an info-route with no role gate, and it responds with
  the cards in your **key's scope**). A 200 with an empty or partial list
  means your key's team/model list has no matching cards — the client-visible
  symptom is placeholder token limits. 401/403 means the key itself was
  rejected (invalid/expired), not missing "permission" for the route.
- **The v-less `/model/info` is the caller's allowlist view; `/v2/model/info`
  is every card.** The difference is the caller's scope, not card fields
  (live-verified: the config-file master key gets the identical 24 from both;
  a team or virtual key gets its team's model list). The four DB-only
  GLM/Baseten HA twins (`glm-[5.3,5.3-flash]-baseten`, `baseten/glm-*`) sit
  on **no** team allowlist by design (DOPS-14376), so no virtual key sees
  them via v-less. Other internal cards *do* appear for scoped keys
  (`baseten/muse-spark-open` via the R&D grant; `deepseek-v4.1-flash-baseten`
  is on every team list) — so team-accessible ≠ picker-clean: pair broad-key
  syncs with `--exclude baseten` (substring — check a dry run first).
  Quirk: teams with an empty `models` list are unscoped and see all 24.
- **Effort ladders are per-rig, not fleet-wide (llaminar #113):** GLM rigs
  treat only `low`/`high` as faithful, and since the gateway-side ladder hook
  deployed (llaminar #114, operator GO 2026-10-13), a GLM `medium` is
  *translated to* `high` rather than riding the max-burn default. qwen3.8
  rigs are different: their tier map is `xhigh` (default)/`medium`/`low` — no
  `high` (which 400s in the thinking-on regime) — but their cards bake
  thinking *off*, so a Zed-sent effort string is inert there today. Hence the
  `high` default (right for GLM, inert for qwen) and the card-
  `default_reasoning_effort` override — the operator-side per-card remedy
  (**effective only when `supports_reasoning` resolves to true** — it is not
  an escape hatch for a null `supports_reasoning`, which stays a
  fix-the-card issue and warns at runtime)
  (no live card sets it yet; on this fleet it's forward-compat, but it wins
  whenever set). If ops ever flips qwen thinking on, they must set
  `default_reasoning_effort` on those cards — this tool already honors it.
- **Never cap output at the advertised window.** Two reasons: (a) an
  out==window cap guarantees `prompt + max_tokens > window` on every
  non-empty request, and summed-gate backends reject it (live-proven on the
  qwen3.8-27b-onprem hop: 262144 out → 400); (b) the llaminar#94 empty-200
  class is real but its *mechanism* is client-side — the failing requests
  carried client-computed `max_tokens ≈ 1` (a client-estimate collapse, not
  a backend one; Zed sends static caps so it cannot produce that shape).
  Generated caps are therefore always strictly below the window (degenerate
  one-token windows aside — the code reserves nothing at `max_tokens <= 1`,
  emitting the unusable window verbatim).
- **Generated caps are NOT fallback-chain-aware — hand-tune for that.**
  The sync cannot see backend enforcement shapes (fleet operating knowledge
  in llaminar's `docs/post-boot-registration.md`, not card fields): on-prem
  GLM rigs enforce a `prompt + max_tokens ≤ 1,048,576` sum gate, the Baseten
  twins enforce `max_tokens ≤ 262144` standalone (no sum check), and the
  qwen twin `qwen3.8-27b-onprem` enforces a strict 262,144 total
  (live-verified 2026-10-16: 262144 out → 400, 24576 → 200). Zed's
  auto-compaction boundary is `90% × (window − max_output_tokens)` — the
  output cap is reserved *separately* from the compaction threshold
  (Zed `crates/agent/src/thread.rs`, `compaction_input_capacity`; test
  pins 0.9×(1M−32k)=871,200). With the tool's 65536 default on a 262144
  window: threshold ≈ 176.9k, so a hopped request ≈ 242k ≤ 262,144 — it
  *fits* under compaction-governed flows. The residual exposure is
  threshold-overshoot and compaction-off flows, so capping a chained alias
  like `joby-onprem` (hops to `qwen3.8-27b-onprem`) at ~24k remains sound
  defense-in-depth: hand-tune the preserved entry. (The card's
  `fallback_models` field tells you *which* chains to check.)
- **Zed schema notes (verified against Zed `main` 2026-10-16):** the
  `capabilities` block requires `tools`, `images`, `parallel_tool_calls`,
  and `prompt_cache_key` (no serde defaults — blocks omitting them fail to
  load, the exact regression this tool guards against);
  `reasoning_effort` accepts `none|minimal|low|medium|high|xhigh|max`, so
  card-set defaults and manual `xhigh` tuning are legal. Zed's default wire
  parameter for the output cap is `max_completion_tokens` (`max_tokens_parameter: true`
  switches to classic `max_tokens`); the gateway accepts both (LiteLLM
  translates) — the classic pin is defense-in-depth, not a fix for a broken
  default.

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
