//! Flat form fields mapped onto native Provider data; no provider persistence here.
use super::{gui_catalog, service_error, Result};
use crate::{AppState, AppType, Provider, ProviderService};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[cfg(test)]
#[path = "gui_provider_context_tests.rs"]
mod context_tests;

pub(super) fn hash(value: &Value) -> String {
    fn ordered(value: &Value) -> Value {
        match value {
            Value::Object(map) => json!(map
                .iter()
                .map(|(k, v)| (k, ordered(v)))
                .collect::<std::collections::BTreeMap<_, _>>()),
            Value::Array(items) => Value::Array(items.iter().map(ordered).collect()),
            _ => value.clone(),
        }
    }
    format!(
        "{:x}",
        Sha256::digest(ordered(value).to_string().as_bytes())
    )
}

pub(super) fn stored(state: &AppState, id: &str) -> Result<Provider> {
    state
        .db
        .get_provider_by_id(id, "codex")
        .map_err(service_error)?
        .ok_or("provider_not_found")
}

pub(super) fn snapshot(state: &AppState, provider: &Provider) -> Result<Provider> {
    let mut edited = provider.clone();
    if ProviderService::current(state, AppType::Codex).map_err(service_error)? == provider.id
        && !state
            .proxy_service
            .detect_takeover_in_live_config_for_app(&AppType::Codex)
    {
        let Some(live) = super::auto_capture::owned_snapshot(state, provider).unwrap_or(None)
        else {
            return Ok(edited);
        };
        let live = crate::services::provider::strip_common_config_from_live_settings(
            &state.db,
            &AppType::Codex,
            provider,
            live,
        );
        // Keep native fields not represented by auth/config (catalog and future metadata).
        for field in ["auth", "config"] {
            if let Some(value) = live.get(field) {
                edited.settings_config[field] = value.clone();
            }
        }
    }
    Ok(edited)
}

pub(super) fn version(stored: &Provider, edited: &Provider) -> String {
    hash(&json!({"stored":stored,"edited":edited}))
}

// Only retire our own catalog projection. External catalog files remain user-owned.
pub(super) fn clear_chatgpt_catalog(provider: &mut Provider) -> Result<bool> {
    if kind(provider)? != "chatgpt" {
        return Ok(false);
    }
    let config = provider.settings_config["config"].as_str().unwrap_or("");
    let cleaned = crate::codex_config::set_codex_model_catalog_json_field(config, None)
        .map_err(|_| "invalid_config")?;
    let changed = cleaned != config || provider.settings_config.get("modelCatalog").is_some();
    if changed {
        provider
            .settings_config
            .as_object_mut()
            .ok_or("invalid_provider")?
            .remove("modelCatalog");
        provider.settings_config["config"] = json!(cleaned);
    }
    Ok(changed)
}

pub(super) fn kind(provider: &Provider) -> Result<&'static str> {
    let (base, _) = provider.resolve_usage_credentials(&AppType::Codex);
    if provider.is_codex_oauth()
        || provider.category.as_deref() == Some("official")
        || provider
            .meta
            .as_ref()
            .and_then(|m| m.auth_binding.as_ref())
            .is_some_and(|binding| binding.auth_provider.as_deref() == Some("codex_oauth"))
    {
        return Ok("chatgpt");
    }
    match provider
        .meta
        .as_ref()
        .and_then(|m| m.api_format.as_deref())
        .or_else(|| provider.settings_config["apiFormat"].as_str())
        .or_else(|| provider.settings_config["api_format"].as_str())
        .unwrap_or("openai_responses")
    {
        "openai_chat" => Ok("chat_completions"),
        "anthropic" => Ok("anthropic"),
        "openai_responses"
            if reqwest::Url::parse(&base)
                .ok()
                .and_then(|v| v.host_str().map(str::to_owned))
                .as_deref()
                == Some("api.openai.com") =>
        {
            Ok("official_api")
        }
        "openai_responses" => Ok("responses"),
        _ => Err("unsupported_api_format"),
    }
}

pub(super) fn summary(state: &AppState, stored: &Provider, edited: &Provider) -> Result<Value> {
    let kind = kind(edited)?;
    let (base, key) = edited.resolve_usage_credentials(&AppType::Codex);
    if !base.is_empty() {
        let url = reqwest::Url::parse(&base).map_err(|_| "invalid_base_url")?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err("invalid_base_url");
        }
    }
    let base = masked_url(&base);
    let base = if edited
        .meta
        .as_ref()
        .is_some_and(|m| m.is_full_url == Some(true))
        && matches!(kind, "responses" | "official_api")
    {
        format!("{}/responses", base.trim_end_matches('/'))
    } else {
        base
    };
    let config = config(edited)?;
    let account_id = edited
        .meta
        .as_ref()
        .and_then(|m| m.auth_binding.as_ref())
        .and_then(|b| b.account_id.as_deref());
    let accounts = futures::executor::block_on(state.codex_oauth_manager.list_accounts());
    let account = accounts.iter().find(|a| Some(a.id.as_str()) == account_id);
    let presets = gui_catalog::presets()?;
    let preset = presets["presets"].as_array().and_then(|items| {
        items.iter().find(|item| {
            item["kind"] == kind
                && (kind == "chatgpt" || item["baseUrl"].as_str() == Some(base.as_str()))
        })
    });
    Ok(json!({"id":edited.id,"name":edited.name,"kind":kind,
        "baseUrl":if kind == "chatgpt" {"https://chatgpt.com/backend-api/codex"} else {&base},
        "model":config.get("model").and_then(toml::Value::as_str).unwrap_or(""),
        "version":version(stored,edited),"presetId":preset.map(|p|p["id"].clone()).unwrap_or(json!("custom")),
        "presetName":preset.map(|p|p["name"].clone()).unwrap_or(json!("Custom")),
        "hasApiKey":!key.is_empty(),"accountId":account_id,
        "accountLabel":account.map(|a| a.login.as_str()),
        "needsReauthentication":account_id.is_some() && account.is_none_or(|a|a.reauth_required),
        "commonConfigEnabled":edited.meta.as_ref().and_then(|m|m.common_config_enabled).unwrap_or(true),
        "models":masked(&json!(gui_catalog::read_models(edited)?)),"advanced":masked(&read_advanced(edited)?)}))
}

fn masked_url(base: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(base) else {
        return base.to_string();
    };
    if url.query().is_none() {
        return base.to_string();
    }
    let keys = url
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .collect::<Vec<_>>();
    url.set_query(None);
    for key in keys {
        url.query_pairs_mut().append_pair(&key, "********");
    }
    url.to_string()
}

fn config(provider: &Provider) -> Result<toml::Value> {
    toml::from_str(provider.settings_config["config"].as_str().unwrap_or(""))
        .map_err(|_| "invalid_config")
}

pub(super) fn read_advanced(provider: &Provider) -> Result<Value> {
    let config = config(provider)?;
    let mut result = read_context(&config)?;
    let source = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    let table = config.get("model_providers").and_then(|p| p.get(source));
    for (native, flat) in [
        ("env_key", "envKey"),
        ("http_headers", "requestHeaders"),
        ("query_params", "queryParams"),
    ] {
        if let Some(value) = table.and_then(|t| t.get(native)) {
            result[flat] = serde_json::to_value(value).map_err(|_| "invalid_config")?;
        }
    }
    if let Some(meta) = &provider.meta {
        let fields = serde_json::to_value(meta).map_err(|_| "invalid_provider")?;
        for (native, flat) in [
            ("isFullUrl", "isFullUrl"),
            ("customUserAgent", "userAgent"),
            ("maxOutputTokens", "maxTokens"),
            ("impersonateClaudeCode", "impersonateClaudeCode"),
            ("promptCacheRouting", "promptCacheRouting"),
            ("codexChatReasoning", "codexChatReasoning"),
        ] {
            if let Some(value) = fields.get(native) {
                result[flat] = value.clone();
            }
        }
        if let Some(overrides) = &meta.local_proxy_request_overrides {
            if !overrides.headers.is_empty() {
                result["requestHeaders"] = json!(overrides.headers);
            }
            if let Some(body) = &overrides.body {
                result["requestBody"] = body.clone();
            }
        }
        result["anthropicAuthHeader"] = json!(if meta.api_key_field.as_deref()
            == Some("ANTHROPIC_API_KEY")
        {
            "x-api-key"
        } else {
            "bearer"
        });
    }
    Ok(result)
}

pub(super) fn read_context(config: &toml::Value) -> Result<Value> {
    let source = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    let table = config.get("model_providers").and_then(|p| p.get(source));
    let mut result = json!({"remoteCompaction":
        crate::codex_config::is_custom_codex_model_provider_id(source)
            && table.and_then(|t| t.get("name")).and_then(toml::Value::as_str) == Some("OpenAI")});
    for (native, flat) in [
        ("model_context_window", "modelContextWindow"),
        ("model_auto_compact_token_limit", "autoCompactTokenLimit"),
    ] {
        if let Some(value) = config.get(native) {
            result[flat] = serde_json::to_value(value).map_err(|_| "invalid_config")?;
        }
    }
    Ok(result)
}

pub(super) fn masked(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let name = key.to_ascii_lowercase().replace(['_', '-'], "");
                    let numeric_limit = name == "autocompacttokenlimit" && value.is_i64();
                    let secret = !matches!(name.as_str(), "maxtokens" | "maxoutputtokens")
                        && !numeric_limit
                        && [
                            "authorization",
                            "apikey",
                            "token",
                            "secret",
                            "password",
                            "credential",
                            "cookie",
                        ]
                        .iter()
                        .any(|part| name.contains(part));
                    let value = if secret && !value.is_null() {
                        json!("********")
                    } else if matches!(key.as_str(), "requestHeaders" | "queryParams") {
                        value
                            .as_object()
                            .map(|m| {
                                Value::Object(
                                    m.keys().map(|k| (k.clone(), json!("********"))).collect(),
                                )
                            })
                            .unwrap_or_else(|| value.clone())
                    } else {
                        masked(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(masked).collect()),
        _ => value.clone(),
    }
}

pub(super) fn restore_masks(value: &mut Value, previous: Option<&Value>) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                restore_masks(value, previous.and_then(|v| v.get(key)))?;
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter_mut().enumerate() {
                restore_masks(value, previous.and_then(|v| v.get(index)))?;
            }
        }
        Value::String(text) if text == "********" => {
            *value = previous
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or("invalid_params")?
        }
        _ => (),
    }
    Ok(())
}

pub(super) fn validate_toml(text: &str) -> Result<()> {
    if text.len() > 1024 * 1024 {
        return Err("invalid_config");
    }
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|_| "invalid_config")?;
    Ok(())
}

pub(super) fn replace_config(provider: &mut Provider, text: &str) -> Result<()> {
    validate_toml(text)?;
    let config: toml::Value = text.parse().map_err(|_| "invalid_config")?;
    for key in ["model", "model_provider"] {
        if config.get(key).is_some_and(|v| !v.is_str()) {
            return Err("invalid_config");
        }
    }
    if let Some(providers) = config.get("model_providers") {
        let providers = providers.as_table().ok_or("invalid_config")?;
        for table in providers.values() {
            let table = table.as_table().ok_or("invalid_config")?;
            for key in [
                "name",
                "base_url",
                "wire_api",
                "env_key",
                "experimental_bearer_token",
            ] {
                if table.get(key).is_some_and(|v| !v.is_str()) {
                    return Err("invalid_config");
                }
            }
        }
    }
    provider.settings_config["config"] = json!(text);
    let settings = provider
        .settings_config
        .as_object_mut()
        .ok_or("invalid_config")?;
    settings.remove("base_url");
    settings.remove("baseURL");
    // TOML headers edited here must also replace the proxy metadata projection.
    if let Some(overrides) = provider
        .meta
        .as_mut()
        .and_then(|m| m.local_proxy_request_overrides.as_mut())
    {
        overrides.headers.clear();
    }
    let advanced = read_advanced(provider)?;
    validate_advanced(&advanced)?;
    apply_advanced(provider, &advanced)?;
    Ok(())
}

pub(super) fn edit(provider: Provider, params: &Value) -> Result<Provider> {
    edit_draft(provider, params, false)
}

pub(super) fn edit_draft(mut provider: Provider, params: &Value, draft: bool) -> Result<Provider> {
    if let Some(text) = params.get("configToml") {
        replace_config(&mut provider, text.as_str().ok_or("invalid_params")?)?;
    }
    let name = params["name"].as_str().ok_or("invalid_params")?;
    let kind = params["kind"].as_str().ok_or("invalid_params")?;
    let model = params["model"].as_str().ok_or("invalid_params")?;
    if (!draft && name.trim().is_empty())
        || name.len() > 256
        || model.len() > 256
        || model.chars().any(char::is_control)
        || (!draft && kind != "chatgpt" && model.trim().is_empty())
    {
        return Err("invalid_params");
    }
    let format = match kind {
        "chatgpt" | "official_api" | "responses" => "openai_responses",
        "chat_completions" => "openai_chat",
        "anthropic" => "anthropic",
        _ => return Err("invalid_params"),
    };
    let previous_advanced = read_advanced(&provider)?;
    let mut advanced = params
        .get("advanced")
        .cloned()
        .unwrap_or_else(|| previous_advanced.clone());
    restore_masks(&mut advanced, Some(&previous_advanced))?;
    validate_advanced(&advanced)?;
    let (stored_base, stored_key) = provider.resolve_usage_credentials(&AppType::Codex);
    let key = if params["clearApiKey"] == true {
        ""
    } else {
        params["apiKey"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(&stored_key)
    };
    if key.len() > 16384 || key.chars().any(char::is_control) {
        return Err("invalid_api_key");
    }
    let mut document = provider.settings_config["config"]
        .as_str()
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "invalid_config")?;
    document["model"] = toml_edit::value(model);
    // Omitted fields retain native values, including non-1M context windows.
    // Null is an explicit deletion, as when CC's 1M toggle is turned off.
    for (flat, native) in [
        ("modelContextWindow", "model_context_window"),
        ("autoCompactTokenLimit", "model_auto_compact_token_limit"),
    ] {
        if let Some(value) = advanced.get(flat) {
            if value.is_null() {
                document.remove(native);
            } else {
                document[native] = toml_edit::value(value.as_i64().ok_or("invalid_params")?);
            }
        }
    }
    let meta = provider.meta.get_or_insert_with(Default::default);
    meta.api_format = Some(format.to_string());
    if let Some(enabled) = params.get("commonConfigEnabled") {
        meta.common_config_enabled = Some(enabled.as_bool().ok_or("invalid_params")?);
    } else if meta.common_config_enabled.is_none() {
        meta.common_config_enabled = Some(true);
    }
    if kind == "chatgpt" {
        let id = params["accountId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or(if draft { Some("") } else { None })
            .ok_or("account_not_found")?;
        meta.auth_binding = Some(crate::provider::AuthBinding {
            source: crate::provider::AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".into()),
            account_id: Some(id.into()),
        });
        meta.provider_type = Some("codex_oauth".into());
        provider.category = Some("official".into());
        document.remove("model_provider");
        // Other inactive provider definitions remain user-owned. Remove only a
        // native OpenAI override that would intercept the account's direct URL.
        if let Some(providers) = document
            .get_mut("model_providers")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            providers.remove("openai");
        }
        document.remove("base_url");
        provider.settings_config["auth"] = json!({});
    } else {
        let requested_base = params["baseUrl"].as_str().ok_or("invalid_base_url")?;
        let base = if requested_base == masked_url(&stored_base) {
            stored_base.as_str()
        } else {
            requested_base
        };
        if !draft || !base.is_empty() {
            let url = reqwest::Url::parse(base).map_err(|_| "invalid_base_url")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err("invalid_base_url");
            }
            if kind == "official_api"
                && (url.scheme() != "https" || url.host_str() != Some("api.openai.com"))
            {
                return Err("invalid_base_url");
            }
        }
        meta.auth_binding = None;
        meta.provider_type = None;
        if provider.category.as_deref() == Some("official") {
            provider.category = None;
        }
        let source = document
            .get("model_provider")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("custom")
            .to_string();
        document["model_provider"] = toml_edit::value(&source);
        if !document.contains_key("model_providers") {
            document["model_providers"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let providers = document["model_providers"]
            .as_table_like_mut()
            .ok_or("invalid_config")?;
        if !providers.contains_key(&source) {
            providers.insert(&source, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let table = providers
            .get_mut(&source)
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or("invalid_config")?;
        let preserve_auth_source = table.contains_key("base_url")
            && params["apiKey"].as_str().is_none_or(|key| key.is_empty())
            && params["clearApiKey"] != true;
        let remote_compaction = advanced
            .get("remoteCompaction")
            .and_then(Value::as_bool)
            .unwrap_or(previous_advanced["remoteCompaction"] == true);
        let native_name = if crate::codex_config::is_custom_codex_model_provider_id(&source)
            && remote_compaction
        {
            "OpenAI"
        } else {
            name
        };
        table.insert("name", toml_edit::value(native_name));
        let base = if advanced["isFullUrl"] == true && format == "openai_responses" {
            base.strip_suffix("/responses").ok_or("invalid_base_url")?
        } else {
            base
        };
        table.insert("base_url", toml_edit::value(base));
        table.insert("wire_api", toml_edit::value("responses"));
        if !preserve_auth_source {
            table.insert("requires_openai_auth", toml_edit::value(true));
        }
        if params.get("apiKey").is_some() || params["clearApiKey"] == true {
            table.remove("experimental_bearer_token");
        }
        if let Some(env) = advanced["envKey"].as_str().filter(|s| !s.is_empty()) {
            table.insert("env_key", toml_edit::value(env));
        } else {
            table.remove("env_key");
        }
        for (flat, native) in [
            ("queryParams", "query_params"),
            ("requestHeaders", "http_headers"),
        ] {
            if let Some(values) = advanced.get(flat) {
                let mut fields = toml_edit::Table::new();
                for (key, value) in values.as_object().ok_or("invalid_params")? {
                    fields.insert(
                        key,
                        toml_edit::value(value.as_str().ok_or("invalid_params")?),
                    );
                }
                table.insert(native, toml_edit::Item::Table(fields));
            }
        }
        if !provider.settings_config["auth"].is_object() {
            provider.settings_config["auth"] = json!({});
        }
        provider.settings_config["auth"]["OPENAI_API_KEY"] = json!(key);
    }
    provider.name = name.into();
    // The native adapter gives JSON aliases priority over TOML. The form edits
    // the TOML source, so stale aliases must never override the newly saved URL.
    if let Some(settings) = provider.settings_config.as_object_mut() {
        settings.remove("base_url");
        settings.remove("baseURL");
        settings.remove("apiFormat");
        settings.remove("api_format");
    }
    provider.settings_config["config"] = json!(document.to_string());
    apply_advanced(&mut provider, &advanced)?;
    if kind == "chatgpt" {
        clear_chatgpt_catalog(&mut provider)?;
    } else if let Some(models) = params.get("models") {
        gui_catalog::apply_models(&mut provider, models.as_array().ok_or("invalid_models")?)?;
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn provider() -> Provider {
        Provider::with_id(
            "one".into(),
            "Original".into(),
            json!({
            "config":"model='old'\nmodel_provider='custom'\nunknown_preference='retain'\n[model_providers.custom]\nbase_url='https://old.invalid/v1'\nwire_api='responses'\n[model_providers.other]\nbase_url='https://other.invalid/v1'\nextra='retain'\n",
            "auth":{"OPENAI_API_KEY":"synthetic-secret-key"},"base_url":"https://old-alias.invalid/v1","baseURL":"https://another-alias.invalid/v1",
            "api_format":"openai_chat","unknown":{"keep":true}}),
            None,
        )
    }
    #[test]
    fn edited_native_endpoint_wins_over_aliases_and_retains_unrepresented_fields() {
        use crate::proxy::providers::{CodexAdapter, ProviderAdapter};
        let result=edit(provider(),&json!({"name":"New","kind":"responses","baseUrl":"https://new.invalid/v1","model":"new"})).unwrap();
        assert_eq!(
            CodexAdapter.extract_base_url(&result).unwrap(),
            "https://new.invalid/v1"
        );
        assert_eq!(kind(&result).unwrap(), "responses");
        assert_eq!(result.settings_config["unknown"]["keep"], true);
        assert_eq!(
            result.settings_config["auth"]["OPENAI_API_KEY"],
            "synthetic-secret-key"
        );
        let config = config(&result).unwrap();
        assert_eq!(config["unknown_preference"].as_str(), Some("retain"));
        assert_eq!(
            config["model_providers"]["other"]["extra"].as_str(),
            Some("retain")
        );
    }
    #[test]
    fn account_edit_preserves_inactive_provider_definitions() {
        let result = edit(
            provider(),
            &json!({"name":"Account","kind":"chatgpt","model":"","accountId":"fake-account"}),
        )
        .unwrap();
        assert_eq!(
            config(&result).unwrap()["model_providers"]["other"]["extra"].as_str(),
            Some("retain")
        );
        assert_eq!(kind(&result).unwrap(), "chatgpt");
        assert!(result.settings_config["auth"]
            .as_object()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn account_edit_retires_only_owned_catalog_and_ignores_submitted_models() {
        for pointer in ["cc-switch-model-catalog.json", "external-catalog.json"] {
            let mut original = provider();
            original.settings_config["modelCatalog"] = json!({"models":[{"model":"wrong"}]});
            original.settings_config["config"] =
                json!(format!("model='chosen'\nmodel_catalog_json='{pointer}'\n"));
            let result = edit(
                original,
                &json!({"name":"Account","kind":"chatgpt","model":"chosen",
                "accountId":"fake-account","models":[{"model":"wrong","reasoningLevels":["low"]}]}),
            )
            .unwrap();
            assert!(result.settings_config.get("modelCatalog").is_none());
            let config = config(&result).unwrap();
            assert_eq!(config["model"].as_str(), Some("chosen"));
            assert_eq!(
                config
                    .get("model_catalog_json")
                    .and_then(toml::Value::as_str),
                (pointer == "external-catalog.json").then_some(pointer)
            );
        }
    }
    #[test]
    fn headers_are_masked_even_for_unfamiliar_secret_names_and_hashes_are_order_independent() {
        let original = json!({"requestHeaders":{"odd":"synthetic-secret-header"},"queryParams":{"unusual":"synthetic-secret-query"}});
        let mut projected = masked(&original);
        assert!(!projected.to_string().contains("synthetic-secret"));
        restore_masks(&mut projected, Some(&original)).unwrap();
        assert_eq!(projected, original);
        let a: Value = serde_json::from_str(r#"{"b":2,"a":{"y":2,"x":1}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"x":1,"y":2},"b":2}"#).unwrap();
        assert_eq!(hash(&a), hash(&b));
    }
}

pub(super) fn validate_advanced(value: &Value) -> Result<()> {
    if !value.is_object() || value.to_string().len() > 65536 {
        return Err("invalid_params");
    }
    for key in ["isFullUrl", "impersonateClaudeCode", "remoteCompaction"] {
        if value
            .get(key)
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err("invalid_params");
        }
    }
    for key in ["modelContextWindow", "autoCompactTokenLimit"] {
        if value
            .get(key)
            .is_some_and(|v| !v.is_null() && v.as_i64().is_none_or(|n| n <= 0))
        {
            return Err("invalid_params");
        }
    }
    if value
        .get("anthropicAuthHeader")
        .is_some_and(|v| !matches!(v.as_str(), Some("bearer" | "x-api-key")))
    {
        return Err("invalid_params");
    }
    if value
        .get("envKey")
        .and_then(Value::as_str)
        .is_some_and(|s| {
            !s.is_empty() && !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    {
        return Err("invalid_params");
    }
    for name in ["requestHeaders", "queryParams", "requestBody"] {
        if let Some(value) = value.get(name).filter(|v| !v.is_null()) {
            let map = value.as_object().ok_or("invalid_params")?;
            if map.len() > 64
                || map.iter().any(|(k, v)| {
                    k.is_empty()
                        || k.chars().any(char::is_control)
                        || (name != "requestBody"
                            && v.as_str()
                                .is_none_or(|s| s.len() > 16384 || s.chars().any(char::is_control)))
                })
            {
                return Err("invalid_params");
            }
        }
    }
    Ok(())
}

fn apply_advanced(provider: &mut Provider, advanced: &Value) -> Result<()> {
    let meta = provider.meta.get_or_insert_with(Default::default);
    let mut fields = serde_json::to_value(&*meta).map_err(|_| "invalid_provider")?;
    let map = fields.as_object_mut().ok_or("invalid_provider")?;
    for (flat, native) in [
        ("isFullUrl", "isFullUrl"),
        ("userAgent", "customUserAgent"),
        ("maxTokens", "maxOutputTokens"),
        ("impersonateClaudeCode", "impersonateClaudeCode"),
        ("promptCacheRouting", "promptCacheRouting"),
        ("codexChatReasoning", "codexChatReasoning"),
    ] {
        match advanced.get(flat).filter(|v| !v.is_null()) {
            Some(value) => {
                map.insert(native.into(), value.clone());
            }
            None => {
                map.remove(native);
            }
        }
    }
    if let Some(header) = advanced["anthropicAuthHeader"].as_str() {
        if !matches!(header, "bearer" | "x-api-key") {
            return Err("invalid_params");
        }
        map.insert(
            "apiKeyField".into(),
            json!(if header == "x-api-key" {
                "ANTHROPIC_API_KEY"
            } else {
                "ANTHROPIC_AUTH_TOKEN"
            }),
        );
    }
    if advanced.get("requestHeaders").is_some() || advanced.get("requestBody").is_some() {
        map.insert("localProxyRequestOverrides".into(),json!({"headers":advanced.get("requestHeaders").cloned().unwrap_or(json!({})),"body":advanced.get("requestBody").cloned()}));
    }
    *meta = serde_json::from_value(fields).map_err(|_| "invalid_params")?;
    if meta.max_output_tokens == Some(0) {
        return Err("invalid_params");
    }
    if crate::provider::parse_custom_user_agent(meta.custom_user_agent.as_deref()).is_err() {
        return Err("invalid_params");
    }
    Ok(())
}
