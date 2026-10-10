//! Model and reasoning choices made in Codex or MyCodex session settings.
//!
//! They are written to the live config.toml, but v4 treats these keys as
//! provider fields and rewrites them from the provider row on every switch
//! without reading them back. As in the 3.x core, the choices made while a
//! provider is in use are backfilled into its row before switching away, so
//! returning to it restores them. Rows are updated only while the live file
//! belongs to that provider (its identity, or our route token when routed).
use super::{auto_capture, lifecycle, service_error, Result};
use crate::provider::Provider;
use crate::services::provider::ProviderService;
use crate::store::AppState;
use crate::AppType;
use serde_json::json;
use toml_edit::{DocumentMut, Item};

const KEYS: [&str; 4] = [
    "model",
    "model_reasoning_effort",
    "plan_mode_reasoning_effort",
    "review_model",
];

/// The live config text while it belongs to `provider` as the provider in use.
pub(super) fn owned_live(state: &AppState, provider: &Provider) -> Result<Option<String>> {
    if let Some(text) = auto_capture::live_overlay(state, provider)? {
        return Ok(Some(text));
    }
    if lifecycle::is_proxy()
        && ProviderService::current(state, AppType::Codex).map_err(service_error)? == provider.id
    {
        let text = auto_capture::live_config_text()?;
        if lifecycle::owns_live(&text) {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

/// Copy the live choices into a row's TOML. Returns whether anything changed.
pub(super) fn apply(row: &mut DocumentMut, live: &DocumentMut) -> bool {
    let mut changed = false;
    for key in KEYS {
        match live.get(key).and_then(Item::as_value) {
            Some(value) => {
                let mut value = value.clone();
                value.decor_mut().clear();
                let differs = row
                    .get(key)
                    .and_then(Item::as_value)
                    .is_none_or(|stored| stored.to_string().trim() != value.to_string().trim());
                if differs {
                    row[key] = Item::Value(value);
                    changed = true;
                }
            }
            // A missing model keeps the saved one; other fields follow the live file.
            None if key != "model" => changed |= row.remove(key).is_some(),
            None => (),
        }
    }
    changed
}

/// Before switching away, store the current provider's live choices in its row.
pub(super) fn backfill(state: &AppState) -> Result<()> {
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    if current.is_empty() {
        return Ok(());
    }
    let Some(mut provider) = state
        .db
        .get_provider_by_id(&current, "codex")
        .map_err(service_error)?
    else {
        return Ok(());
    };
    let Some(live) = owned_live(state, &provider)? else {
        return Ok(());
    };
    let Ok(live) = live.parse::<DocumentMut>() else {
        return Ok(());
    };
    let mut row = provider.settings_config["config"]
        .as_str()
        .unwrap_or("")
        .parse::<DocumentMut>()
        .map_err(|_| "invalid_config")?;
    if apply(&mut row, &live) {
        provider.settings_config["config"] = json!(row.to_string());
        state
            .db
            .save_provider("codex", &provider)
            .map_err(service_error)?;
    }
    Ok(())
}
