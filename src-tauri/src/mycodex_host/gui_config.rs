//! Explicit provider TOML editor; native services still own storage and live projection.
//!
//! v4 rows hold only a provider's key fields. `preview` edits the row's own
//! TOML. With `fullConfigEditor`, `view` shows the complete `config.toml` the
//! provider would produce; saving splits it with v4's editor so global changes
//! are written by three-way merge and never overwrite keys the user left alone.
use super::{gui, gui_provider as form, Host, Result};
use crate::error::AppError;
use crate::live::patch::EDIT_CONFLICT_CODE;
use crate::provider::Provider;
use crate::services::provider::claude_editor::ConflictPolicy;
use crate::services::provider::codex_direct;
use crate::services::provider::codex_editor::{self, CodexEditorPlan, KeyFields, Origin};
use crate::services::provider::editor_toml::TomlEdits;
use crate::services::provider::ProviderService;
use crate::store::AppState;
use crate::AppType;
use serde_json::{json, Value};

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    match method {
        "gui/config/provider/view" => {
            let state = &host.state;
            let (stored, base) = gui::edit_base(host, params)?;
            let provider = form::edit_draft(base, params, true)?;
            let category = ProviderService::editor_category(
                state,
                &AppType::Codex,
                stored.as_ref().map(|row| row.id.as_str()),
                provider.category.clone(),
            )
            .map_err(|_| "invalid_config")?;
            let view = ProviderService::editor_view_with_meta(
                state,
                AppType::Codex,
                &provider.settings_config,
                category.as_deref(),
                None,
                provider.meta.as_ref(),
            )
            .map_err(|_| "invalid_config")?;
            let inactive: Vec<Value> = view
                .inactive
                .iter()
                .map(|field| json!({"path":field.path.join("."),"value":field.value}))
                .collect();
            // Only this opt-in editor returns raw TOML; normal list/get stay masked.
            Ok(json!({"text":view.settings["config"],"inactive":inactive}))
        }
        "gui/config/provider/preview" => {
            let (stored, base) = gui::edit_base(host, params)?;
            let mut provider = form::edit_draft(base, params, true)?;
            let mut global = Vec::new();
            if let Some(editor_base) = params.get("editorBase") {
                let text = params.get("editedText").ok_or("invalid_params")?;
                let plan = plan(
                    stored.as_ref(),
                    &provider,
                    editor_base,
                    text,
                    ConflictPolicy::Refuse,
                )?;
                global = plan.edits.paths();
                let row = plan.row_settings["config"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                form::replace_config(&mut provider, &row)?;
            } else if let Some(text) = params.get("editedText") {
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
            Ok(
                json!({"text":provider.settings_config["config"], "baseUrl":base,
                "model":config.get("model").and_then(toml::Value::as_str).unwrap_or(""),
                "advanced":form::masked(&advanced), "globalChanges":global}),
            )
        }
        _ => Err("method_not_supported"),
    }
}

/// Split a complete editor text against the text shown when the editor opened.
/// Key and provider-exclusive fields go to the row; the rest are global edits.
pub(super) fn plan(
    stored: Option<&Provider>,
    draft: &Provider,
    base: &Value,
    text: &Value,
    on_conflict: ConflictPolicy,
) -> Result<CodexEditorPlan> {
    let base = base.as_str().ok_or("invalid_params")?;
    let text = text.as_str().ok_or("invalid_params")?;
    form::validate_toml(base)?;
    form::validate_toml(text)?;
    let mut edited = draft.settings_config.clone();
    edited["config"] = json!(text);
    let origin =
        Origin::row(&stored.unwrap_or(draft).settings_config).map_err(|_| "invalid_config")?;
    codex_editor::plan_save(
        stored.map(|row| &row.settings_config),
        &edited,
        &json!({"config":base}),
        &origin,
        codex_direct::is_official(draft),
        draft.uses_proxy_injected_oauth(),
        on_conflict,
    )
    .map_err(|_| "invalid_config")
}

/// Write the editor's global edits. A refused conflict writes nothing and
/// returns the conflicting keys so the user can choose which side to keep.
pub(super) fn write_global(
    state: &AppState,
    edits: &TomlEdits,
) -> Result<std::result::Result<(), Vec<String>>> {
    let _settled = crate::mode::controller::lock_settled_blocking(state, &AppType::Codex)
        .map_err(|_| "save_outcome_unknown")?;
    match codex_editor::write_live(
        state.db.as_ref(),
        &state.codex_oauth_manager,
        edits,
        KeyFields::None,
    ) {
        Ok(()) => Ok(Ok(())),
        Err(AppError::Message(message)) => match serde_json::from_str::<Value>(&message) {
            Ok(error) if error["code"] == EDIT_CONFLICT_CODE => Ok(Err(error["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect())),
            _ => Err("save_outcome_unknown"),
        },
        Err(_) => Err("save_outcome_unknown"),
    }
}
