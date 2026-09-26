//! Adopt the effective global identity before native snapshots or switching.
//! This module writes only the target's private stores, never Codex live files.
use super::{gui_provider, lifecycle, service_error, Result};
use crate::{codex_config as codex, AppState, AppType, McpService, Provider, ProviderService};
use futures::executor::block_on;
use serde_json::{json, Value};

mod identity;

pub(super) struct Outcome {
    pub current: String,
    pub state: &'static str,
    pub error: Option<&'static str>,
}

impl Outcome {
    fn unavailable(error: &'static str) -> Self {
        Self {
            current: String::new(),
            state: "unavailable",
            error: Some(error),
        }
    }
}

// Do not cache credentials or infer ownership from the last successful read.
pub(super) fn read(state: &AppState) -> Result<Outcome> {
    lifecycle::pause_if_external(state);
    let _guard = match lifecycle::mutation() {
        Ok(guard) => guard,
        Err(error) => return Ok(Outcome::unavailable(error)),
    };
    reconcile(state)
}

pub(super) fn before_write(state: &AppState) -> Result<()> {
    let previous = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let result = reconcile(state)?;
    if let Some(error) = result.error {
        return Err(error);
    }
    if previous != result.current {
        return Err("config_conflict");
    }
    Ok(())
}

// The caller owns lifecycle::mutation. Recoverable live errors leave the archive readable.
pub(super) fn reconcile(state: &AppState) -> Result<Outcome> {
    match capture(state) {
        Ok(result) => Ok(result),
        Err("database_failed" | "account_store_invalid") => Err("account_store_invalid"),
        Err(error) => Ok(Outcome::unavailable(error)),
    }
}

struct Snapshot {
    auth: Option<Vec<u8>>,
    config: Option<Vec<u8>>,
    live: Value,
}

impl Snapshot {
    fn read() -> Result<Self> {
        fn bytes(path: std::path::PathBuf) -> Result<Option<Vec<u8>>> {
            match std::fs::read(path) {
                Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err("invalid_live_config"),
            }
        }
        let auth = bytes(codex::get_codex_auth_path())?;
        let config = bytes(codex::get_codex_config_path())?;
        let value: Value = match &auth {
            Some(bytes) => serde_json::from_slice(bytes).map_err(|_| "invalid_live_config")?,
            None => json!({}),
        };
        if !value.is_object() {
            return Err("invalid_live_config");
        }
        let text = match &config {
            Some(bytes) => std::str::from_utf8(bytes).map_err(|_| "invalid_live_config")?,
            None => "",
        };
        let _: toml::Table = toml::from_str(text).map_err(|_| "invalid_live_config")?;
        let live = json!({"auth":value,"config":text});
        Ok(Self { auth, config, live })
    }

    fn unchanged(&self) -> Result<()> {
        let next = Self::read()?;
        if next.auth != self.auth || next.config != self.config {
            return Err("config_conflict");
        }
        Ok(())
    }
}

fn set_current(state: &AppState, id: &str) -> Result<()> {
    if ProviderService::current(state, AppType::Codex).map_err(service_error)? == id
        && state
            .db
            .get_current_provider("codex")
            .map_err(service_error)?
            .unwrap_or_default()
            == id
    {
        return Ok(());
    }
    state
        .db
        .set_current_provider("codex", id)
        .map_err(service_error)?;
    crate::settings::set_current_provider(
        &AppType::Codex,
        if id.is_empty() { None } else { Some(id) },
    )
    .map_err(service_error)
}

fn capture(state: &AppState) -> Result<Outcome> {
    let snapshot = Snapshot::read()?;
    let previous = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    if lifecycle::owns_snapshot(state, &snapshot.live)? {
        let provider = gui_provider::stored(state, &previous)?;
        if !lifecycle::conversion(&provider)? {
            return Err("route_live_changed_externally");
        }
        lifecycle::resume_owned(state)?;
        return Ok(Outcome {
            current: previous,
            state: "current",
            error: None,
        });
    }
    let detected = identity::identify(&snapshot.live)?;
    snapshot.unchanged()?;
    // No restore: the external files are authoritative, including signed-out state.
    lifecycle::detach_external(state)?;
    let Some(detected) = detected else {
        snapshot.unchanged()?;
        set_current(state, "")?;
        return Ok(Outcome {
            current: String::new(),
            state: "signed_out",
            error: None,
        });
    };
    let providers = ProviderService::list(state, AppType::Codex).map_err(service_error)?;
    let mut candidates = Vec::new();
    let mut exact = Vec::new();
    for provider in providers.values() {
        if !lifecycle::conversion(provider)? && identity::matches(state, provider, &detected)? {
            candidates.push(provider.clone());
            if identity::same_settings(state, provider, &snapshot.live)? {
                exact.push(provider.id.clone());
            }
        }
    }
    exact.sort();
    let chosen = if exact.iter().any(|id| id == &previous) {
        Some(previous.clone())
    } else if !exact.is_empty() {
        exact.first().cloned()
    } else if candidates.iter().any(|p| p.id == previous) {
        Some(previous.clone())
    } else if candidates.len() == 1 {
        Some(candidates[0].id.clone())
    } else {
        None
    };
    let account = if detected.chatgpt {
        Some(block_on(state.codex_oauth_manager.import_existing_login(&snapshot.live["auth"]))
            .map_err(|error| match error {
                crate::proxy::providers::codex_oauth_auth::CodexOAuthError::ExistingLoginInvalid => "incomplete_account_credentials",
                crate::proxy::providers::codex_oauth_auth::CodexOAuthError::ExistingLoginConflict => "credential_generation_conflict",
                _ => "account_write_failed",
            })?)
    } else {
        None
    };
    snapshot.unchanged()?;
    let created = chosen.is_none();
    let mut provider = match chosen {
        Some(id) => gui_provider::stored(state, &id)?,
        None => {
            let mut provider = Provider::with_id(
                uuid::Uuid::new_v4().to_string(),
                detected.name.clone(),
                snapshot.live.clone(),
                None,
            );
            let meta = provider.meta.get_or_insert_with(Default::default);
            meta.common_config_enabled = Some(true);
            meta.api_format = Some("openai_responses".into());
            provider
        }
    };
    if let Some(account) = &account {
        if created {
            if let Some(saved) = block_on(state.codex_oauth_manager.list_accounts())
                .iter()
                .find(|a| &a.id == account)
            {
                provider.name = format!("ChatGPT ({})", saved.login);
            }
        }
        let meta = provider.meta.get_or_insert_with(Default::default);
        meta.auth_binding = Some(crate::provider::AuthBinding {
            source: crate::provider::AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".into()),
            account_id: Some(account.clone()),
        });
        meta.provider_type = Some("codex_oauth".into());
        provider.category = Some("official".into());
        provider.settings_config["auth"] = json!({});
    } else if created {
        provider.settings_config["auth"] = detected.saved_auth.clone();
    }
    // Only initialization imports MCP; ordinary list refresh is not an MCP sync.
    if state
        .db
        .get_setting("mycodex.captureInitialized")
        .map_err(service_error)?
        .is_none()
    {
        if providers.is_empty() {
            McpService::import_from_codex(state).map_err(service_error)?;
        }
        snapshot.unchanged()?;
        state
            .db
            .set_setting("mycodex.captureInitialized", "1")
            .map_err(service_error)?;
    }
    if created {
        let mut result = crate::services::provider::SwitchResult::default();
        ProviderService::sync_common_config_snippet_from_live(
            state,
            &AppType::Codex,
            &provider,
            &snapshot.live,
            &mut result,
        );
        if !result.warnings.is_empty() {
            return Err("save_outcome_unknown");
        }
        provider.settings_config =
            crate::services::provider::strip_common_config_from_live_settings(
                &state.db,
                &AppType::Codex,
                &provider,
                provider.settings_config.clone(),
            );
        codex::strip_codex_mcp_servers_from_settings(&mut provider.settings_config)
            .map_err(service_error)?;
    }
    snapshot.unchanged()?;
    // Saving this row via DAO deliberately bypasses add's first-provider activation.
    if created
        || providers
            .get(&provider.id)
            .is_some_and(|p| json!(p.meta) != json!(provider.meta))
    {
        state
            .db
            .save_provider("codex", &provider)
            .map_err(service_error)?;
    }
    snapshot.unchanged()?;
    set_current(state, &provider.id)?;
    if let Some(account) = account {
        if !codex::codex_auth_matches_recorded_managed_oauth(&snapshot.live["auth"], &account)
            .map_err(|_| "capture_incomplete")?
        {
            codex::record_codex_managed_oauth_live_auth(&snapshot.live["auth"], &account)
                .map_err(|_| "capture_incomplete")?;
        }
    }
    snapshot.unchanged()?;
    Ok(Outcome {
        current: provider.id,
        state: "current",
        error: None,
    })
}

// Called inside the native snapshot path too, so no caller can attach B's live to A.
pub(super) fn owned_snapshot(state: &AppState, provider: &Provider) -> Result<Option<Value>> {
    if lifecycle::owns_live(state)? {
        return Ok(None);
    }
    let snapshot = Snapshot::read()?;
    let Some(identity) = identity::identify(&snapshot.live)? else {
        return Ok(None);
    };
    if !identity::matches(state, provider, &identity)? {
        return Ok(None);
    }
    snapshot.unchanged()?;
    Ok(Some(snapshot.live))
}

pub(crate) fn require_live_owner(
    state: &AppState,
    provider: &Provider,
    live: &Value,
) -> Result<()> {
    if lifecycle::owns_snapshot(state, live)? {
        return if ProviderService::current(state, AppType::Codex).map_err(service_error)?
            == provider.id
            && lifecycle::conversion(provider)?
        {
            Ok(())
        } else {
            Err("config_conflict")
        };
    }
    // Native takeover-off restores a provider-built backup verbatim. It may
    // hold the key in auth before the normal writer projects a bearer token.
    // Accept only an equivalent real upstream identity, never a proxy marker.
    if lifecycle::conversion(provider)? {
        let mut effective = live.clone();
        effective["config"] = json!(codex::prepare_codex_provider_live_config(
            &live["auth"],
            live["config"].as_str().ok_or("invalid_live_config")?
        )
        .map_err(service_error)?);
        let Some(identity) = identity::identify(&effective)? else {
            return Err("config_conflict");
        };
        return if identity::matches(state, provider, &identity)? {
            Ok(())
        } else {
            Err("config_conflict")
        };
    }
    let Some(identity) = identity::identify(live)? else {
        return Err("config_conflict");
    };
    if identity::matches(state, provider, &identity)? {
        Ok(())
    } else {
        Err("config_conflict")
    }
}
