//! Provider-exclusive settings such as `model_verbosity` belong to the provider
//! whose row sets them: v4 overwrites the live value when switching to that
//! provider and removes it again when switching away. MyCodex official settings
//! are written to the same keys in config.toml, so a value the user chose
//! before would be lost. Remember it while a provider shadows it and restore it
//! when the provider is left; a change the user makes in the meantime wins.
use super::{runtime_paths, Result};
use crate::codex_config::get_codex_config_path;
use crate::live::project::codex::{CodexProjection, RowInput};
use crate::provider::Provider;
use crate::services::provider::claude_editor::ConflictPolicy;
use crate::services::provider::codex_direct;
use crate::services::provider::codex_editor::{self, KeyFields};
use crate::services::provider::editor_toml::{render, Entry, TomlEdits};
use crate::services::provider::ProviderService;
use crate::store::AppState;
use crate::AppType;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item};

const FILE: &str = "shadowed-settings.json";

/// A user value hidden by a provider, both rendered with [`render`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Shadow {
    user: String,
    applied: String,
}

fn path() -> Result<PathBuf> {
    Ok(runtime_paths()
        .ok_or("runtime_not_initialized")?
        .data_dir
        .join(FILE))
}

fn load() -> BTreeMap<String, Shadow> {
    path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store(shadows: &BTreeMap<String, Shadow>) -> Result<()> {
    let path = path()?;
    if shadows.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err("store_write_failed"),
            _ => Ok(()),
        };
    }
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(shadows).map_err(|_| "store_write_failed")?;
    std::fs::write(&temporary, bytes).map_err(|_| "store_write_failed")?;
    std::fs::rename(&temporary, &path).map_err(|_| "store_write_failed")
}

fn live() -> Option<DocumentMut> {
    std::fs::read_to_string(get_codex_config_path())
        .ok()?
        .parse()
        .ok()
}

fn live_value(doc: Option<&DocumentMut>, key: &str) -> Option<String> {
    doc?.get(key).filter(|item| item.is_value()).map(render)
}

/// The exclusive settings a provider row writes, rendered for comparison.
fn exclusive(provider: &Provider) -> BTreeMap<String, String> {
    CodexProjection::of(&RowInput {
        settings: &provider.settings_config,
        official: codex_direct::is_official(provider),
        proxy_injected_oauth: provider.uses_proxy_injected_oauth(),
    })
    .map(|projection| {
        projection
            .exclusive
            .into_iter()
            .map(|(key, value)| (key, render(&Item::Value(value))))
            .collect()
    })
    .unwrap_or_default()
}

fn current_exclusive(state: &AppState) -> BTreeMap<String, String> {
    ProviderService::current(state, AppType::Codex)
        .ok()
        .filter(|id| !id.is_empty())
        .and_then(|id| state.db.get_provider_by_id(&id, "codex").ok().flatten())
        .map(|provider| exclusive(&provider))
        .unwrap_or_default()
}

/// Run a write that makes `target` the live provider, keeping user values it shadows.
pub(super) fn around<T>(
    state: &AppState,
    target: &Provider,
    write: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let incoming = exclusive(target);
    let outgoing = current_exclusive(state);
    let before = live();
    let mut shadows = load();
    // The user changed a shadowed setting while the provider was active: their choice wins.
    shadows
        .retain(|key, shadow| live_value(before.as_ref(), key).as_ref() == Some(&shadow.applied));
    for (key, value) in &incoming {
        if shadows.contains_key(key) {
            continue;
        }
        let Some(user) = live_value(before.as_ref(), key) else {
            continue;
        };
        // A value the current provider wrote is not the user's.
        if &user != value && outgoing.get(key) != Some(&user) {
            shadows.insert(
                key.clone(),
                Shadow {
                    user,
                    applied: value.clone(),
                },
            );
        }
    }
    let result = write()?;
    restore(state, &incoming, &mut shadows)?;
    store(&shadows)?;
    Ok(result)
}

fn restore(
    state: &AppState,
    incoming: &BTreeMap<String, String>,
    shadows: &mut BTreeMap<String, Shadow>,
) -> Result<()> {
    let after = live();
    let mut entries = Vec::new();
    shadows.retain(|key, shadow| {
        if let Some(value) = incoming.get(key) {
            // Still shadowed, possibly by a different value.
            shadow.applied = value.clone();
            return live_value(after.as_ref(), key).as_ref() == Some(value);
        }
        if live_value(after.as_ref(), key).is_none() {
            if let Ok(doc) = shadow.user.parse::<DocumentMut>() {
                if let Some(item) = doc.get("v") {
                    entries.push(Entry {
                        path: vec![key.clone()],
                        item: item.clone(),
                    });
                }
            }
        }
        false
    });
    if entries.is_empty() {
        return Ok(());
    }
    // Something written there meanwhile is kept rather than overwritten.
    let edits = TomlEdits::between(&[], &entries, ConflictPolicy::KeepTheirs);
    let _settled = crate::mode::controller::lock_settled_blocking(state, &AppType::Codex)
        .map_err(|_| "save_outcome_unknown")?;
    codex_editor::write_live(
        state.db.as_ref(),
        &state.codex_oauth_manager,
        &edits,
        KeyFields::None,
    )
    .map_err(|_| "save_outcome_unknown")
}
