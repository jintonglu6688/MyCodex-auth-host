//! Adopt the effective global identity before reading or changing providers.
//! This module writes only the target's private stores, never Codex live files.
use super::{gui_provider, lifecycle, service_error, Result};
use crate::{codex_config as codex, AppState, AppType, McpService, Provider, ProviderService};
use futures::executor::block_on;
use serde_json::{json, Value};

mod diagnostics;
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
    lifecycle::pause_if_external();
    let _guard = match lifecycle::mutation() {
        Ok(guard) => guard,
        Err(error) => return Ok(Outcome::unavailable(error)),
    };
    reconcile(state, "read")
}

pub(super) fn before_write(state: &AppState) -> Result<()> {
    let previous = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let result = reconcile(state, "before_write")?;
    if let Some(error) = result.error {
        return Err(error);
    }
    if previous != result.current {
        return Err("config_conflict");
    }
    Ok(())
}

// Only a missing external environment reference can be replaced explicitly.
// Invalid files, account ownership, profiles and store errors remain blocking.
pub(super) fn before_write_or_credential_recovery(state: &AppState) -> Result<bool> {
    match before_write(state) {
        Ok(()) => Ok(false),
        Err("credential_reference_unavailable") => Ok(true),
        Err(error) => Err(error),
    }
}

pub(super) fn backup_for_credential_recovery(fingerprint: &str) -> Result<()> {
    for path in [codex::get_codex_config_path(), codex::get_codex_auth_path()] {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err("invalid_live_config");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err("invalid_live_config");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("invalid_live_config"),
        }
    }
    let snapshot = Snapshot::read()?;
    let backup = json!({
        "config":snapshot.config.as_deref().map(String::from_utf8_lossy),
        "auth":snapshot.auth.as_deref().map(String::from_utf8_lossy),
    });
    let path = super::runtime_paths()
        .ok_or("runtime_not_initialized")?
        .data_dir
        .join(format!("credentials-before-recovery-{fingerprint}.json"));
    let bytes = serde_json::to_vec(&backup).map_err(|_| "config_backup_failed")?;
    crate::config::stage_write(&path, &bytes, Some(0o600), true)
        .and_then(|file| file.commit())
        .map_err(|_| "config_backup_failed")?;
    snapshot.unchanged()
}

// The caller owns lifecycle::mutation. Recoverable live errors leave the archive readable.
fn reconcile(state: &AppState, source: &'static str) -> Result<Outcome> {
    let mut trace = diagnostics::Trace::new(source);
    let result = capture(state, &mut trace);
    trace.finish(&result);
    match result {
        Ok(result) => Ok(result),
        Err("database_failed" | "account_store_invalid") => Err("account_store_invalid"),
        Err(error) => Ok(Outcome::unavailable(error)),
    }
}

pub(super) fn live_config_text() -> Result<String> {
    match std::fs::read(codex::get_codex_config_path()) {
        Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 => {
            String::from_utf8(bytes).map_err(|_| "invalid_live_config")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        _ => Err("invalid_live_config"),
    }
}

/// The live config text when it belongs to `provider` as the direct provider in
/// use. Routed clients hold the proxy contract, not the provider's own values.
pub(super) fn live_overlay(state: &AppState, provider: &Provider) -> Result<Option<String>> {
    if ProviderService::current(state, AppType::Codex).map_err(service_error)? != provider.id {
        return Ok(None);
    }
    let snapshot = Snapshot::read()?;
    let text = snapshot.live["config"].as_str().unwrap_or("").to_string();
    if lifecycle::owns_live(&text) {
        return Ok(None);
    }
    let Some(identity) = identity::identify(&snapshot.live)? else {
        return Ok(None);
    };
    if !identity::matches(state, provider, &identity)? {
        return Ok(None);
    }
    snapshot.unchanged()?;
    Ok(Some(text))
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
    // Both the provider in use and the direct pointer already name `id`.
    if ProviderService::current(state, AppType::Codex).map_err(service_error)? == id
        && crate::mode::current::provider_for(
            &state.db,
            &AppType::Codex,
            crate::mode::current::Purpose::Direct,
        )
        .map_err(service_error)?
        .unwrap_or_default()
            == id
    {
        return Ok(());
    }
    // Serialize with v4 switches; the pointer changes without writing client files.
    let _guard = crate::mode::controller::lock_settled_blocking(state, &AppType::Codex)
        .map_err(service_error)?;
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

fn capture(state: &AppState, trace: &mut diagnostics::Trace) -> Result<Outcome> {
    let snapshot = Snapshot::read()?;
    let previous = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    trace.previous_ref = diagnostics::reference(&previous);
    trace.stage = "check_route_owner";
    if lifecycle::owns_live(snapshot.live["config"].as_str().unwrap_or("")) {
        let provider = gui_provider::stored(state, &previous)?;
        if !lifecycle::conversion(&provider)? {
            return Err("route_live_changed_externally");
        }
        trace.kind = "routed";
        trace.branch = "owned_route";
        trace.provider_ref = diagnostics::reference(&previous);
        lifecycle::sync_routing(state);
        trace.stage = "complete";
        return Ok(Outcome {
            current: previous,
            state: "current",
            error: None,
        });
    }
    trace.stage = "identify_live";
    let detected = identity::identify(&snapshot.live)?;
    snapshot.unchanged()?;
    // No restore: the external files are authoritative, including signed-out state.
    trace.stage = "detach_external";
    lifecycle::detach_external(state)?;
    let Some(detected) = detected else {
        snapshot.unchanged()?;
        trace.kind = "signed_out";
        trace.branch = "signed_out";
        trace.stage = "set_current";
        set_current(state, "")?;
        trace.current_set = true;
        trace.stage = "complete";
        return Ok(Outcome {
            current: String::new(),
            state: "signed_out",
            error: None,
        });
    };
    trace.kind = if detected.chatgpt { "chatgpt" } else { "api" };
    trace.stage = "match_providers";
    let providers = ProviderService::list(state, AppType::Codex).map_err(service_error)?;
    trace.provider_count = Some(providers.len());
    let mut candidates = Vec::new();
    let mut exact = Vec::new();
    for provider in providers.values() {
        if !lifecycle::conversion(provider)? && identity::matches(state, provider, &detected)? {
            candidates.push(provider.clone());
            if identity::same_settings(provider, &snapshot.live)? {
                exact.push(provider.id.clone());
            }
        }
    }
    trace.candidate_count = Some(candidates.len());
    trace.exact_count = Some(exact.len());
    exact.sort();
    let chosen = if exact.iter().any(|id| id == &previous) {
        trace.branch = "exact_previous";
        Some(previous.clone())
    } else if !exact.is_empty() {
        trace.branch = "exact_match";
        exact.first().cloned()
    } else if candidates.iter().any(|p| p.id == previous) {
        trace.branch = "identity_previous";
        Some(previous.clone())
    } else if candidates.len() == 1 {
        trace.branch = "single_identity";
        Some(candidates[0].id.clone())
    } else {
        // Several variants of a known identity are still an existing account.
        // Use a stable fallback, as with ties in exact matches above. Capture
        // preserves each card's private settings and never rewrites live files.
        trace.branch = if candidates.is_empty() {
            "new_identity"
        } else {
            "identity_fallback"
        };
        candidates.iter().map(|provider| provider.id.clone()).min()
    };
    trace.provider_ref = chosen.as_deref().and_then(diagnostics::reference);
    trace.stage = "import_account";
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
    trace.account_imported = account.is_some();
    trace.account_ref = account.as_deref().and_then(diagnostics::reference);
    snapshot.unchanged()?;
    trace.stage = "prepare_provider";
    let created = chosen.is_none();
    let mut provider = match chosen {
        Some(id) => gui_provider::stored(state, &id)?,
        None => {
            // v4 rows hold key fields; everything else stays in the live file.
            let config = identity::projection_text(&snapshot.live, detected.chatgpt)
                .ok_or("invalid_live_config")?;
            let mut provider = Provider::with_id(
                uuid::Uuid::new_v4().to_string(),
                detected.name.clone(),
                json!({"auth":detected.saved_auth,"config":config}),
                None,
            );
            let meta = provider.meta.get_or_insert_with(Default::default);
            meta.api_format = Some("openai_responses".into());
            provider
        }
    };
    trace.provider_ref = diagnostics::reference(&provider.id);
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
    }
    trace.stage = "initialize_capture";
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
    snapshot.unchanged()?;
    trace.stage = "save_provider";
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
        trace.provider_saved = true;
    }
    snapshot.unchanged()?;
    trace.stage = "set_current";
    set_current(state, &provider.id)?;
    trace.current_set = true;
    trace.stage = "record_auth_owner";
    if let Some(account) = account {
        if !codex::codex_auth_matches_recorded_managed_oauth(&snapshot.live["auth"], &account)
            .map_err(|_| "capture_incomplete")?
        {
            codex::record_codex_managed_oauth_live_auth(&snapshot.live["auth"], &account)
                .map_err(|_| "capture_incomplete")?;
        }
    }
    snapshot.unchanged()?;
    trace.stage = "complete";
    Ok(Outcome {
        current: provider.id,
        state: "current",
        error: None,
    })
}
