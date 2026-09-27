//! Semantic identity comparison; secret values never leave this module's callers.
use super::*;
use crate::proxy::providers::codex_oauth_auth::existing_login_identity;

pub(super) struct Identity {
    pub value: Value,
    pub chatgpt: bool,
    pub name: String,
    pub saved_auth: Value,
}

pub(super) fn identify(live: &Value) -> Result<Option<Identity>> {
    let text = live["config"].as_str().ok_or("invalid_live_config")?;
    let config: toml::Value = toml::from_str(text).map_err(|_| "invalid_live_config")?;
    if config.get("profile").is_some() {
        return Err("external_profile_unsupported");
    }
    let source = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    let selected = config.get("model_providers").and_then(|p| p.get(source));
    let field = |key| {
        selected
            .and_then(|p| p.get(key))
            .and_then(toml::Value::as_str)
    };
    if selected.is_some_and(|table| table.get("auth").is_some() || table.get("aws").is_some()) {
        return Err("unsupported_auth_source");
    }
    if source != "openai" && selected.is_none() {
        return Err("unsupported_auth_source");
    }
    let base = field("base_url")
        .map(str::to_string)
        .or_else(|| {
            if source == "openai" {
                config
                    .get("openai_base_url")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            if source == "openai" {
                "https://api.openai.com/v1".into()
            } else {
                String::new()
            }
        });
    let api_key = codex::extract_codex_auth_api_key(&live["auth"]);
    let bearer = field("experimental_bearer_token");
    if bearer.is_some_and(|v| v.trim().is_empty()) {
        return Err("unsupported_auth_source");
    }
    let env_key = field("env_key");
    let headers = selected.and_then(|p| p.get("http_headers"));
    let env_headers = selected.and_then(|p| p.get("env_http_headers"));
    let requires_openai = selected
        .and_then(|p| p.get("requires_openai_auth"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(source == "openai");
    if bearer == Some("PROXY_MANAGED") || api_key.as_deref() == Some("PROXY_MANAGED") {
        return Err("external_proxy_unavailable");
    }
    let has_header_auth = headers.into_iter().chain(env_headers).any(|value| {
        value.as_table().is_some_and(|table| {
            table.keys().any(|key| {
                matches!(
                    key.to_ascii_lowercase().as_str(),
                    "authorization" | "x-api-key" | "api-key"
                )
            })
        })
    });
    if let Some(env) = env_key {
        if env.trim().is_empty() || std::env::var_os(env).is_none_or(|v| v.is_empty()) {
            return Err("credential_reference_unavailable");
        }
    }
    if let Some(table) = env_headers.and_then(toml::Value::as_table) {
        for value in table.values() {
            if !value.as_str().is_some_and(|env| {
                !env.trim().is_empty() && std::env::var_os(env).is_some_and(|v| !v.is_empty())
            }) {
                return Err("credential_reference_unavailable");
            }
        }
    }
    let has_config_auth =
        bearer.is_some() || env_key.is_some() || (!requires_openai && has_header_auth);
    let mode = live["auth"]["auth_mode"].as_str();
    if !has_config_auth
        && requires_openai
        && mode.is_some_and(|m| !matches!(m, "chatgpt" | "apikey"))
    {
        return Err("unsupported_auth_source");
    }
    if !has_config_auth && requires_openai && (api_key.is_none() || mode == Some("chatgpt")) {
        if codex::codex_config_auth_store_mode(text) != codex::CodexAuthStoreMode::File {
            return Err("unsupported_auth_store");
        }
        if live["auth"].as_object().is_some_and(|a| a.is_empty()) {
            return Ok(None);
        }
        let identity =
            existing_login_identity(&live["auth"]).map_err(|_| "incomplete_account_credentials")?;
        // Never archive an OAuth login as usable through an arbitrary custom host.
        if !matches!(
            base.as_str(),
            "https://api.openai.com/v1" | "https://chatgpt.com/backend-api/codex"
        ) {
            return Err("unsupported_auth_source");
        }
        return Ok(Some(Identity {
            value: json!({"kind":"chatgpt","sub":identity.subject,
            "workspace":identity.workspace_id}),
            chatgpt: true,
            name: "ChatGPT".into(),
            saved_auth: json!({}),
        }));
    }
    if !has_config_auth && (!requires_openai || api_key.is_none()) {
        return Ok(None);
    }
    if !has_config_auth
        && codex::codex_config_auth_store_mode(text) != codex::CodexAuthStoreMode::File
    {
        return Err("unsupported_auth_store");
    }
    let url = reqwest::Url::parse(&base).map_err(|_| "invalid_base_url")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid_base_url");
    }
    let wire = field("wire_api").unwrap_or("responses");
    if wire != "responses" {
        return Err("unsupported_api_format");
    }
    let auth = if let Some(value) = env_key {
        json!({"env":value})
    } else if let Some(value) = bearer {
        json!({"secret":value})
    } else if !requires_openai && has_header_auth {
        json!({"headers":headers,"envHeaders":env_headers})
    } else {
        json!({"secret":api_key})
    };
    let saved_auth = if !has_config_auth {
        json!({"OPENAI_API_KEY":api_key})
    } else {
        json!({})
    };
    Ok(Some(Identity {
        value: json!({"kind":"api","url":url.as_str(),"wire":wire,"auth":auth,
        "headers":headers,"envHeaders":env_headers,"query":selected.and_then(|p|p.get("query_params"))}),
        chatgpt: false,
        name: format!("Custom ({})", url.host_str().unwrap_or("API")),
        saved_auth,
    }))
}

pub(super) fn matches(state: &AppState, provider: &Provider, identity: &Identity) -> Result<bool> {
    if let Some(id) = provider
        .meta
        .as_ref()
        .and_then(|m| m.managed_account_id_for("codex_oauth"))
    {
        let stored = block_on(state.codex_oauth_manager.existing_account_identity(&id));
        return Ok(stored.is_some_and(|stored| {
            identity.value
                == json!({"kind":"chatgpt",
            "sub":stored.subject,"workspace":stored.workspace_id})
        }));
    }
    let mut effective = provider.settings_config.clone();
    if !provider.is_codex_oauth() && provider.category.as_deref() != Some("official") {
        effective["config"] = json!(codex::prepare_codex_provider_live_config(
            &effective["auth"],
            effective["config"].as_str().unwrap_or("")
        )
        .map_err(service_error)?);
    }
    Ok(identify(&effective)
        .ok()
        .flatten()
        .is_some_and(|stored| stored.value == identity.value))
}

pub(super) fn same_settings(state: &AppState, provider: &Provider, live: &Value) -> Result<bool> {
    fn comparable(state: &AppState, provider: &Provider, mut settings: Value) -> Result<Value> {
        settings = crate::services::provider::strip_common_config_from_live_settings(
            &state.db,
            &AppType::Codex,
            provider,
            settings,
        );
        codex::strip_codex_mcp_servers_from_settings(&mut settings).map_err(service_error)?;
        if !provider.is_codex_oauth() && provider.category.as_deref() != Some("official") {
            settings["config"] = json!(codex::prepare_codex_provider_live_config(
                &settings["auth"],
                settings["config"].as_str().unwrap_or("")
            )
            .map_err(service_error)?);
            settings["config"] = json!(
                codex::align_codex_requires_openai_auth_with_login_preservation(
                    settings["config"].as_str().unwrap_or(""),
                    crate::settings::preserve_codex_official_auth_on_switch()
                )
                .map_err(service_error)?
            );
        }
        // The generated catalog pointer is a native projection, not a private
        // preference. Preserve user-owned catalog paths with the native helper.
        let text = codex::set_codex_model_catalog_json_field(
            settings["config"].as_str().unwrap_or(""),
            None,
        )
        .map_err(service_error)?;
        let mut config: toml::Value = toml::from_str(&text).map_err(|_| "invalid_config")?;
        if let Some(providers) = config
            .get_mut("model_providers")
            .and_then(toml::Value::as_table_mut)
        {
            for table in providers
                .iter_mut()
                .filter_map(|(_, value)| value.as_table_mut())
            {
                table.remove("name");
            }
        }
        Ok(json!(config))
    }
    Ok(
        comparable(state, provider, provider.settings_config.clone())?
            == comparable(state, provider, live.clone())?,
    )
}
