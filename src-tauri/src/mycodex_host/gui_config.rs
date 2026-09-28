//! Explicit TOML editors; native services still own storage and live projection.
use super::{auto_capture, gui, gui_provider as form, lifecycle, service_error, Host, Result};
use crate::{AppType, ProviderService};
use serde_json::{json, Value};

fn common(host: &Host) -> Result<Value> {
    let text = host
        .state
        .db
        .get_config_snippet("codex")
        .map_err(service_error)?;
    let cleared = host
        .state
        .db
        .is_config_snippet_cleared("codex")
        .map_err(service_error)?;
    Ok(json!({"text":text.as_deref().unwrap_or(""),
        "version":form::hash(&json!([text, cleared, gui::fingerprint(host)?]))}))
}

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    let state = &host.state;
    match method {
        "gui/config/common/get" => {
            auto_capture::read(state)?;
            common(host)
        }
        "gui/config/common/save" => {
            let text = params["text"].as_str().ok_or("invalid_params")?;
            form::validate_toml(text)?;
            let _guard = lifecycle::mutation()?;
            auto_capture::before_write(state)?;
            let previous = common(host)?;
            if params["expectedVersion"].as_str() != previous["version"].as_str() {
                return Err("version_conflict");
            }
            let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
            // Match CC's set_common_config_snippet, including legacy migration and
            // the explicit-cleared marker. Do not recapture old live settings here.
            ProviderService::migrate_legacy_common_config_usage(
                state,
                AppType::Codex,
                previous["text"].as_str().unwrap_or(""),
            )
            .map_err(|_| "save_outcome_unknown")?;
            state
                .db
                .set_config_snippet(
                    "codex",
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(text.into())
                    },
                )
                .map_err(|_| "save_outcome_unknown")?;
            state
                .db
                .set_config_snippet_cleared("codex", text.trim().is_empty())
                .map_err(|_| "save_outcome_unknown")?;
            ProviderService::sync_current_provider_for_app(state, AppType::Codex)
                .map_err(|_| "save_outcome_unknown")?;
            Ok(json!({"globalApplied":!current.is_empty()}))
        }
        "gui/config/provider/preview" => {
            let (stored, mut base) = gui::edit_base(host, params)?;
            // The native editor starts with the complete current live TOML,
            // not the stripped snapshot used for private provider persistence.
            // During takeover, retain the stored upstream configuration.
            if params.get("configToml").is_none() {
                if let Some(stored) = stored.as_ref() {
                    let current =
                        ProviderService::current(state, AppType::Codex).map_err(service_error)?;
                    if current == stored.id {
                        if let Some(live) = auto_capture::owned_snapshot(state, stored)? {
                            base.settings_config["config"] = live["config"].clone();
                            if params["commonConfigEnabled"] == false
                                && stored.meta.as_ref().and_then(|m| m.common_config_enabled)
                                    == Some(true)
                            {
                                base.settings_config = crate::services::provider::strip_common_config_from_live_settings(
                                    &state.db, &AppType::Codex, stored, base.settings_config.clone());
                            }
                        }
                    }
                }
            }
            let mut provider = form::edit_draft(base, params, true)?;
            provider.settings_config =
                crate::services::provider::build_effective_settings_with_common_config(
                    &state.db,
                    &AppType::Codex,
                    &provider,
                )
                .map_err(service_error)?;
            if let Some(text) = params.get("editedText") {
                form::replace_config(&mut provider, text.as_str().ok_or("invalid_params")?)?;
            }
            let config: toml::Value = provider.settings_config["config"]
                .as_str()
                .unwrap_or("")
                .parse()
                .map_err(|_| "invalid_config")?;
            let (mut base, _) = provider.resolve_usage_credentials(&AppType::Codex);
            let advanced = form::read_advanced(&provider)?;
            if advanced["isFullUrl"] == true
                && matches!(params["kind"].as_str(), Some("responses" | "official_api"))
            {
                base = format!("{}/responses", base.trim_end_matches('/'));
            }
            // Only this opt-in editor returns raw TOML; normal list/get stay masked.
            Ok(
                json!({"text":provider.settings_config["config"], "baseUrl":base,
                "model":config.get("model").and_then(toml::Value::as_str).unwrap_or(""),
                "advanced":form::masked(&advanced)}),
            )
        }
        _ => Err("method_not_found"),
    }
}
