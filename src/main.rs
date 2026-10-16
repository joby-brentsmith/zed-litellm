use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Parser;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use jsonc_parser::ParseOptions;
use serde::Deserialize;
use serde_json::{json, Value};

/// Fallback context window for models where LiteLLM doesn't report
/// `max_input_tokens`.
const DEFAULT_MAX_TOKENS: u64 = 128_000;

/// Safe per-response output cap when the LiteLLM card's `max_output_tokens`
/// is missing or unreliable. LiteLLM frequently reports `max_output_tokens`
/// equal to the context window — on llaminar that's the deliberate card
/// clamp shape, not rot — so a sub-window cap is emitted instead, always
/// strictly below the context window (see to_zed_models) so prompt-full
/// requests can never trip the gateway/sum gates at the window edge.
const DEFAULT_OUTPUT_TOKENS: u64 = 65_536;

/// Sync models from a LiteLLM proxy into Zed's settings.
///
/// Queries the proxy's `/model/info` endpoint and updates the
/// `language_models.openai_compatible.<provider>` block in Zed's
/// settings.json, preserving comments and formatting. Dry-run by
/// default; pass --write to update the file in place.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Base URL of the LiteLLM proxy (e.g. http://localhost:4000)
    #[arg(long, env = "LITELLM_URL")]
    url: String,

    /// API key for the proxy. /model/info is authenticated but not
    /// access-gated — the key's scope (team/model list) determines which
    /// cards are visible, so pick a key whose scope covers the models you
    /// want in your picker.
    #[arg(long, env = "LITELLM_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Provider id to write under language_models.openai_compatible
    #[arg(long, default_value = "litellm")]
    provider: String,

    /// Path to Zed's settings.json
    #[arg(long, default_value_os_t = default_settings_path())]
    settings: PathBuf,

    /// Update the settings file in place (a .bak backup is written first).
    /// Without this flag the merged file is printed to stdout.
    #[arg(long)]
    write: bool,

    /// Replace all existing available_models entries with freshly generated
    /// ones. By default, existing entries are preserved verbatim (keeping any
    /// manual tuning like display_name or interleaved_reasoning) and only new
    /// models are appended.
    #[arg(long)]
    replace: bool,

    /// Probe each reasoning-capable model by sending a minimal chat-completions
    /// request with `reasoning_content` on a prior assistant turn. Sets
    /// `capabilities.interleaved_reasoning` to true only when the model both
    /// accepts the field and emits `reasoning_content` in its own response —
    /// distinguishing real support from silent accept-and-ignore.
    #[arg(long)]
    probe: bool,

    /// reasoning_effort assigned to models that report supports_reasoning.
    /// A card-set `default_reasoning_effort` (agreed across deployments)
    /// wins over this — the operator-side per-rig remedy (no live card sets
    /// it today, 2026-10-16). Default `high` is the right fleet call: on GLM
    /// rigs `high` is the documented faithful tier (~22% of default burn —
    /// and since the gateway-side ladder hook deployed (llaminar #114, GO
    /// 2026-10-13), a GLM `medium` gets translated to `high` anyway); on
    /// qwen3.8 rigs the tier map is different (xhigh/medium/low — no
    /// `high`), but their cards bake thinking off so the parameter is
    /// inert today. If ops ever flips qwen thinking on card-side, they must
    /// also set `default_reasoning_effort` on those cards — a Zed-sent
    /// `high` would 400 there (llaminar docs/reasoning-effort.md #113).
    #[arg(long, default_value = "high")]
    reasoning_effort: String,

    /// Send a 1-token chat completion to each discovered chat model and report
    /// which ones are live vs returning an HTTP error or timing out. Useful
    /// after a fleet cutover/rollback to catch stale aliases pointing at dead
    /// backends before they land in your Zed model picker. Report-only — dead
    /// models are still synced (you decide whether to remove them).
    #[arg(long)]
    check_liveness: bool,

    /// Skip discovered models whose name contains any of these substrings
    /// (repeatable). Only applies to newly discovered entries; preserved
    /// existing entries are never filtered. On llaminar, use this for
    /// internal capacity a broad (admin) key can see but that shouldn't land
    /// in the picker: --exclude baseten --exclude auto --exclude upg-test.
    #[arg(long = "exclude", value_name = "SUBSTRING")]
    excludes: Vec<String>,

    /// Full sync: shorthand for --write --replace --probe --check-liveness.
    /// Regenerates every model entry from /model/info, probes reasoning models
    /// for interleaved_reasoning, checks liveness, and writes the result to
    /// your Zed settings. This is the "make my Zed config accurate" one-shot.
    #[arg(short = 'f', long = "full")]
    full: bool,
}

fn default_settings_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/zed/settings.json")
}

#[derive(Deserialize)]
struct ModelInfoResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize, Clone)]
struct ModelEntry {
    model_name: Option<String>,
    model_info: Option<ModelInfo>,
}

#[derive(Deserialize, Default, Clone)]
struct ModelInfo {
    mode: Option<String>,
    max_input_tokens: Option<f64>,
    max_tokens: Option<f64>,
    max_output_tokens: Option<f64>,
    supports_function_calling: Option<bool>,
    supports_parallel_function_calling: Option<bool>,
    supports_vision: Option<bool>,
    supports_prompt_caching: Option<bool>,
    supports_reasoning: Option<bool>,
    default_reasoning_effort: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // -f / --full is shorthand for --write --replace --probe --check-liveness.
    let (write, replace, probe, do_liveness) = if args.full {
        (true, true, true, true)
    } else {
        (args.write, args.replace, args.probe, args.check_liveness)
    };

    let base_url = normalize_base_url(&args.url);
    let api_url = format!("{base_url}/v1");

    let raw_entries = fetch_model_info(&base_url, args.api_key.as_deref())?;
    if raw_entries.is_empty() {
        eprintln!("note: the gateway returned zero cards. /model/info is not");
        eprintln!("  access-gated — it returns the cards in your key's scope, so this");
        eprintln!("  usually means the key has no team/model list covering any models");
        eprintln!("  (client symptom: placeholder token limits). Mint a team-scoped");
        eprintln!("  key in the gateway key UI and retry.");
        eprintln!();
    }
    // /model/info reports one entry per deployment; a public model name can
    // fan out to several (llaminar wildcard aliases). Merge same-name entries
    // into one conservative card — 3-valued AND on capabilities, min across
    // reporters for token limits — per the semantics established by
    // joby-software/vscode-llaminar#61 (PR #63).
    let entries = merge_deployments(&raw_entries);

    // The --exclude patterns decide which models land in the picker; they
    // must also gate the probe/liveness candidates. Excluded models are
    // commonly excluded because their backends are unreachable — probing
    // them is wasted latency (up to the 10s liveness timeout each) on
    // models that will never be emitted.
    let probeable: Vec<ModelEntry> = entries
        .iter()
        .filter(|entry| !is_excluded(entry.model_name.as_deref().unwrap_or(""), &args.excludes))
        .cloned()
        .collect();

    // Warn about null capability fields on the gateway cards — the recurring
    // bug class where sbatch register payloads omit the fields so /model/info
    // returns null and the sync quietly writes wrong defaults.
    let null_warnings = warn_null_fields(&entries);
    if !null_warnings.is_empty() {
        eprintln!(
            "warning: {} chat model(s) have null capability fields on the gateway;",
            null_warnings.len()
        );
        eprintln!("  the sync will fall back to defaults that may be wrong. Fix the");
        eprintln!("  LiteLLM model cards (not the Zed settings) to resolve:");
        for w in &null_warnings {
            eprintln!("{w}");
        }
        eprintln!();
    }

    let interleaved = if probe {
        eprintln!("Probing reasoning models for interleaved_reasoning support...");
        probe_interleaved_reasoning(
            &api_url,
            &probeable,
            &args.reasoning_effort,
            args.api_key.as_deref(),
        )
    } else {
        Vec::new()
    };
    let mut discovered = to_zed_models(&entries, &interleaved, &args.reasoning_effort);
    let excluded = exclude_discovered(&mut discovered, &args.excludes);
    if excluded > 0 {
        eprintln!("Excluded {excluded} discovered model(s) matching the --exclude patterns");
    }

    // Read the settings before deciding whether to bail: when preserving
    // existing entries, an all-excluded (or empty) discovery is not fatal
    // — the sync becomes a no-op that keeps the preserved entries. Only a
    // true zero-model outcome bails.
    let settings_text = match fs::read_to_string(&args.settings) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", args.settings.display()));
        }
    };
    if discovered.is_empty() {
        let excluded_note = if excluded > 0 {
            " (all discovered models matched the --exclude patterns)"
        } else {
            ""
        };
        if !replace && !existing_models(&settings_text, &args.provider).is_empty() {
            eprintln!(
                "No chat models to sync from {base_url}/model/info{excluded_note}; preserving existing entries in {}",
                args.settings.display()
            );
            // A true no-op: falling through would still write the file, and
            // merge_into_settings unconditionally re-points the provider's
            // api_url — so an empty discovery with a different --url would
            // silently repoint every preserved model at a gateway that
            // returned no cards. Return without touching anything.
            return Ok(());
        } else {
            bail!("no chat models to sync from {base_url}/model/info{excluded_note}");
        }
    }

    // Liveness check: probe each chat model with a 1-token completion to catch
    // dead backends / stale aliases before they land in the Zed picker.
    // Report-only — dead models are still synced.
    if do_liveness {
        eprintln!("Checking liveness of chat models...");
        let results = check_liveness(&api_url, &probeable, args.api_key.as_deref());
        let dead: Vec<_> = results.iter().filter(|r| !r.live).collect();
        if dead.is_empty() {
            eprintln!("All {} chat model(s) are live.", results.len());
        } else {
            eprintln!(
                "{} of {} chat model(s) NOT LIVE:",
                dead.len(),
                results.len()
            );
            for r in &dead {
                eprintln!("  {}: {}", r.model, r.detail);
            }
        }
        eprintln!();
    }

    // Reload the settings before merging: the probe/liveness passes above
    // can run for minutes, and Zed saves this file on its own triggers —
    // merging into the stale pre-probe snapshot would clobber edits saved
    // in the meantime, including unrelated keys (the whole file is rewritten
    // from this snapshot). The pre-check read above remains only for the
    // empty-discovery decision.
    let settings_text = match fs::read_to_string(&args.settings) {
        Ok(text) => text,
        // File gone since the early read (e.g. deleted while the probe /
        // liveness passes ran): start from an empty document, as the
        // original single-read path did — writing back the stale snapshot
        // would recreate a file the user deleted, with settings they
        // removed.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", args.settings.display()));
        }
    };

    let models = if replace {
        eprintln!(
            "Discovered {} chat model(s) from {base_url} (replacing existing entries)",
            discovered.len()
        );
        discovered
    } else {
        let existing = existing_models(&settings_text, &args.provider);
        let merged = merge_models(existing, discovered);
        eprintln!(
            "Discovered {} chat model(s) from {base_url}: {} existing entries preserved, {} new",
            merged.discovered_count, merged.existing_count, merged.new_count
        );
        merged.models
    };

    let merged = merge_into_settings(&settings_text, &args.provider, &api_url, models)?;

    if write {
        let backup = args.settings.with_extension("json.bak");
        if args.settings.exists() {
            fs::copy(&args.settings, &backup)
                .with_context(|| format!("backing up to {}", backup.display()))?;
        }
        fs::write(&args.settings, merged)
            .with_context(|| format!("writing {}", args.settings.display()))?;
        eprintln!(
            "Updated {} (backup at {})",
            args.settings.display(),
            backup.display()
        );
    } else {
        println!("{merged}");
        eprintln!(
            "(dry run: pass --write to update {})",
            args.settings.display()
        );
    }

    Ok(())
}

/// Strips a trailing `/` and `/v1` so the tool accepts either the proxy root
/// or the OpenAI-style base URL users already have in their Zed settings.
fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

fn fetch_model_info(base_url: &str, api_key: Option<&str>) -> Result<Vec<ModelEntry>> {
    let url = format!("{base_url}/model/info");
    let mut request = ureq::get(&url).set("Accept", "application/json");
    if let Some(api_key) = api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, _)) if status == 401 || status == 403 => {
            // /model/info itself has no per-key access gate — a key whose
            // scope lacks cards gets a 200 with fewer/zero entries, so a
            // 401/403 here is authentication (invalid/expired key) or a
            // route-level rejection, not missing /model/info "permission".
            let key_note = if api_key.is_some() {
                "the gateway rejected the key (invalid or expired)"
            } else {
                "no API key was provided — /model/info requires authentication"
            };
            bail!("requesting {url}: HTTP {status} — {key_note}. Note: /model/info is not access-gated beyond authentication; a key whose scope has no cards gets an empty 200, not an error.");
        }
        Err(error) => return Err(error).with_context(|| format!("requesting {url}")),
    };
    let parsed: ModelInfoResponse = response
        .into_json()
        .with_context(|| format!("parsing response from {url}"))?;
    Ok(parsed.data)
}

/// Merges per-deployment `/model/info` entries that share a `model_name`
/// into one conservative card. The gateway reports one entry per deployment
/// and a public model name can fan out to several; first-wins dedupe would
/// let entry ordering decide capabilities, so instead (the semantics
/// established for the same endpoint in joby-software/vscode-llaminar#61 / PR
/// #63):
///   - capability booleans: any-false → false, all-true → true, else null
///     (unknown — null warnings fire downstream)
///   - token limits: min across the deployments that report them
///   - mode: the common mode when all entries agree; a mix that includes a
///     non-chat mode reports that non-chat mode (the group is excluded as
///     non-chat downstream); a chat/responses mix reports
///     "mixed-chat-responses" and is excluded too (no single API serves
///     every member of the group). Unreported (null) modes count as chat
///     here — exactly as downstream treats them — so a [null, "responses"]
///     group is excluded as mixed rather than passing as Responses-only;
///     a group where nothing reports a mode stays null (warns downstream)
///   - default_reasoning_effort: honored only when every deployment sets
///     the same value; any disagreement or absence falls back to the CLI
///     value (one backend's curated default is not stretched onto a
///     sibling deployment across a failover hop)
fn merge_deployments(entries: &[ModelEntry]) -> Vec<ModelEntry> {
    let mut by_name: std::collections::BTreeMap<String, Vec<&ModelEntry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        // Nameless entries have no stable identity to merge under and are
        // discarded (same policy as to_zed_models).
        if let Some(name) = entry.model_name.as_deref() {
            by_name.entry(name.to_string()).or_default().push(entry);
        }
    }
    by_name
        .into_iter()
        .map(|(name, group)| ModelEntry {
            model_name: Some(name),
            model_info: Some(merge_infos(&group)),
        })
        .collect()
}

fn merge_infos(group: &[&ModelEntry]) -> ModelInfo {
    let infos: Vec<ModelInfo> = group
        .iter()
        .map(|e| e.model_info.clone().unwrap_or_default())
        .collect();

    fn and(values: Vec<Option<bool>>) -> Option<bool> {
        if values.iter().any(|value| *value == Some(false)) {
            Some(false)
        } else if values.iter().all(|value| *value == Some(true)) {
            Some(true)
        } else {
            None
        }
    }

    fn min_of(values: Vec<Option<f64>>) -> Option<f64> {
        // Non-positive caps are unusable metadata (a null-card sibling of the
        // sbatch omission class), not real limits — dropping them before the
        // min matches the emitter's positive-cap check. Taking them at face
        // value would merge [8192, 0] to 0, which the emitter rejects via
        // the default 65536 fallback — silently exceeding the honest 8192 cap.
        values
            .into_iter()
            .flatten()
            .filter(|v| *v > 0.0)
            .reduce(f64::min)
    }

    fn agree(values: Vec<Option<String>>) -> Option<String> {
        // Unanimity requires completeness: an abstaining deployment (null)
        // means the card default is NOT honored for the merged alias — the
        // CLI fallback applies instead, so one backend's curated default is
        // not stretched onto a sibling deployment across a failover hop.
        let all: Vec<String> = values.into_iter().collect::<Option<Vec<_>>>()?;
        let Some(first) = all.first() else {
            return None;
        };
        all.iter().all(|v| v == first).then(|| first.clone())
    }

    let mode = {
        // A null (unreported) mode counts as chat downstream — to_zed_models
        // includes null-mode cards as chat — so fold it that way here too,
        // or a [null, "responses"] group would pass as Responses-only and
        // emit chat_completions: false for a deployment that is really
        // chat-capable-but-unlabeled (the recurring sbatch null-metadata
        // class flagged by the null-field warnings).
        let mut effective: Vec<&str> = infos
            .iter()
            .map(|info| info.mode.as_deref().unwrap_or("chat"))
            .collect();
        effective.sort_unstable();
        effective.dedup();
        // No deployment reports a mode at all: stays unknown — the
        // null-mode warning fires downstream.
        let no_mode_reported = infos.iter().all(|info| info.mode.is_none());
        match effective.as_slice() {
            [] => None,
            _ if no_mode_reported => None,
            [single] => Some((*single).to_string()),
            many => match many
                .iter()
                .copied()
                .find(|m| *m != "chat" && *m != "responses")
            {
                // A group mixing in a non-chat mode (e.g. embedding) is
                // reported as non-chat so it's excluded downstream.
                Some(non_chat) => Some(non_chat.to_string()),
                // A chat/responses mix has no mode every member can serve:
                // a Responses-only deployment fails /chat/completions, so
                // the merged alias must not advertise chat (the AND-cap
                // rule applied to the group end to end). Report a
                // non-chat/responses mode so downstream excludes it.
                None => {
                    if let Some(name) = group.first().and_then(|e| e.model_name.clone()) {
                        eprintln!(
                            "warning: {name} mixes chat and Responses deployments; \
                             excluded from the sync (no single API serves every member)"
                        );
                    }
                    Some("mixed-chat-responses".to_string())
                }
            },
        }
    };

    ModelInfo {
        mode,
        // Fold the per-entry window (max_input_tokens, falling back to
        // max_tokens) into one min-across-reporters max_input_tokens.
        max_input_tokens: min_of(
            infos
                .iter()
                .map(|info| info.max_input_tokens.or(info.max_tokens))
                .collect(),
        ),
        max_tokens: None,
        max_output_tokens: min_of(infos.iter().map(|info| info.max_output_tokens).collect()),
        supports_function_calling: and(infos
            .iter()
            .map(|info| info.supports_function_calling)
            .collect()),
        supports_parallel_function_calling: and(infos
            .iter()
            .map(|info| info.supports_parallel_function_calling)
            .collect()),
        supports_vision: and(infos.iter().map(|info| info.supports_vision).collect()),
        supports_prompt_caching: and(infos
            .iter()
            .map(|info| info.supports_prompt_caching)
            .collect()),
        supports_reasoning: and(infos.iter().map(|info| info.supports_reasoning).collect()),
        default_reasoning_effort: agree(
            infos
                .iter()
                .map(|info| info.default_reasoning_effort.clone())
                .collect(),
        ),
    }
}

/// Probes reasoning-capable models for `interleaved_reasoning` support by
/// sending a minimal chat-completions request with `reasoning_content` set on
/// a prior assistant turn. A model is reported as supporting interleaved
/// reasoning only when it (a) returns 200 (accepts the field) and (b) emits
/// `reasoning_content` in its own response message (uses the field) — this
/// distinguishes real support from silent accept-and-ignore.
///
/// Failures (network errors, parse errors, non-200) for a given model are
/// logged and the model is reported as not supporting interleaved reasoning;
/// other models are still probed.
fn probe_interleaved_reasoning(
    api_url: &str,
    entries: &[ModelEntry],
    cli_reasoning_effort: &str,
    api_key: Option<&str>,
) -> Vec<String> {
    let mut supported = Vec::new();
    let candidates: Vec<(&str, String)> = entries
        .iter()
        .filter_map(|entry| {
            let info = entry.model_info.as_ref()?;
            if info.supports_reasoning == Some(true) {
                // Probe the effort the sync will actually emit — the card
                // default when set, else the CLI fallback — so a model that
                // accepts the emitted tier but rejects `high` is not
                // spuriously marked not-supporting (and vice versa).
                let effort = info
                    .default_reasoning_effort
                    .clone()
                    .unwrap_or_else(|| cli_reasoning_effort.to_string());
                entry.model_name.as_deref().map(|n| (n, effort))
            } else {
                None
            }
        })
        .collect();

    for (model_name, effort) in candidates {
        match probe_one_model(api_url, model_name, &effort, api_key) {
            Ok(true) => {
                eprintln!("  {model_name}: interleaved_reasoning supported");
                supported.push(model_name.to_string());
            }
            Ok(false) => {
                eprintln!("  {model_name}: no (accepts but does not emit reasoning_content)");
            }
            Err(error) => {
                eprintln!("  {model_name}: probe failed ({error:#}) — leaving off");
            }
        }
    }
    supported
}

fn probe_one_model(
    api_url: &str,
    model_name: &str,
    reasoning_effort: &str,
    api_key: Option<&str>,
) -> Result<bool> {
    let url = format!("{api_url}/chat/completions");
    // Probing the same `reasoning_effort` the generated entry will send: on
    // the always-on GLM family reasoning fires regardless of effort
    // strings; on qwen3.8 the card default is thinking-off, making the
    // parameter inert. `max_tokens: 1` keeps cost near zero, and on
    // reasoning models the single completion token lands in
    // reasoning_content (the signal we need) with no real output required.
    let payload = json!({
        "model": model_name,
        "max_tokens": 1,
        "reasoning_effort": reasoning_effort,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "reasoning_content": "prior thinking", "content": "ok"},
            {"role": "user", "content": "hi again"},
        ],
    });
    let body = serde_json::to_string(&payload)?;

    let mut request = ureq::post(&url)
        .timeout(Duration::from_secs(30))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    if let Some(api_key) = api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    let response = request
        .send_string(&body)
        .with_context(|| format!("probing {model_name} at {url}"))?;

    let parsed: ProbeResponse = response.into_json().context("parsing probe response")?;
    let message = parsed
        .choices
        .first()
        .and_then(|choice| choice.message.as_ref())
        .ok_or_else(|| anyhow!("probe response had no message"))?;
    // The real signal: the model emitted reasoning_content of its own.
    // A server that silently drops the input field would not generate this.
    Ok(message.reasoning_content.is_some())
}

/// Check a single chat model for liveness by sending a 1-token completion.
/// Returns Ok(()) if the model responded 2xx, or Err with the HTTP status /
/// network error detail.
fn check_one_model_liveness(api_url: &str, model_name: &str, api_key: Option<&str>) -> Result<()> {
    let url = format!("{api_url}/chat/completions");
    let payload = json!({
        "model": model_name,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ok"}],
    });
    let body = serde_json::to_string(&payload)?;
    let mut request = ureq::post(&url)
        .timeout(Duration::from_secs(10))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    if let Some(api_key) = api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    request
        .send_string(&body)
        .with_context(|| format!("liveness probe for {model_name}"))?;
    Ok(())
}

/// Result of a liveness check for one model.
struct LivenessResult {
    model: String,
    live: bool,
    detail: String,
}

/// Probe each chat model with a 1-token completion and report liveness.
/// A model is "live" if the gateway returned 2xx (the backend answered,
/// however briefly). Non-2xx (e.g. 403 from an upstream rate-limit, 500 from a
/// dead backend) or network errors are reported as not live with the detail.
/// Models are NOT removed from the sync — this is report-only so the user
/// can decide whether to remove dead entries from their settings.
fn check_liveness(
    api_url: &str,
    entries: &[ModelEntry],
    api_key: Option<&str>,
) -> Vec<LivenessResult> {
    let candidates: Vec<&str> = entries
        .iter()
        .filter_map(|entry| {
            let Some(info) = entry.model_info.as_ref() else {
                return entry.model_name.as_deref();
            };
            let mode = info.mode.as_deref();
            // Only probe chat-mode (or null-mode) models — responses-mode
            // models are Responses-API-only (chat_completions: false) and
            // can't be probed via /chat/completions (would be a false
            // negative). Non-chat modes (embedding, image_generation, etc.)
            // are also excluded.
            if let Some(m) = mode {
                if m != "chat" {
                    return None;
                }
            }
            entry.model_name.as_deref()
        })
        // Dedup by model name — /model/info can return multiple deployments
        // for the same model_name, and probing each is redundant.
        .collect();
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<&str> = candidates
        .into_iter()
        .filter(|name| seen.insert(*name))
        .collect();

    let mut results = Vec::new();
    for model_name in candidates {
        match check_one_model_liveness(api_url, model_name, api_key) {
            Ok(()) => {
                eprintln!("  {model_name}: live");
                results.push(LivenessResult {
                    model: model_name.to_string(),
                    live: true,
                    detail: String::new(),
                });
            }
            Err(error) => {
                let detail = format!("{error:#}");
                eprintln!("  {model_name}: NOT LIVE — {detail}");
                results.push(LivenessResult {
                    model: model_name.to_string(),
                    live: false,
                    detail,
                });
            }
        }
    }
    results
}

/// Check chat-model entries for null capability fields and return warning lines.
///
/// Null fields cause the sync to fall back to defaults that may be wrong:
///   - `supports_vision: null` → `false` (vision silently disabled)
///   - `supports_reasoning: null` → `reasoning_effort` omitted (capability loss)
///   - `supports_function_calling: null` → `true` (may be wrong for vision-only)
///
/// This is the recurring bug class where sbatch register payloads omit the
/// capability fields, so `/model/info` returns null, and the sync quietly
/// writes wrong defaults. The warning surfaces the gap so the operator knows
/// the gateway cards need fixing (not the Zed settings).
///
/// Only chat models (mode = "chat", "responses", or null) are checked —
/// non-chat modes (embedding, image_generation, etc.) legitimately lack
/// these fields.
fn warn_null_fields(entries: &[ModelEntry]) -> Vec<String> {
    let mut warnings = Vec::new();
    for entry in entries {
        let Some(name) = entry.model_name.as_deref() else {
            continue;
        };
        let Some(info) = entry.model_info.as_ref() else {
            continue;
        };
        let mode = info.mode.as_deref();
        if let Some(m) = mode {
            if m != "chat" && m != "responses" {
                continue;
            }
        }
        let mut gaps: Vec<&str> = Vec::new();
        if info.supports_function_calling.is_none() {
            gaps.push("supports_function_calling (defaults to true — may be wrong)");
        }
        if info.supports_vision.is_none() {
            gaps.push("supports_vision (defaults to false — vision silently disabled)");
        }
        if info.supports_parallel_function_calling.is_none() {
            gaps.push("supports_parallel_function_calling (defaults to false)");
        }
        if info.supports_reasoning.is_none() {
            gaps.push("supports_reasoning (reasoning_effort omitted — capability loss)");
        }
        if info.supports_prompt_caching.is_none() {
            gaps.push(
                "supports_prompt_caching (prompt_cache_key defaults to false — cache misses)",
            );
        }
        if info.max_input_tokens.is_none() && info.max_tokens.is_none() {
            gaps.push("max_input_tokens (defaults to 128000)");
        }
        if info.mode.is_none() {
            gaps.push("mode (null — included as chat, but strict clients may skip)");
        }
        if !gaps.is_empty() {
            warnings.push(format!("  {name}: {}", gaps.join("; ")));
        }
    }
    warnings
}

#[derive(Deserialize)]
struct ProbeResponse {
    choices: Vec<ProbeChoice>,
}

#[derive(Deserialize)]
struct ProbeChoice {
    message: Option<ProbeMessage>,
}

#[derive(Deserialize)]
struct ProbeMessage {
    reasoning_content: Option<serde_json::Value>,
}

/// Maps LiteLLM `/model/info` entries to Zed `openai_compatible`
/// `available_models` values. Non-chat models (embeddings, image generation,
/// ...) are skipped, and duplicate deployments of the same model name are
/// collapsed into one entry. `interleaved` lists model names that have been
/// verified (via probe) to both accept `reasoning_content` in input and emit
/// it in output — those get `interleaved_reasoning: true`.
fn to_zed_models(
    entries: &[ModelEntry],
    interleaved: &[String],
    reasoning_effort: &str,
) -> Vec<Value> {
    let mut by_name = std::collections::BTreeMap::new();

    for entry in entries {
        let Some(name) = entry.model_name.as_deref() else {
            continue;
        };
        let info = entry.model_info.as_ref();
        let mode = info.and_then(|info| info.mode.as_deref());
        if let Some(mode) = mode {
            if mode != "chat" && mode != "responses" {
                continue;
            }
        }
        if by_name.contains_key(name) {
            continue;
        }

        let info = info.cloned().unwrap_or_default();
        let interleaved_support = interleaved
            .iter()
            .any(|name| Some(name.as_str()) == entry.model_name.as_deref());
        let is_responses_mode = mode == Some("responses");
        let mut model = serde_json::Map::new();
        model.insert("name".into(), json!(name));

        let max_tokens = info
            .max_input_tokens
            .or(info.max_tokens)
            .map(|tokens| tokens as u64)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        model.insert("max_tokens".into(), json!(max_tokens));

        // LiteLLM's `max_output_tokens` card value can't be used directly as
        // the Zed side setting: llaminar deliberately clamps glm-family
        // cards to 262144/262144 (the 2026-09-22 session, mirroring the
        // Baseten twins' standalone enforcement), and other LiteLLM
        // deployments report the same out==in shape through card rot.
        // Either way an out==window cap in Zed guarantees prompt+out >
        // window on every non-empty request — a guaranteed 400 on
        // sum-gate-checked routes (live-proven on the qwen3.8-27b-onprem
        // hop). Emit a sane cap instead: honor the card value only when
        // it's a deliberate sub-window cap; otherwise fall back to
        // DEFAULT_OUTPUT_TOKENS, strictly below the context window.
        let mut max_output_tokens = match info.max_output_tokens {
            Some(cap) if (cap as u64) > 0 && (cap as u64) < max_tokens => cap as u64,
            _ => DEFAULT_OUTPUT_TOKENS.min(max_tokens),
        };
        // Never emit max_output_tokens >= the context window: a request
        // capped at the advertised window leaves no room for input and is
        // guaranteed to 400 on summed-gate backends (qwen3.8-27b-onprem:
        // 262144 out -> 400, 24576 -> 200, live-proven 2026-10-16). For
        // degenerate windows <= 1 there is nothing to reserve.
        if max_output_tokens >= max_tokens && max_tokens > 1 {
            let reserve = (max_tokens / 8).max(4_096).min(max_tokens / 2);
            max_output_tokens = max_tokens - reserve;
        }
        model.insert("max_output_tokens".into(), json!(max_output_tokens));

        if info.supports_reasoning == Some(true) {
            // A card-set default_reasoning_effort (operator-curated, agreed
            // across deployments) wins over the CLI value; the CLI value is
            // the fallback.
            let effort = info
                .default_reasoning_effort
                .clone()
                .unwrap_or_else(|| reasoning_effort.to_string());
            model.insert("reasoning_effort".into(), json!(effort));
        }
        let mut caps = serde_json::Map::new();
        caps.insert(
            "tools".into(),
            json!(info.supports_function_calling.unwrap_or(true)),
        );
        caps.insert(
            "images".into(),
            json!(info.supports_vision.unwrap_or(false)),
        );
        caps.insert(
            "parallel_tool_calls".into(),
            json!(info.supports_parallel_function_calling.unwrap_or(false)),
        );
        // Required by Zed's capabilities schema whenever `capabilities`
        // is present. LiteLLM reports `supports_prompt_caching`, so map
        // it directly rather than guessing `false`.
        caps.insert(
            "prompt_cache_key".into(),
            json!(info.supports_prompt_caching.unwrap_or(false)),
        );
        // True only when verified by --probe: the model accepts
        // `reasoning_content` in input AND emits it in output. Defaults
        // to false because LiteLLM's /model/info cannot report this and
        // a blind `true` degrades multi-turn reasoning on endpoints
        // that silently ignore the field.
        caps.insert("interleaved_reasoning".into(), json!(interleaved_support));
        // Zed's `openai_compatible` provider defaults to sending the output
        // cap as `max_completion_tokens` on the wire; classic `max_tokens` is
        // the one every backend on this fleet accepts (SGLang, vLLM, the
        // Baseten twins), so pin the classic shape explicitly. Unknown to
        // older Zed, which just ignores the field.
        caps.insert("max_tokens_parameter".into(), json!(true));
        // A `responses`-mode model only works with the Responses API.
        // Zed defaults `chat_completions` to true, so we must explicitly
        // set it to false to avoid routing to /chat/completions.
        if is_responses_mode {
            caps.insert("chat_completions".into(), json!(false));
        }
        model.insert("capabilities".into(), Value::Object(caps));

        by_name.insert(name.to_string(), Value::Object(model));
    }

    by_name.into_values().collect()
}

/// Removes discovered models whose name contains any of the given
/// substrings. Returns how many were removed. Substring (not regex, not
/// exact) by design — the internal-capacity tells are prefixes/suffixes
/// (`baseten/...`, `...-baseten`) plus the bare `auto` router alias — and a
/// typo'd regex silently filtering a picker is worse than a blunt match.
/// NOTE: substring means `--exclude auto` also drops any name *containing*
/// "auto"; run a dry run to check what matches before `--write`.
fn exclude_discovered(models: &mut Vec<Value>, patterns: &[String]) -> usize {
    if patterns.is_empty() {
        return 0;
    }
    let before = models.len();
    models.retain(|model| {
        let name = model.get("name").and_then(|n| n.as_str()).unwrap_or("");
        !is_excluded(name, patterns)
    });
    before - models.len()
}
/// True when `name` contains any of the `--exclude` patterns. See
/// `exclude_discovered` for why this is substring (not regex, not exact).
fn is_excluded(name: &str, patterns: &[String]) -> bool {
    !patterns.is_empty() && patterns.iter().any(|pattern| name.contains(pattern))
}

/// Reads the provider's current `available_models` entries from the settings
/// text. Returns an empty list when the provider (or any parent key) is
/// missing or the settings can't be parsed.
fn existing_models(settings_text: &str, provider: &str) -> Vec<Value> {
    jsonc_parser::parse_to_serde_value(settings_text, &ParseOptions::default())
        .ok()
        .flatten()
        .and_then(|settings| {
            settings
                .get("language_models")?
                .get("openai_compatible")?
                .get(provider)?
                .get("available_models")?
                .as_array()
                .cloned()
        })
        .unwrap_or_default()
}

struct MergedModels {
    models: Vec<Value>,
    discovered_count: usize,
    existing_count: usize,
    new_count: usize,
}

/// Existing entries win: they are kept verbatim (preserving manual tuning
/// such as `display_name`, `reasoning_effort`, or capabilities LiteLLM can't
/// report, like `interleaved_reasoning`). Discovered models not already
/// present are appended. Nothing is removed.
fn merge_models(existing: Vec<Value>, discovered: Vec<Value>) -> MergedModels {
    let existing_names: std::collections::HashSet<String> = existing
        .iter()
        .filter_map(|model| Some(model.get("name")?.as_str()?.to_string()))
        .collect();

    let discovered_count = discovered.len();
    let existing_count = existing.len();
    let mut models = existing;
    let mut new_count = 0;
    for model in discovered {
        let is_new = model
            .get("name")
            .and_then(|name| name.as_str())
            .is_some_and(|name| !existing_names.contains(name));
        if is_new {
            models.push(model);
            new_count += 1;
        }
    }

    MergedModels {
        models,
        discovered_count,
        existing_count,
        new_count,
    }
}

/// Updates `language_models.openai_compatible.<provider>` in the settings
/// text, creating intermediate objects as needed. Only `api_url` and
/// `available_models` are touched; any other provider keys (e.g.
/// `custom_headers`) and all comments/formatting elsewhere are preserved.
fn merge_into_settings(
    settings_text: &str,
    provider: &str,
    api_url: &str,
    models: Vec<Value>,
) -> Result<String> {
    let root = CstRootNode::parse(settings_text, &ParseOptions::default())
        .map_err(|error| anyhow!("parsing settings file: {error}"))?;
    let root_obj = root.object_value_or_set();
    let language_models = root_obj.object_value_or_set("language_models");
    let openai_compatible = language_models.object_value_or_set("openai_compatible");
    let provider_obj = openai_compatible.object_value_or_set(provider);

    set_prop(&provider_obj, "api_url", json!(api_url));
    set_prop(&provider_obj, "available_models", Value::Array(models));

    Ok(root.to_string())
}

fn set_prop(object: &CstObject, name: &str, value: Value) {
    let value = to_cst_value(value);
    match object.get(name) {
        Some(prop) => {
            prop.set_value(value);
        }
        None => {
            object.append(name, value);
        }
    }
}

fn to_cst_value(value: Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value),
        Value::Array(values) => {
            CstInputValue::Array(values.into_iter().map(to_cst_value).collect())
        }
        Value::Object(entries) => CstInputValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, to_cst_value(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, info: ModelInfo) -> ModelEntry {
        ModelEntry {
            model_name: Some(name.to_string()),
            model_info: Some(info),
        }
    }

    #[test]
    fn exclude_discovered_filters_substring_matches() {
        let mut models = vec![
            json!({"name": "glm-5.3"}),
            json!({"name": "baseten/muse-spark-open"}),
            json!({"name": "glm-5.3-baseten"}),
            json!({"name": "auto"}),
        ];
        let excludes = vec!["baseten".to_string(), "auto".to_string()];
        let removed = exclude_discovered(&mut models, &excludes);
        assert_eq!(removed, 3);
        assert_eq!(models[0]["name"], json!("glm-5.3"));
        // No patterns -> no filtering.
        assert_eq!(exclude_discovered(&mut models, &[]), 0);
    }

    #[test]
    fn merge_deployments_ands_capabilities_and_mins_limits() {
        // Two deployments of the same public name (a wildcard alias fanning
        // out to per-backend cards). First-wins would keep whichever entry
        // came first; the conservative merge keeps the narrower truth:
        // any-false -> false, all-true -> true, mixed true/null -> null,
        // token limits min across reporters (vscode-llaminar#61 / PR #63).
        let entries = vec![
            entry(
                "glm-5.3-alias",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    max_input_tokens: Some(1_048_576.0),
                    max_output_tokens: Some(1_048_576.0),
                    supports_function_calling: Some(true),
                    supports_vision: Some(true),
                    supports_reasoning: Some(true),
                    ..Default::default()
                },
            ),
            entry(
                "glm-5.3-alias",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    max_input_tokens: Some(262_144.0),
                    max_output_tokens: Some(262_144.0),
                    supports_function_calling: Some(true),
                    supports_vision: Some(false), // spillover twin: no vision
                    supports_reasoning: None,
                    ..Default::default()
                },
            ),
        ];

        let merged = merge_deployments(&entries);
        assert_eq!(merged.len(), 1);
        let info = merged[0].model_info.as_ref().unwrap();
        assert_eq!(
            info.max_input_tokens,
            Some(262_144.0),
            "min across reporters"
        );
        assert_eq!(info.max_output_tokens, Some(262_144.0));
        assert_eq!(info.supports_function_calling, Some(true));
        assert_eq!(info.supports_vision, Some(false), "any-false wins");
        assert_eq!(info.supports_reasoning, None, "true + null -> null (warns)");
        assert_eq!(info.mode.as_deref(), Some("chat"));

        // And the emitted entry carries the narrowed limit.
        let models = to_zed_models(&merged, &[], "high");
        assert_eq!(models[0]["max_tokens"], json!(262_144));
    }

    #[test]
    fn merge_deployments_excludes_groups_with_non_chat_modes() {
        let entries = vec![
            entry(
                "mixed-mode-alias",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    ..Default::default()
                },
            ),
            entry(
                "mixed-mode-alias",
                ModelInfo {
                    mode: Some("embedding".to_string()),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        assert_eq!(
            merged[0].model_info.as_ref().unwrap().mode.as_deref(),
            Some("embedding"),
            "a non-chat member makes the whole group non-chat"
        );
        assert!(to_zed_models(&merged, &[], "high").is_empty());
    }

    #[test]
    fn merge_deployments_folds_max_tokens_into_window() {
        // Old-style cards report `max_tokens` instead of `max_input_tokens`;
        // the merge must still take the min window across reporters.
        let entries = vec![
            entry(
                "legacy-card",
                ModelInfo {
                    max_tokens: Some(512_000.0),
                    ..Default::default()
                },
            ),
            entry(
                "legacy-card",
                ModelInfo {
                    max_input_tokens: Some(256_000.0),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        let info = merged[0].model_info.as_ref().unwrap();
        assert_eq!(info.max_input_tokens, Some(256_000.0));
        assert_eq!(info.max_tokens, None, "folded into max_input_tokens");
    }

    #[test]
    fn card_default_reasoning_effort_wins_over_cli_fallback() {
        let entries = vec![entry(
            "glm-5.3",
            ModelInfo {
                supports_reasoning: Some(true),
                default_reasoning_effort: Some("low".to_string()),
                ..Default::default()
            },
        )];

        // Card default wins regardless of the CLI value.
        let models = to_zed_models(&entries, &[], "high");
        assert_eq!(models[0]["reasoning_effort"], json!("low"));

        // Without a card default, the CLI fallback applies.
        let entries = vec![entry(
            "kimi-k3",
            ModelInfo {
                supports_reasoning: Some(true),
                ..Default::default()
            },
        )];
        let models = to_zed_models(&entries, &[], "high");
        assert_eq!(models[0]["reasoning_effort"], json!("high"));
    }

    #[test]
    fn merge_deployments_default_effort_needs_agreement() {
        let entries = vec![
            entry(
                "split-effort",
                ModelInfo {
                    supports_reasoning: Some(true),
                    default_reasoning_effort: Some("low".to_string()),
                    ..Default::default()
                },
            ),
            entry(
                "split-effort",
                ModelInfo {
                    supports_reasoning: Some(true),
                    default_reasoning_effort: Some("high".to_string()),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        // Deployments disagree -> no card default, CLI fallback applies.
        assert_eq!(
            merged[0]
                .model_info
                .as_ref()
                .unwrap()
                .default_reasoning_effort,
            None
        );
        let models = to_zed_models(&merged, &[], "medium");
        assert_eq!(models[0]["reasoning_effort"], json!("medium"));
    }

    #[test]
    fn merge_deployments_default_effort_needs_unanimity() {
        // A deployment with no card default (null) is an abstention: the
        // sibling's curated default must not be stretched onto it across
        // the failover hop (regression for the Copilot PR-15 finding):
        // [Some("low"), None] falls back to the CLI value.
        let entries = vec![
            entry(
                "split-effort",
                ModelInfo {
                    supports_reasoning: Some(true),
                    default_reasoning_effort: Some("low".to_string()),
                    ..Default::default()
                },
            ),
            entry(
                "split-effort",
                ModelInfo {
                    supports_reasoning: Some(true),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        assert_eq!(
            merged[0]
                .model_info
                .as_ref()
                .unwrap()
                .default_reasoning_effort,
            None
        );
        let models = to_zed_models(&merged, &[], "high");
        assert_eq!(models[0]["reasoning_effort"], json!("high"));

        // All deployments set the same default -> honored.
        let entries = vec![
            entry(
                "same-effort",
                ModelInfo {
                    supports_reasoning: Some(true),
                    default_reasoning_effort: Some("low".to_string()),
                    ..Default::default()
                },
            ),
            entry(
                "same-effort",
                ModelInfo {
                    supports_reasoning: Some(true),
                    default_reasoning_effort: Some("low".to_string()),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        let models = to_zed_models(&merged, &[], "high");
        assert_eq!(models[0]["reasoning_effort"], json!("low"));
    }

    #[test]
    fn merge_deployments_excludes_chat_responses_mixed_groups() {
        // A Responses-only deployment cannot serve /chat/completions, so a
        // mixed chat/responses alias must not be advertised as chat
        // (regression for the Copilot PR-15 finding): the group is excluded
        // from the sync instead.
        let entries = vec![
            entry(
                "mixed-api",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    ..Default::default()
                },
            ),
            entry(
                "mixed-api",
                ModelInfo {
                    mode: Some("responses".to_string()),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        assert_eq!(
            merged[0].model_info.as_ref().unwrap().mode.as_deref(),
            Some("mixed-chat-responses")
        );
        let models = to_zed_models(&merged, &[], "high");
        assert!(models.is_empty(), "mixed chat/responses group is excluded");
    }

    #[test]
    fn merge_deployments_treats_null_mode_as_chat_when_mixed() {
        // [null, "responses"]: the null-mode deployment is included as chat
        // downstream, so the group is a mixed-API group — it must be
        // excluded, not emitted as Responses-only (regression for the
        // Copilot PR-15 round-2 finding).
        let entries = vec![
            entry(
                "unlabeled-plus-responses",
                ModelInfo {
                    ..Default::default()
                },
            ),
            entry(
                "unlabeled-plus-responses",
                ModelInfo {
                    mode: Some("responses".to_string()),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        assert_eq!(
            merged[0].model_info.as_ref().unwrap().mode.as_deref(),
            Some("mixed-chat-responses")
        );
        let models = to_zed_models(&merged, &[], "high");
        assert!(models.is_empty(), "null + responses group is excluded");

        // [null, "chat"] folds to plain chat — every member serves chat.
        let entries = vec![
            entry(
                "unlabeled-plus-chat",
                ModelInfo {
                    ..Default::default()
                },
            ),
            entry(
                "unlabeled-plus-chat",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        assert_eq!(
            merged[0].model_info.as_ref().unwrap().mode.as_deref(),
            Some("chat")
        );
        let models = to_zed_models(&merged, &[], "high");
        assert_eq!(models.len(), 1);

        // All-null stays null so the warning fires (no behavior change).
        let entries = vec![
            entry(
                "fully-unlabeled",
                ModelInfo {
                    ..Default::default()
                },
            ),
            entry(
                "fully-unlabeled",
                ModelInfo {
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        assert_eq!(merged[0].model_info.as_ref().unwrap().mode, None);
    }

    #[test]
    fn merge_deployments_ignores_non_positive_limits() {
        // A 0 cap is unusable metadata (a "null" sibling), not a real
        // limit: min([8192, 0]) == 0 would be rejected by the emitter in
        // favor of the 65,536 default — silently exceeding the honest
        // 8192 cap (regression for the Copilot PR-15 round-3 finding).
        let entries = vec![
            entry(
                "zero-capped",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    max_input_tokens: Some(262_144.0),
                    max_output_tokens: Some(8_192.0),
                    ..Default::default()
                },
            ),
            entry(
                "zero-capped",
                ModelInfo {
                    mode: Some("chat".to_string()),
                    max_input_tokens: Some(262_144.0),
                    max_output_tokens: Some(0.0),
                    ..Default::default()
                },
            ),
        ];
        let merged = merge_deployments(&entries);
        let info = merged[0].model_info.as_ref().unwrap();
        assert_eq!(info.max_output_tokens, Some(8_192.0), "0 ignored, not min");
        let models = to_zed_models(&merged, &[], "high");
        assert_eq!(models[0]["max_output_tokens"], json!(8_192));
        assert_eq!(models[0]["max_tokens"], json!(262_144));
    }

    #[test]
    fn maps_model_info_to_zed_model() {
        let entries = vec![entry(
            "gpt-4o",
            ModelInfo {
                mode: Some("chat".to_string()),
                max_input_tokens: Some(128000.0),
                supports_function_calling: Some(true),
                supports_parallel_function_calling: Some(true),
                supports_vision: Some(true),
                supports_prompt_caching: Some(true),
                supports_reasoning: None,
                ..Default::default()
            },
        )];

        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(
            models,
            vec![json!({
                "name": "gpt-4o",
                "max_tokens": 128000,
                "max_output_tokens": 65_536,
                "capabilities": {
                    "tools": true,
                    "images": true,
                    "parallel_tool_calls": true,
                    "prompt_cache_key": true,
                    "interleaved_reasoning": false,
                    "max_tokens_parameter": true,
                }
            })]
        );
    }

    /// Regression guard: Zed's `OpenAiCompatibleModelCapabilities` has no
    /// serde defaults on `tools`, `images`, `parallel_tool_calls`, or
    /// `prompt_cache_key`, so every generated `capabilities` block must
    /// include all four or Zed rejects the settings file (this exact bug
    /// shipped once and broke model loading).
    #[test]
    fn generated_capabilities_include_all_zed_required_fields() {
        let entries = vec![entry("mystery-model", ModelInfo::default())];

        let caps = &to_zed_models(&entries, &[], "medium")[0]["capabilities"];
        assert!(caps.get("tools").is_some(), "missing tools");
        assert!(caps.get("images").is_some(), "missing images");
        assert!(
            caps.get("parallel_tool_calls").is_some(),
            "missing parallel_tool_calls"
        );
        assert!(
            caps.get("prompt_cache_key").is_some(),
            "missing prompt_cache_key"
        );
        assert!(
            caps.get("interleaved_reasoning").is_some(),
            "missing interleaved_reasoning"
        );
        assert!(
            caps.get("max_tokens_parameter").is_some(),
            "missing max_tokens_parameter"
        );
        // A model with no reported capabilities should still get safe defaults.
        assert_eq!(caps["tools"], json!(true));
        assert_eq!(caps["images"], json!(false));
        assert_eq!(caps["parallel_tool_calls"], json!(false));
        assert_eq!(caps["prompt_cache_key"], json!(false));
        assert_eq!(caps["interleaved_reasoning"], json!(false));
        assert_eq!(caps["max_tokens_parameter"], json!(true));
    }

    #[test]
    fn skips_non_chat_models_and_dedupes() {
        let entries = vec![
            entry(
                "text-embedding-3-small",
                ModelInfo {
                    mode: Some("embedding".to_string()),
                    ..Default::default()
                },
            ),
            entry("gpt-4o", ModelInfo::default()),
            entry("gpt-4o", ModelInfo::default()),
        ];

        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"], "gpt-4o");
    }

    #[test]
    fn reasoning_models_get_reasoning_effort() {
        let entries = vec![entry(
            "o3",
            ModelInfo {
                supports_reasoning: Some(true),
                ..Default::default()
            },
        )];

        let models = to_zed_models(&entries, &[], "high");
        assert_eq!(models[0]["reasoning_effort"], "high");
    }

    /// The card's `max_output_tokens` is unreliable (frequently equal to
    /// the context window — on llaminar the deliberate clamp shape, elsewhere
    /// card rot), so the sync must substitute a sane cap rather than
    /// propagate it or omit the field:
    ///   - `out==in` card → DEFAULT_OUTPUT_TOKENS, strictly below the window
    ///   - null card      → same fallback
    ///   - a deliberate sub-window cap → honored
    ///   - the emit is always strictly below the context window (w > 1)
    #[test]
    fn emits_safe_max_output_tokens() {
        // out==in (the glm-5.2-1M / kimi-k3-azure bug): fall back to default.
        let out_eq_in = vec![entry(
            "glm-5.2-1M",
            ModelInfo {
                max_input_tokens: Some(1_048_576.0),
                max_output_tokens: Some(1_048_576.0),
                ..Default::default()
            },
        )];
        let m = to_zed_models(&out_eq_in, &[], "medium");
        assert_eq!(m[0]["max_tokens"], json!(1_048_576));
        assert_eq!(m[0]["max_output_tokens"], json!(65_536));

        // null card: same fallback.
        let null_card = vec![entry(
            "any-model",
            ModelInfo {
                max_input_tokens: Some(200_000.0),
                ..Default::default()
            },
        )];
        let m = to_zed_models(&null_card, &[], "medium");
        assert_eq!(m[0]["max_tokens"], json!(200_000));
        assert_eq!(m[0]["max_output_tokens"], json!(65_536));

        // deliberate sub-window cap: honored.
        let deliberate = vec![entry(
            "capped-model",
            ModelInfo {
                max_input_tokens: Some(262_144.0),
                max_output_tokens: Some(32_768.0),
                ..Default::default()
            },
        )];
        let m = to_zed_models(&deliberate, &[], "medium");
        assert_eq!(m[0]["max_output_tokens"], json!(32_768));

        // default never exceeds a small context window (e.g. qwen3-vl 32768)
        // never EQUALS the window: an out==window cap guarantees a 400 on
        // summed-gate routes (qwen3.8-27b-onprem, live-proven), so a reserve
        // slice is left. The llaminar#94 empty-200 class was client-side
        // (clients sending max_tokens ≈ 1 themselves) — this guard is
        // about the gates.
        let small_ctx = vec![entry(
            "qwen3-vl",
            ModelInfo {
                max_input_tokens: Some(32_768.0),
                ..Default::default()
            },
        )];
        let m = to_zed_models(&small_ctx, &[], "high");
        assert_eq!(m[0]["max_output_tokens"], json!(32_768 - 4_096));

        // Pin the reserve arithmetic against the exact misreading a reviewer
        // made of it (dropping the trailing .min): at window 4096 the reserve
        // is min(max(512, 4096), 2048) = 2048, NOT 4096 — the emitted cap is
        // 2048, never 0.
        let w4096 = vec![entry(
            "tiny-window",
            ModelInfo {
                max_input_tokens: Some(4_096.0),
                ..Default::default()
            },
        )];
        let m = to_zed_models(&w4096, &[], "high");
        assert_eq!(m[0]["max_output_tokens"], json!(2_048));

        // Degenerate windows: reserve as much as possible without hitting
        // zero or underflow.
        let w2 = vec![entry(
            "two-token-window",
            ModelInfo {
                max_input_tokens: Some(2.0),
                ..Default::default()
            },
        )];
        let m = to_zed_models(&w2, &[], "high");
        assert_eq!(m[0]["max_output_tokens"], json!(1));
    }

    #[test]
    fn responses_mode_model_gets_chat_completions_false() {
        let entries = vec![entry(
            "o3-only-responses",
            ModelInfo {
                mode: Some("responses".to_string()),
                ..Default::default()
            },
        )];

        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(
            models[0]["capabilities"]["chat_completions"],
            json!(false),
            "responses-mode model must have chat_completions: false"
        );
    }

    #[test]
    fn chat_mode_model_omits_chat_completions() {
        // chat-completions defaults to true in Zed, so we omit it for
        // chat-mode models to avoid cluttering the settings.
        let entries = vec![entry(
            "gpt-4o",
            ModelInfo {
                mode: Some("chat".to_string()),
                ..Default::default()
            },
        )];

        let models = to_zed_models(&entries, &[], "medium");
        assert!(
            models[0]["capabilities"].get("chat_completions").is_none(),
            "chat-mode model should not set chat_completions (Zed defaults to true)"
        );
    }

    #[test]
    fn probe_response_detects_reasoning_content_presence() {
        // With reasoning_content in the response: model emits thinking.
        let with_reasoning = r#"{"choices":[{"message":{"reasoning_content":"thinking..."}}]}"#;
        let parsed: ProbeResponse = serde_json::from_str(with_reasoning).unwrap();
        let has_reasoning = parsed
            .choices
            .first()
            .and_then(|c| c.message.as_ref())
            .is_some_and(|m| m.reasoning_content.is_some());
        assert!(has_reasoning, "should detect reasoning_content emission");

        // Without reasoning_content: model accepts but doesn't emit.
        let without_reasoning = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        let parsed: ProbeResponse = serde_json::from_str(without_reasoning).unwrap();
        let has_reasoning = parsed
            .choices
            .first()
            .and_then(|c| c.message.as_ref())
            .is_some_and(|m| m.reasoning_content.is_some());
        assert!(!has_reasoning, "should detect absence of reasoning_content");
    }

    #[test]
    fn interleaved_reasoning_set_only_for_probed_models() {
        let entries = vec![entry(
            "glm-5.2",
            ModelInfo {
                supports_reasoning: Some(true),
                ..Default::default()
            },
        )];

        // Without probe results: defaults to false.
        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(
            models[0]["capabilities"]["interleaved_reasoning"],
            json!(false)
        );

        // With this model in the probe-supported list: set to true.
        let models = to_zed_models(&entries, &["glm-5.2".to_string()], "medium");
        assert_eq!(
            models[0]["capabilities"]["interleaved_reasoning"],
            json!(true)
        );
    }

    #[test]
    fn missing_token_counts_fall_back_to_default() {
        let entries = vec![entry("mystery-model", ModelInfo::default())];

        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(models[0]["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(models[0]["max_output_tokens"], json!(65_536));
    }

    #[test]
    fn merge_models_preserves_existing_entries_verbatim() {
        let existing = vec![json!({
            "name": "glm-5.2",
            "display_name": "GLM 5.2 (reasoning)",
            "max_tokens": 262144,
            "reasoning_effort": "high",
            "capabilities": { "interleaved_reasoning": true }
        })];
        let discovered = vec![
            json!({ "name": "glm-5.2", "max_tokens": 1048576 }),
            json!({ "name": "brand-new-model", "max_tokens": 128000 }),
        ];

        let merged = merge_models(existing, discovered);

        assert_eq!(merged.existing_count, 1);
        assert_eq!(merged.new_count, 1);
        assert_eq!(merged.models.len(), 2);
        // Existing entry untouched: manual tuning survives.
        assert_eq!(merged.models[0]["display_name"], "GLM 5.2 (reasoning)");
        assert_eq!(merged.models[0]["max_tokens"], 262144);
        assert_eq!(merged.models[0]["reasoning_effort"], "high");
        assert_eq!(
            merged.models[0]["capabilities"]["interleaved_reasoning"],
            true
        );
        // New model appended.
        assert_eq!(merged.models[1]["name"], "brand-new-model");
    }

    #[test]
    fn existing_models_reads_provider_entries_from_jsonc() {
        let settings = r#"{
  // comment
  "language_models": {
    "openai_compatible": {
      "litellm": {
        "api_url": "http://x/v1",
        "available_models": [{ "name": "glm-5.2", "max_tokens": 1 }],
      },
    },
  },
}"#;

        let models = existing_models(settings, "litellm");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"], "glm-5.2");

        assert!(existing_models(settings, "other-provider").is_empty());
        assert!(existing_models("{}", "litellm").is_empty());
    }

    #[test]
    fn merge_preserves_comments_and_other_settings() {
        let settings = r#"{
  // My favorite theme
  "theme": "One Dark",
  "language_models": {
    "openai_compatible": {
      "litellm": {
        "api_url": "http://old:4000/v1",
        "custom_headers": { "X-Team": "devops" },
        "available_models": []
      }
    }
  }
}
"#;

        let merged = merge_into_settings(
            settings,
            "litellm",
            "http://new:4000/v1",
            vec![json!({ "name": "gpt-4o", "max_tokens": 128000 })],
        )
        .unwrap();

        assert!(
            merged.contains("// My favorite theme"),
            "comment lost:\n{merged}"
        );
        assert!(merged.contains(r#""theme": "One Dark""#));
        assert!(
            merged.contains(r#""X-Team": "devops""#),
            "custom_headers lost:\n{merged}"
        );
        assert!(merged.contains("http://new:4000/v1"));
        assert!(merged.contains(r#""name": "gpt-4o""#));
        assert!(!merged.contains("http://old:4000/v1"));
    }

    #[test]
    fn merge_creates_missing_structure() {
        let merged = merge_into_settings(
            "{}\n",
            "litellm",
            "http://localhost:4000/v1",
            vec![json!({ "name": "gpt-4o", "max_tokens": 128000 })],
        )
        .unwrap();

        let parsed = jsonc_parser::parse_to_serde_value(&merged, &ParseOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed["language_models"]["openai_compatible"]["litellm"]["api_url"],
            "http://localhost:4000/v1"
        );
        assert_eq!(
            parsed["language_models"]["openai_compatible"]["litellm"]["available_models"][0]
                ["name"],
            "gpt-4o"
        );
    }

    #[test]
    fn normalizes_base_urls() {
        assert_eq!(
            normalize_base_url("http://localhost:4000"),
            "http://localhost:4000"
        );
        assert_eq!(
            normalize_base_url("http://localhost:4000/"),
            "http://localhost:4000"
        );
        assert_eq!(
            normalize_base_url("http://localhost:4000/v1"),
            "http://localhost:4000"
        );
        assert_eq!(
            normalize_base_url("http://localhost:4000/v1/"),
            "http://localhost:4000"
        );
    }

    #[test]
    fn warn_null_fields_reports_null_capabilities() {
        let entries = vec![entry(
            "laguna-s-2.1",
            ModelInfo {
                mode: Some("chat".to_string()),
                max_input_tokens: Some(262144.0),
                supports_reasoning: Some(true),
                ..Default::default()
            },
        )];

        let warnings = warn_null_fields(&entries);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("laguna-s-2.1"));
        assert!(warnings[0].contains("supports_function_calling"));
        assert!(warnings[0].contains("supports_vision"));
        assert!(warnings[0].contains("supports_parallel_function_calling"));
        assert!(warnings[0].contains("supports_prompt_caching"));
        assert!(!warnings[0].contains("supports_reasoning"));
        assert!(!warnings[0].contains("max_input_tokens"));
    }

    #[test]
    fn warn_null_fields_clean_when_all_set() {
        let entries = vec![entry(
            "glm-5.2",
            ModelInfo {
                mode: Some("chat".to_string()),
                max_input_tokens: Some(1048576.0),
                supports_function_calling: Some(true),
                supports_parallel_function_calling: Some(true),
                supports_vision: Some(false),
                supports_reasoning: Some(true),
                supports_prompt_caching: Some(true),
                ..Default::default()
            },
        )];

        let warnings = warn_null_fields(&entries);
        assert!(
            warnings.is_empty(),
            "no warnings when all fields set: {warnings:?}"
        );
    }

    #[test]
    fn warn_null_fields_reports_null_prompt_caching() {
        // Merged-unknown supports_prompt_caching silently turns into
        // prompt_cache_key: false (a real cost on fleets with prompt
        // caching), so it must be warned like the other capability gaps.
        let entries = vec![entry(
            "glm-5.3",
            ModelInfo {
                mode: Some("chat".to_string()),
                max_input_tokens: Some(262_144.0),
                supports_function_calling: Some(true),
                supports_parallel_function_calling: Some(true),
                supports_vision: Some(false),
                supports_reasoning: Some(true),
                ..Default::default()
            },
        )];

        let warnings = warn_null_fields(&entries);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("supports_prompt_caching"),
            "a merged-unknown prompt-caching flag must warn: {warnings:?}"
        );
    }

    #[test]
    fn warn_null_fields_skips_non_chat_models() {
        let entries = vec![entry(
            "qwen3-embedding-0.6b",
            ModelInfo {
                mode: Some("embedding".to_string()),
                ..Default::default()
            },
        )];

        let warnings = warn_null_fields(&entries);
        assert!(
            warnings.is_empty(),
            "embedding models should not be flagged: {warnings:?}"
        );
    }

    #[test]
    fn warn_null_fields_flags_null_mode() {
        let entries = vec![entry(
            "qwythos-9b",
            ModelInfo {
                max_input_tokens: Some(1010000.0),
                supports_function_calling: Some(true),
                supports_parallel_function_calling: Some(true),
                supports_vision: Some(false),
                supports_reasoning: Some(true),
                ..Default::default()
            },
        )];

        let warnings = warn_null_fields(&entries);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mode (null"));
    }
}
