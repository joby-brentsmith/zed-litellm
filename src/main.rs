use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Parser;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use jsonc_parser::ParseOptions;
use serde::Deserialize;
use serde_json::{json, Value};

/// Fallback context window for models where LiteLLM doesn't report
/// `max_input_tokens`.
const DEFAULT_MAX_TOKENS: u64 = 128_000;

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

    /// API key for the proxy; must be allowed to call /model/info
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

    /// reasoning_effort assigned to models that report supports_reasoning
    #[arg(long, default_value = "medium")]
    reasoning_effort: String,
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

#[derive(Deserialize)]
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
}

fn main() -> Result<()> {
    let args = Args::parse();

    let base_url = normalize_base_url(&args.url);
    let api_url = format!("{base_url}/v1");

    let entries = fetch_model_info(&base_url, args.api_key.as_deref())?;
    let interleaved = if args.probe {
        eprintln!("Probing reasoning models for interleaved_reasoning support...");
        probe_interleaved_reasoning(&api_url, &entries, args.api_key.as_deref())
    } else {
        Vec::new()
    };
    let discovered = to_zed_models(&entries, &interleaved, &args.reasoning_effort);
    if discovered.is_empty() {
        bail!("no chat models returned by {base_url}/model/info");
    }

    let settings_text = match fs::read_to_string(&args.settings) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", args.settings.display()));
        }
    };

    let models = if args.replace {
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

    if args.write {
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
    let response = request
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let parsed: ModelInfoResponse = response
        .into_json()
        .with_context(|| format!("parsing response from {url}"))?;
    Ok(parsed.data)
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
    api_key: Option<&str>,
) -> Vec<String> {
    let mut supported = Vec::new();
    let candidates: Vec<&str> = entries
        .iter()
        .filter_map(|entry| {
            let info = entry.model_info.as_ref()?;
            if info.supports_reasoning == Some(true) {
                entry.model_name.as_deref()
            } else {
                None
            }
        })
        .collect();

    for model_name in candidates {
        match probe_one_model(api_url, model_name, api_key) {
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

fn probe_one_model(api_url: &str, model_name: &str, api_key: Option<&str>) -> Result<bool> {
    let url = format!("{api_url}/chat/completions");
    // `reasoning_effort: medium` to actually trigger a reasoning turn;
    // `max_tokens: 1` to keep cost near zero. We only need a response
    // message shape, not real output.
    let payload = json!({
        "model": model_name,
        "max_tokens": 1,
        "reasoning_effort": "medium",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "reasoning_content": "prior thinking", "content": "ok"},
            {"role": "user", "content": "hi again"},
        ],
    });
    let body = serde_json::to_string(&payload)?;

    let mut request = ureq::post(&url)
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
        let mut model = serde_json::Map::new();
        model.insert("name".into(), json!(name));

        let max_tokens = info
            .max_input_tokens
            .or(info.max_tokens)
            .map(|tokens| tokens as u64)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        model.insert("max_tokens".into(), json!(max_tokens));

        // `max_output_tokens` is the per-response generation cap, NOT the
        // context window. LiteLLM's /model/info frequently reports it equal
        // to `max_input_tokens` (the context window), which is wrong: a model
        // can't generate `context_window` tokens when there's any input.
        // Writing that value makes Zed request `context_window` completion
        // tokens, guaranteeing overflow errors whenever input is non-empty.
        // Omit the field when LiteLLM's value is untrustworthy so the model
        // uses its own default per-response cap.
        if let Some(max_output) = info.max_output_tokens {
            let max_output = max_output as u64;
            if max_output < max_tokens {
                model.insert("max_output_tokens".into(), json!(max_output));
            }
        }
        if info.supports_reasoning == Some(true) {
            model.insert("reasoning_effort".into(), json!(reasoning_effort));
        }
        model.insert(
            "capabilities".into(),
            json!({
                "tools": info.supports_function_calling.unwrap_or(true),
                "images": info.supports_vision.unwrap_or(false),
                "parallel_tool_calls": info.supports_parallel_function_calling.unwrap_or(false),
                // Required by Zed's capabilities schema whenever `capabilities`
                // is present. LiteLLM reports `supports_prompt_caching`, so map
                // it directly rather than guessing `false`.
                "prompt_cache_key": info.supports_prompt_caching.unwrap_or(false),
                // True only when verified by --probe: the model accepts
                // `reasoning_content` in input AND emits it in output. Defaults
                // to false because LiteLLM's /model/info cannot report this and
                // a blind `true` degrades multi-turn reasoning on endpoints
                // that silently ignore the field.
                "interleaved_reasoning": interleaved_support,
            }),
        );

        by_name.insert(name.to_string(), Value::Object(model));
    }

    by_name.into_values().collect()
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
    fn maps_model_info_to_zed_model() {
        let entries = vec![entry(
            "gpt-4o",
            ModelInfo {
                mode: Some("chat".to_string()),
                max_input_tokens: Some(128000.0),
                max_output_tokens: Some(16384.0),
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
                "max_output_tokens": 16384,
                "capabilities": {
                    "tools": true,
                    "images": true,
                    "parallel_tool_calls": true,
                    "prompt_cache_key": true,
                    "interleaved_reasoning": false,
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
        // A model with no reported capabilities should still get safe defaults.
        assert_eq!(caps["tools"], json!(true));
        assert_eq!(caps["images"], json!(false));
        assert_eq!(caps["parallel_tool_calls"], json!(false));
        assert_eq!(caps["prompt_cache_key"], json!(false));
        assert_eq!(caps["interleaved_reasoning"], json!(false));
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

    /// Regression guard: LiteLLM frequently reports `max_output_tokens`
    /// equal to `max_input_tokens` (the context window). That's bad data — a
    /// model can't generate `context_window` tokens with any input present,
    /// and writing it makes Zed request that many completion tokens, causing
    /// overflow errors. The tool must omit `max_output_tokens` when it's
    /// untrustworthy (>= the resolved `max_tokens`).
    #[test]
    fn omits_max_output_tokens_when_untrustworthy() {
        // max_output_tokens == max_input_tokens: untrustworthy, must omit.
        let entries = vec![entry(
            "bad-data-model",
            ModelInfo {
                max_input_tokens: Some(1048576.0),
                max_output_tokens: Some(1048576.0),
                ..Default::default()
            },
        )];
        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(models[0]["max_tokens"], json!(1048576));
        assert_eq!(
            models[0].get("max_output_tokens"),
            None,
            "must omit max_output_tokens when it equals the context window"
        );

        // max_output_tokens < max_input_tokens: trustworthy, keep it.
        let entries = vec![entry(
            "good-data-model",
            ModelInfo {
                max_input_tokens: Some(200000.0),
                max_output_tokens: Some(8192.0),
                ..Default::default()
            },
        )];
        let models = to_zed_models(&entries, &[], "medium");
        assert_eq!(models[0]["max_tokens"], json!(200000));
        assert_eq!(models[0]["max_output_tokens"], json!(8192));
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
        assert_eq!(models[0].get("max_output_tokens"), None);
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
}
