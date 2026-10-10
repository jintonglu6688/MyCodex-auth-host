//! Explicit provider TOML editor; native services still own storage and live projection.
//!
//! v4 rows hold only a provider's key fields. Global settings stay in the live
//! file, so the editor shows and edits the row's own TOML.
use super::{gui, gui_provider as form, Host, Result};
use crate::AppType;
use serde_json::{json, Value};

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    match method {
        "gui/config/provider/preview" => {
            let (_, base) = gui::edit_base(host, params)?;
            let mut provider = form::edit_draft(base, params, true)?;
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
        _ => Err("method_not_supported"),
    }
}
