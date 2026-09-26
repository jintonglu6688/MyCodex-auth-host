//! Presentation mapping only: upstream presets, model discovery and modelCatalog storage.
use super::{gui_provider, Result};
use crate::{AppState, AppType, Provider};
use futures::executor::block_on;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

pub(super) fn native_preset(id: &str) -> Result<Option<Provider>> {
    if matches!(id, "custom" | "official_api" | "") {
        return Ok(None);
    }
    let export: Value =
        serde_json::from_str(include_str!("gui_presets.json")).map_err(|_| "invalid_presets")?;
    let presets = export["presets"].as_array().ok_or("invalid_presets")?;
    let preset = if id == "chatgpt" {
        presets.iter().find(|p| p["providerType"] == "codex_oauth")
    } else {
        id.strip_prefix("cc-switch-")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| presets.get(n))
    }
    .ok_or("invalid_presets")?;
    if preset["providerType"] == "xai_oauth" {
        return Err("oauth_not_supported");
    }
    let mut provider = Provider::with_id(
        id.into(),
        preset["name"].as_str().ok_or("invalid_presets")?.into(),
        json!({
            "auth":preset["auth"],"config":preset["config"],
            "modelCatalog":{"models":preset.get("modelCatalog").cloned().unwrap_or(json!([]))},
        }),
        preset["websiteUrl"].as_str().map(str::to_string),
    );
    provider.category = preset["category"].as_str().map(str::to_string);
    provider.icon = preset["icon"].as_str().map(str::to_string);
    provider.icon_color = preset["iconColor"].as_str().map(str::to_string);
    // ProviderMeta's serde names are the upstream preset metadata names.
    let mut meta = serde_json::from_value::<crate::provider::ProviderMeta>(preset.clone())
        .map_err(|_| "invalid_presets")?;
    meta.common_config_enabled = Some(true);
    provider.meta = Some(meta);
    Ok(Some(provider))
}

pub(super) fn presets() -> Result<Value> {
    let export: Value =
        serde_json::from_str(include_str!("gui_presets.json")).map_err(|_| "invalid_presets")?;
    let mut result = vec![
        json!({"id":"official_api","name":"OpenAI API","kind":"official_api","baseUrl":"https://api.openai.com/v1","model":"","models":[],"advanced":{},"available":true,"allowsProtocolSelection":false}),
        json!({"id":"custom","name":"Custom","kind":"responses","baseUrl":"","model":"","models":[],"advanced":{},"available":true,"allowsProtocolSelection":true}),
    ];
    for (index, preset) in export["presets"]
        .as_array()
        .ok_or("invalid_presets")?
        .iter()
        .enumerate()
    {
        let config: toml::Value = preset["config"]
            .as_str()
            .ok_or("invalid_presets")?
            .parse()
            .map_err(|_| "invalid_presets")?;
        let bucket = config
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .unwrap_or("custom");
        let provider = config.get("model_providers").and_then(|p| p.get(bucket));
        let chatgpt = preset["providerType"] == "codex_oauth";
        let unsupported = preset["providerType"] == "xai_oauth";
        let kind = if chatgpt {
            "chatgpt"
        } else {
            match preset["apiFormat"].as_str() {
                Some("openai_chat") => "chat_completions",
                Some("anthropic") => "anthropic",
                Some("openai_responses") | None => "responses",
                _ => return Err("invalid_presets"),
            }
        };
        let mut advanced = json!({});
        for key in [
            "codexChatReasoning",
            "promptCacheRouting",
            "impersonateClaudeCode",
        ] {
            if let Some(value) = preset.get(key) {
                advanced[key] = value.clone();
            }
        }
        for (native, gui) in [("env_key", "envKey"), ("query_params", "queryParams")] {
            if let Some(value) = provider.and_then(|p| p.get(native)) {
                advanced[gui] = serde_json::to_value(value).map_err(|_| "invalid_presets")?;
            }
        }
        result.push(json!({
            "id": if chatgpt {"chatgpt".to_string()} else {format!("cc-switch-{index}")},
            "name": if chatgpt {json!("ChatGPT")} else {preset["name"].clone()},
            "kind":kind,
            "baseUrl":provider.and_then(|p|p.get("base_url")).and_then(toml::Value::as_str).unwrap_or(""),
            "model":config.get("model").and_then(toml::Value::as_str).unwrap_or(""),
            "models":preset.get("modelCatalog").cloned().unwrap_or(json!([])),
            "advanced":advanced,"available":!unsupported,
            "unavailableReason":if unsupported {Some("oauth_not_supported")} else {None},
            "allowsProtocolSelection":false,
        }));
    }
    Ok(json!({"presets":result}))
}

const MODEL_ALIASES: &[(&str, &str)] = &[
    ("display_name", "displayName"),
    ("context_window", "contextWindow"),
    ("reasoning_levels", "reasoningLevels"),
    ("default_reasoning_level", "defaultReasoningLevel"),
    ("input_modalities", "inputModalities"),
    ("supports_parallel_tool_calls", "supportsParallelToolCalls"),
    ("base_instructions", "baseInstructions"),
];

pub(super) fn read_models(provider: &Provider) -> Result<Vec<Value>> {
    let mut models = match provider
        .settings_config
        .get("modelCatalog")
        .and_then(|c| c.get("models"))
    {
        None => Vec::new(),
        Some(value) => value.as_array().ok_or("invalid_models")?.clone(),
    };
    for model in &mut models {
        let map = model.as_object_mut().ok_or("invalid_models")?;
        for (alias, canonical) in MODEL_ALIASES {
            if let Some(value) = map.remove(*alias) {
                map.entry(*canonical).or_insert(value);
            }
        }
    }
    Ok(models)
}

pub(super) fn apply_models(provider: &mut Provider, models: &[Value]) -> Result<()> {
    let previous = read_models(provider)?;
    let mut normalized = models.to_vec();
    for model in &mut normalized {
        let map = model.as_object_mut().ok_or("invalid_models")?;
        for (alias, canonical) in MODEL_ALIASES {
            if let Some(value) = map.remove(*alias) {
                map.entry(*canonical).or_insert(value);
            }
        }
        let prior = previous.iter().find(|p| p["model"] == model["model"]);
        gui_provider::restore_masks(model, prior)?;
    }
    validate_models(&normalized)?;
    // Preserve outer metadata and unknown per-model fields in the original native catalog.
    let settings = provider
        .settings_config
        .as_object_mut()
        .ok_or("invalid_provider")?;
    let catalog = settings.entry("modelCatalog").or_insert_with(|| json!({}));
    let catalog = catalog.as_object_mut().ok_or("invalid_models")?;
    catalog.insert("models".into(), json!(normalized));
    Ok(())
}

fn validate_models(models: &[Value]) -> Result<()> {
    if models.len() > 2048 {
        return Err("invalid_models");
    }
    let mut seen = HashSet::new();
    for model in models {
        let id = model["model"].as_str().ok_or("invalid_models")?;
        if id.trim().is_empty()
            || id != id.trim()
            || id.len() > 256
            || id.chars().any(char::is_control)
            || !seen.insert(id)
        {
            return Err("invalid_models");
        }
        if model
            .get("contextWindow")
            .filter(|v| !v.is_null())
            .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > i64::MAX as u64))
        {
            return Err("invalid_models");
        }
        for key in ["displayName", "defaultReasoningLevel", "baseInstructions"] {
            if model
                .get(key)
                .filter(|v| !v.is_null())
                .is_some_and(|v| !v.is_string())
            {
                return Err("invalid_models");
            }
        }
        for key in ["reasoningLevels", "inputModalities"] {
            if let Some(value) = model.get(key).filter(|v| !v.is_null()) {
                let items = value.as_array().ok_or("invalid_models")?;
                if items.len() > 64
                    || items.iter().any(|v| {
                        v.as_str().is_none_or(|s| {
                            s.is_empty() || s.len() > 64 || s.chars().any(char::is_control)
                        })
                    })
                {
                    return Err("invalid_models");
                }
            }
        }
        if let Some(default) = model["defaultReasoningLevel"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            if !model["reasoningLevels"]
                .as_array()
                .is_some_and(|levels| levels.contains(&json!(default)))
            {
                return Err("invalid_models");
            }
        }
        if model
            .get("supportsParallelToolCalls")
            .filter(|v| !v.is_null())
            .is_some_and(|v| !v.is_boolean())
        {
            return Err("invalid_models");
        }
    }
    Ok(())
}

fn validate_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).map_err(|_| "invalid_base_url")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err("invalid_base_url");
    }
    Ok(url)
}

fn validate_credential_target(base: &str, kind: &str, provider: &Provider) -> Result<()> {
    let (stored_base, _) = provider.resolve_usage_credentials(&AppType::Codex);
    let base = if matches!(kind, "responses" | "official_api")
        && provider
            .meta
            .as_ref()
            .is_some_and(|m| m.is_full_url == Some(true))
    {
        base.trim_end_matches('/')
            .strip_suffix("/responses")
            .unwrap_or(base)
    } else {
        base
    };
    if validate_url(&stored_base)?.as_str().trim_end_matches('/')
        != validate_url(base)?.as_str().trim_end_matches('/')
        || gui_provider::kind(provider)? != kind
    {
        return Err("credential_target_mismatch");
    }
    Ok(())
}

pub(super) fn fetch_models(state: &AppState, params: &Value) -> Result<Value> {
    block_on(fetch(state, params))
}

async fn fetch(state: &AppState, params: &Value) -> Result<Value> {
    let kind = params["kind"].as_str().ok_or("invalid_params")?;
    let models = if kind == "chatgpt" {
        if params.get("apiKey").is_some_and(|v| !v.is_null())
            || params["baseUrl"].as_str().is_some_and(|s| !s.is_empty())
        {
            return Err("invalid_params");
        }
        let id = params["accountId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("account_not_found")?;
        let manager = &state.codex_oauth_manager;
        let token = manager
            .get_valid_token_for_account(id)
            .await
            .map_err(|_| "reauthentication_required")?;
        let account = manager
            .chatgpt_account_id_for_account(id)
            .await
            .map_err(|_| "account_not_found")?;
        crate::services::codex_oauth_models::fetch_models_with_token(&token, &account)
            .await
            .map_err(|_| "model_fetch_failed")?
    } else {
        if !matches!(
            kind,
            "official_api" | "responses" | "chat_completions" | "anthropic"
        ) {
            return Err("invalid_params");
        }
        let base = params["baseUrl"].as_str().ok_or("invalid_base_url")?;
        let base_url = validate_url(base)?;
        if kind == "official_api"
            && (base_url.scheme() != "https" || base_url.host_str() != Some("api.openai.com"))
        {
            return Err("invalid_base_url");
        }
        let mut advanced = params
            .get("advanced")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(json!({}));
        if !advanced.is_object() {
            return Err("invalid_params");
        }
        let mut key = params["apiKey"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut previous = None;
        if let Some(id) = params["providerId"].as_str().filter(|s| !s.is_empty()) {
            let provider = state
                .db
                .get_provider_by_id(id, "codex")
                .map_err(|_| "database_failed")?
                .ok_or("provider_not_found")?;
            let target_matches = validate_credential_target(base, kind, &provider).is_ok();
            if target_matches {
                if key.is_none() {
                    let (_, stored_key) = provider.resolve_usage_credentials(&AppType::Codex);
                    key = (!stored_key.is_empty()).then_some(stored_key);
                }
                previous = Some(gui_provider::read_advanced(&provider)?);
            } else if key.is_none() {
                return Err("credential_target_mismatch");
            }
        }
        gui_provider::restore_masks(&mut advanced, previous.as_ref())?;
        gui_provider::validate_advanced(&advanced)?;
        if key.is_none() {
            if let Some(name) = advanced["envKey"].as_str().filter(|s| !s.is_empty()) {
                if !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                    return Err("invalid_params");
                }
                key = std::env::var(name).ok();
            }
        }
        let mut headers: BTreeMap<String, String> = advanced
            .get("requestHeaders")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| "invalid_params"))
            .transpose()?
            .unwrap_or_default();
        if key.as_deref().is_none_or(str::is_empty) && headers.is_empty() {
            return Err("api_key_required");
        }
        if kind == "anthropic" {
            if !headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("anthropic-version"))
            {
                headers.insert("anthropic-version".into(), "2023-06-01".into());
            }
        }
        let agent = crate::provider::parse_custom_user_agent(advanced["userAgent"].as_str())
            .ok()
            .flatten();
        let override_url = advanced["modelsUrl"].as_str().filter(|s| !s.is_empty());
        if let Some(target) = override_url {
            if validate_url(target)?.origin() != base_url.origin() {
                return Err("credential_target_mismatch");
            }
        }
        let candidates = crate::services::model_fetch::build_models_url_candidates(
            base,
            advanced["isFullUrl"].as_bool().unwrap_or(false),
            override_url,
        )
        .map_err(|_| "invalid_base_url")?;
        let format = (kind == "anthropic" && advanced["anthropicAuthHeader"] != "bearer")
            .then_some("anthropic-messages");
        if advanced
            .get("queryParams")
            .and_then(Value::as_object)
            .is_none_or(|query| query.is_empty())
        {
            let models = crate::services::model_fetch::fetch_models(
                base,
                key.as_deref().unwrap_or(""),
                advanced["isFullUrl"].as_bool().unwrap_or(false),
                override_url,
                agent,
                format,
                Some(&headers),
            )
            .await
            .map_err(|_| "model_fetch_failed")?;
            return Ok(model_rows(models));
        }
        let mut found = None;
        for candidate in candidates {
            let mut url = validate_url(&candidate)?;
            if url.origin() != base_url.origin() {
                return Err("credential_target_mismatch");
            }
            if let Some(query) = advanced.get("queryParams").filter(|v| !v.is_null()) {
                for (name, value) in query.as_object().ok_or("invalid_params")? {
                    url.query_pairs_mut()
                        .append_pair(name, value.as_str().ok_or("invalid_params")?);
                }
            }
            match crate::services::model_fetch::fetch_models(
                base,
                key.as_deref().unwrap_or(""),
                false,
                Some(url.as_str()),
                agent.clone(),
                format,
                Some(&headers),
            )
            .await
            {
                Ok(models) => {
                    found = Some(models);
                    break;
                }
                // Preserve upstream's 404/405-only fallback with query-appended candidates.
                Err(error) if missing_model_endpoint(&error) => {}
                Err(_) => return Err("model_fetch_failed"),
            }
        }
        found.ok_or("model_fetch_failed")?
    };
    Ok(model_rows(models))
}

fn model_rows(models: Vec<crate::services::model_fetch::FetchedModel>) -> Value {
    // Upstream exposes IDs only. Leave unsupported context/reasoning fields unset.
    json!({"models":models.into_iter().map(|m|json!({"model":m.id,"displayName":m.id,"ownedBy":m.owned_by})).collect::<Vec<_>>()})
}

fn missing_model_endpoint(error: &str) -> bool {
    error.starts_with("All candidates failed: HTTP 404 ")
        || error.starts_with("All candidates failed: HTTP 405 ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn model_fetch_does_not_retry_auth_failures_or_error_body_status_text() {
        assert!(missing_model_endpoint(
            "All candidates failed: HTTP 404 Not Found: missing"
        ));
        assert!(missing_model_endpoint(
            "All candidates failed: HTTP 405 Method Not Allowed: missing"
        ));
        for error in [
            "HTTP 401 Unauthorized: All candidates failed: HTTP 404 Not Found",
            "HTTP 429 Too Many Requests: retry later",
            "HTTP 500 Internal Server Error: All candidates failed: HTTP 405 Method Not Allowed",
            "Request failed: timeout",
            "Failed to parse response: EOF",
        ] {
            assert!(!missing_model_endpoint(error));
        }
        let rows = model_rows(vec![crate::services::model_fetch::FetchedModel {
            id: "vendor/model".into(),
            owned_by: None,
        }]);
        assert_eq!(rows["models"][0]["displayName"], "vendor/model");
        assert!(rows["models"][0].get("contextWindow").is_none());
    }

    #[test]
    fn generated_presets_match_upstream_and_have_native_catalogs() {
        let export: Value = serde_json::from_str(include_str!("gui_presets.json")).unwrap();
        assert_eq!(
            export["sourceSha256"],
            format!(
                "{:x}",
                Sha256::digest(include_bytes!(
                    "../../../src/config/codexProviderPresets.ts"
                ))
            )
        );
        let result = presets().unwrap();
        let items = result["presets"].as_array().unwrap();
        assert!(items.len() > 10);
        assert!(items
            .iter()
            .any(|p| p["id"] == "chatgpt" && p["kind"] == "chatgpt"));
        assert!(items.iter().any(|p| p["name"] == "DeepSeek"));
        assert!(items
            .iter()
            .any(|p| p["id"] == "custom" && p["allowsProtocolSelection"] == true));
        assert!(items.iter().any(|p| p["available"] == false));
        for item in items {
            validate_models(item["models"].as_array().unwrap()).unwrap();
            if item["available"] == true {
                if let Some(provider) = native_preset(item["id"].as_str().unwrap()).unwrap() {
                    assert_eq!(
                        read_models(&provider).unwrap(),
                        item["models"].as_array().unwrap().clone()
                    );
                }
            }
        }
    }

    #[test]
    fn native_model_catalog_roundtrip_preserves_metadata_and_clears_aliases() {
        let mut provider = Provider::with_id(
            "test".into(),
            "test".into(),
            json!({"modelCatalog":{"vendorMeta":"retain","models":[{"model":"one","context_window":4000,"vendorCapability":true}]}}),
            None,
        );
        let mut models = read_models(&provider).unwrap();
        assert_eq!(models[0]["contextWindow"], 4000);
        models[0]["contextWindow"] = Value::Null;
        apply_models(&mut provider, &models).unwrap();
        assert_eq!(
            provider.settings_config["modelCatalog"]["vendorMeta"],
            "retain"
        );
        assert_eq!(read_models(&provider).unwrap()[0]["vendorCapability"], true);
        assert!(provider.settings_config["modelCatalog"]["models"][0]
            .get("context_window")
            .is_none());
        assert!(validate_models(&[json!({"model":"a","contextWindow":0})]).is_err());
        assert!(validate_models(&[json!({"model":"a"}), json!({"model":"a"})]).is_err());
        assert!(validate_models(&[
            json!({"model":"a","reasoningLevels":["high"],"defaultReasoningLevel":"low"})
        ])
        .is_err());
    }

    #[test]
    fn model_masks_restore_by_id_after_reordering() {
        let mut provider = Provider::with_id(
            "test".into(),
            "test".into(),
            json!({"modelCatalog":{"models":[
                {"model":"one","vendorToken":"first"},{"model":"two","vendorToken":"second"}
            ]}}),
            None,
        );
        let mut models = read_models(&provider)
            .unwrap()
            .iter()
            .map(gui_provider::masked)
            .collect::<Vec<_>>();
        models.reverse();
        apply_models(&mut provider, &models).unwrap();
        assert_eq!(read_models(&provider).unwrap()[0]["vendorToken"], "second");
        models[0]["model"] = json!("new");
        assert!(apply_models(&mut provider, &models).is_err());
    }

    #[test]
    fn fetch_rejects_credential_destinations_before_network_access() {
        for address in [
            "https://key@example.test/v1",
            "file:///tmp/x",
            "https://example.test/v1?key=secret",
            "https://example.test/v1#secret",
        ] {
            assert!(validate_url(address).is_err());
        }
        let provider = Provider::with_id(
            "test".into(),
            "test".into(),
            json!({"config":"model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://example.test/v1'\nwire_api = 'responses'\n", "auth":{"OPENAI_API_KEY":"private"}}),
            None,
        );
        assert!(
            validate_credential_target("https://other.test/v1", "responses", &provider).is_err()
        );
        assert!(
            validate_credential_target("https://example.test/v2", "responses", &provider).is_err()
        );
        assert!(
            validate_credential_target("https://example.test/v1", "anthropic", &provider).is_err()
        );
        assert!(
            validate_credential_target("https://example.test/v1/", "responses", &provider).is_ok()
        );
    }
}
