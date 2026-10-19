//! GUI RPC projection. Native services own persistence and switching semantics.
use super::{
    auto_capture, gui_accounts, gui_catalog, gui_provider as form, lifecycle, service_error, Host,
    Result,
};
use crate::{AppType, McpService, Provider, ProviderService};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
pub(super) struct Session {
    operations: BTreeMap<String, (String, Option<Value>)>,
    operation_order: VecDeque<String>,
    pub logins: BTreeMap<String, gui_accounts::Login>,
}

pub(super) fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("invalid_params");
    }
    Ok(())
}

fn selected(host: &Host, params: &Value, check: bool) -> Result<(Provider, Provider)> {
    let id = params["providerId"].as_str().ok_or("invalid_params")?;
    valid_id(id)?;
    let stored = form::stored(&host.state, id)?;
    let edited = form::snapshot(&host.state, &stored)?;
    if check && params["expectedVersion"].as_str() != Some(form::version(&stored, &edited).as_str())
    {
        return Err("version_conflict");
    }
    Ok((stored, edited))
}

pub(super) fn fingerprint(host: &Host) -> Result<String> {
    let state = &host.state;
    let mut files = Vec::new();
    for path in [
        crate::codex_config::get_codex_config_path(),
        crate::codex_config::get_codex_auth_path(),
    ] {
        match std::fs::read(path) {
            Ok(bytes) => files.push(json!(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => files.push(Value::Null),
            Err(_) => return Err("invalid_live_config"),
        }
    }
    Ok(form::hash(&json!({"files":files,
        "current":ProviderService::current(state,AppType::Codex).map_err(service_error)?})))
}

pub(super) fn edit_base(host: &Host, params: &Value) -> Result<(Option<Provider>, Provider)> {
    let state = &host.state;
    let id = params["id"].as_str().ok_or("invalid_params")?;
    valid_id(id)?;
    let previous = state
        .db
        .get_provider_by_id(id, "codex")
        .map_err(service_error)?;
    let edited = if let Some(stored) = &previous {
        let edited = form::snapshot(state, stored)?;
        if params["expectedVersion"].as_str() != Some(form::version(stored, &edited).as_str()) {
            return Err("version_conflict");
        }
        edited
    } else {
        if params
            .get("expectedVersion")
            .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        {
            return Err("version_conflict");
        }
        let mut provider =
            gui_catalog::native_preset(params["presetId"].as_str().unwrap_or("custom"))?
                .unwrap_or_else(|| {
                    Provider::with_id(
                        id.into(),
                        String::new(),
                        json!({"auth":{},"config":""}),
                        None,
                    )
                });
        provider.id = id.into();
        provider
    };
    Ok((previous, edited))
}

/// The lock a provider write holds. Without routed requests it is the usual
/// `mutation`; while they stream, only rows that are not in use can be stored,
/// and only when the client is still on this host's unchanged route.
enum WriteGate {
    Exclusive(#[allow(dead_code)] tokio::sync::OwnedRwLockWriteGuard<()>),
    Beside(#[allow(dead_code)] lifecycle::BesideRequests),
}

impl WriteGate {
    fn enter(state: &crate::AppState) -> Result<Self> {
        match lifecycle::mutation() {
            Ok(guard) => Ok(Self::Exclusive(guard)),
            Err(error) => {
                let beside = lifecycle::beside_requests().map_err(|_| error)?;
                auto_capture::steady_route(state)?.ok_or(error)?;
                Ok(Self::Beside(beside))
            }
        }
    }

    fn beside(&self) -> bool {
        matches!(self, Self::Beside(_))
    }

    /// The steady route needs no capture, and no credential recovery applies to it.
    fn recovery(&self, state: &crate::AppState) -> Result<bool> {
        if self.beside() {
            return Ok(false);
        }
        auto_capture::before_write_or_credential_recovery(state)
    }
}

/// Saving this row would change what the client uses: the provider in use, the
/// direct provider restored on exit, the first provider, or an aggregated one.
fn in_use(state: &crate::AppState, id: &str) -> Result<bool> {
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    if current.is_empty() || current == id {
        return Ok(true);
    }
    let direct = crate::mode::current::provider_for(
        &state.db,
        &AppType::Codex,
        crate::mode::current::Purpose::Direct,
    )
    .map_err(service_error)?;
    if direct.as_deref() == Some(id) {
        return Ok(true);
    }
    if lifecycle::stack_active() {
        let stack = crate::mode::controller::stack_views(state, &AppType::Codex)
            .map_err(|_| "stack_read_failed")?;
        return Ok(stack.members.iter().any(|member| member.provider_id == id));
    }
    Ok(false)
}

// Persist an edited row. A row in use whose route need no longer matches the
// mode is stored first, then switched into the right mode.
fn persist(host: &Host, previous: Option<&Provider>, edited: Provider) -> Result<bool> {
    let state = &host.state;
    let id = edited.id.clone();
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let applied = current == id || (previous.is_none() && current.is_empty());
    let mode_change = lifecycle::needs_mode_change(&edited)?;
    if current.is_empty() && previous.is_none() {
        McpService::import_from_codex(state).map_err(service_error)?;
    }
    if applied && mode_change {
        state
            .db
            .save_provider("codex", &edited)
            .map_err(|_| "save_outcome_unknown")?;
        lifecycle::activate_saved(state, &edited).map_err(|_| "save_outcome_unknown")?;
    } else {
        // A routed host has no direct pointer, so native add would treat any new
        // row as the first provider and write it live. Inactive rows are stored only.
        let target = edited.clone();
        let write = || {
            let result = if previous.is_some() {
                ProviderService::update(state, AppType::Codex, None, edited)
            } else if applied {
                ProviderService::add(state, AppType::Codex, edited, false)
            } else {
                state.db.save_provider("codex", &edited).map(|_| true)
            };
            // A native write may have occurred before an error; never advertise safe retry.
            result.map_err(|_| "save_outcome_unknown")?;
            lifecycle::reconcile(state).map_err(|_| "save_outcome_unknown")
        };
        if applied {
            super::shadowed_settings::around(state, &target, write)?;
        } else {
            write()?;
        }
    }
    if current.is_empty() {
        McpService::sync_enabled_for_app(state, &AppType::Codex)
            .map_err(|_| "save_outcome_unknown")?;
    }
    Ok(applied)
}

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    let state = &host.state;
    match method {
        method if method.starts_with("gui/config/") => {
            super::gui_config::handle(host, method, params)
        }
        method if method.starts_with("gui/stack/") => super::stack::handle(host, method, params),
        "gui/preset/list" => gui_catalog::presets(),
        "gui/model/fetch" => gui_catalog::fetch_models(state, params),
        method if method.starts_with("gui/mcp/") => super::gui_mcp::handle(host, method, params),
        method if method.starts_with("gui/sessionHistory/") => {
            super::session_history::handle(host, method, params)
        }
        "gui/provider/list" => {
            let captured = auto_capture::read(state)?;
            let current = captured.current;
            let mut current_version = String::new();
            let mut rows = Vec::new();
            for stored in ProviderService::list(state, AppType::Codex)
                .map_err(service_error)?
                .values()
            {
                let edited = form::snapshot(state, stored)?;
                let row = form::summary(state, stored, &edited)?;
                if stored.id == current {
                    current_version = form::version(stored, &edited);
                }
                rows.push(row);
            }
            let account_label = rows
                .iter()
                .find(|row| row["id"].as_str() == Some(current.as_str()))
                .and_then(|row| row.get("accountLabel"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(
                json!({"providers":rows,"currentProviderId":current,"currentProviderVersion":current_version,
                "liveState":captured.state,"liveAccountLabel":account_label,"syncError":captured.error,
                "route":lifecycle::status(state)?}),
            )
        }
        "gui/provider/get" => {
            auto_capture::read(state)?;
            let (stored, edited) = selected(host, params, false)?;
            form::summary(state, &stored, &edited)
        }
        "gui/provider/save" => {
            let id = params["id"].as_str().ok_or("invalid_params")?;
            valid_id(id)?;
            // The caller cannot reconnect now: store the row only if that changes nothing in use.
            let inactive_only = params["inactiveOnly"].as_bool() == Some(true);
            let gate = WriteGate::enter(state)?;
            if (inactive_only || gate.beside()) && in_use(state, id)? {
                return Err(if inactive_only {
                    "runtime_busy"
                } else {
                    "route_requests_active"
                });
            }
            let recovery = gate.recovery(state)?;
            let (previous, edited) = edit_base(host, params)?;
            let edited = form::edit(edited, params)?;
            if let Some(account) = edited
                .meta
                .as_ref()
                .and_then(|m| m.auth_binding.as_ref())
                .and_then(|b| b.account_id.as_ref())
            {
                if !futures::executor::block_on(state.codex_oauth_manager.list_accounts())
                    .iter()
                    .any(|a| &a.id == account)
                {
                    return Err("account_not_found");
                }
            }
            // The full config editor's global edits are written first: a refused
            // conflict then leaves both the row and config.toml untouched.
            let mut global_written = false;
            if let Some(editor) = params.get("editor") {
                let on_conflict = serde_json::from_value(
                    editor.get("onConflict").cloned().unwrap_or(json!("refuse")),
                )
                .map_err(|_| "invalid_params")?;
                let plan = super::gui_config::plan(
                    previous.as_ref(),
                    &edited,
                    &editor["base"],
                    &editor["text"],
                    on_conflict,
                )?;
                if !plan.edits.is_empty() {
                    if inactive_only || gate.beside() {
                        return Err(if inactive_only {
                            "runtime_busy"
                        } else {
                            "route_requests_active"
                        });
                    }
                    if recovery {
                        return Err("credential_recovery_confirmation_required");
                    }
                    if let Err(keys) = super::gui_config::write_global(state, &plan.edits)? {
                        return Ok(json!({"editorConflict":{"keys":keys}}));
                    }
                    global_written = true;
                }
            }
            let applied = if recovery {
                lifecycle::conversion(&edited)?;
                // No first-provider activation, MCP sync, or live update while
                // the existing identity cannot be safely captured.
                state
                    .db
                    .save_provider("codex", &edited)
                    .map_err(|_| "save_outcome_unknown")?;
                false
            } else {
                persist(host, previous.as_ref(), edited)?
            };
            let stored = form::stored(state, id).map_err(|_| "save_outcome_unknown")?;
            let edited = form::snapshot(state, &stored).map_err(|_| "save_outcome_unknown")?;
            let mut result =
                form::summary(state, &stored, &edited).map_err(|_| "save_outcome_unknown")?;
            // Global settings changed in config.toml also need the client to reload.
            result["globalApplied"] = json!(applied || global_written);
            result["recoveryRequired"] = json!(recovery);
            Ok(result)
        }
        "gui/provider/copy" => {
            // A copy is never in use, so it can be stored beside routed requests.
            let gate = WriteGate::enter(state)?;
            if !gate.beside() {
                auto_capture::before_write(state)?;
            }
            let (_, mut edited) = selected(host, params, true)?;
            let id = params["newId"].as_str().ok_or("invalid_params")?;
            let name = params["name"].as_str().ok_or("invalid_params")?;
            valid_id(id)?;
            if name.trim().is_empty() || name.len() > 256 {
                return Err("invalid_params");
            }
            if state
                .db
                .get_provider_by_id(id, "codex")
                .map_err(service_error)?
                .is_some()
            {
                return Err("version_conflict");
            }
            edited.id = id.into();
            edited.name = name.into();
            edited.created_at = None;
            edited.sort_index = None;
            lifecycle::conversion(&edited)?;
            // A copy is never in use; store the row without native activation.
            state
                .db
                .save_provider("codex", &edited)
                .map_err(|_| "save_outcome_unknown")?;
            let stored = form::stored(state, id)?;
            form::summary(state, &stored, &stored)
        }
        "gui/provider/delete" => {
            let _guard = lifecycle::mutation()?;
            auto_capture::before_write(state)?;
            let (stored, _) = selected(host, params, true)?;
            // Neither the provider in use nor the direct pointer restored on exit.
            if ProviderService::current(state, AppType::Codex).map_err(service_error)? == stored.id
                || crate::mode::current::provider_for(
                    &state.db,
                    &AppType::Codex,
                    crate::mode::current::Purpose::Direct,
                )
                .map_err(service_error)?
                .as_deref()
                    == Some(&stored.id)
            {
                return Err("provider_in_use");
            }
            ProviderService::delete(state, AppType::Codex, &stored.id).map_err(service_error)?;
            Ok(json!({"accountRemoved":false}))
        }
        "gui/provider/preflight" => {
            let _guard = lifecycle::mutation()?;
            let recovery = auto_capture::before_write_or_credential_recovery(state)?;
            let (stored, edited) = selected(host, params, true)?;
            let route_required = lifecycle::conversion(&edited)?;
            Ok(
                json!({"providerId":stored.id,"version":form::version(&stored,&edited),"fingerprint":fingerprint(host)?,"routeRequired":route_required,"recoveryRequired":recovery}),
            )
        }
        "gui/provider/apply" => {
            let _guard = lifecycle::mutation()?;
            let operation = params["operationId"].as_str().ok_or("invalid_params")?;
            valid_id(operation)?;
            let request_hash = form::hash(params);
            {
                let session = host.gui.lock().map_err(|_| "session_failed")?;
                if let Some((previous, response)) = session.operations.get(operation) {
                    if previous != &request_hash {
                        return Err("operation_id_conflict");
                    }
                    return response.clone().ok_or("operation_unknown");
                }
            }
            let recovery = auto_capture::before_write_or_credential_recovery(state)?;
            let (stored, mut edited) = selected(host, params, true)?;
            if params["expectedFingerprint"].as_str() != Some(fingerprint(host)?.as_str()) {
                return Err("config_conflict");
            }
            if recovery {
                if params["confirmCredentialRecovery"].as_bool() != Some(true) {
                    return Err("credential_recovery_confirmation_required");
                }
                lifecycle::conversion(&edited)?;
                let before = params["expectedFingerprint"]
                    .as_str()
                    .ok_or("invalid_params")?;
                auto_capture::backup_for_credential_recovery(before)?;
                if fingerprint(host)? != before {
                    return Err("config_conflict");
                }
            }
            {
                let mut session = host.gui.lock().map_err(|_| "session_failed")?;
                // Management requests are serialized: older attempts have all finished.
                // ponytail: retain only 128 receipts; evicted/restarted results are unknown.
                if session.operations.len() >= 128 {
                    if let Some(old) = session.operation_order.pop_front() {
                        session.operations.remove(&old);
                    }
                }
                session.operation_order.push_back(operation.into());
                session
                    .operations
                    .insert(operation.into(), (request_hash, None));
            }
            if form::clear_chatgpt_catalog(&mut edited)? {
                ProviderService::update(state, AppType::Codex, None, edited.clone())
                    .map_err(|_| "operation_unknown")?;
            }
            if recovery {
                // The live identity is external and cannot be backfilled into
                // the previously selected row or restored on proxy exit.
                lifecycle::detach_external(state).map_err(|_| "operation_unknown")?;
                lifecycle::activate_saved(state, &edited).map_err(|_| "operation_unknown")?;
            } else {
                lifecycle::activate(state, &edited).map_err(|_| "operation_unknown")?;
            }
            let response =
                json!({"operationId":operation,"providerId":stored.id,"status":"applied"});
            if let Some((_, receipt)) = host
                .gui
                .lock()
                .map_err(|_| "operation_unknown")?
                .operations
                .get_mut(operation)
            {
                *receipt = Some(response.clone());
            }
            Ok(response)
        }
        "gui/operation/get" => {
            let id = params["operationId"].as_str().ok_or("invalid_params")?;
            valid_id(id)?;
            host.gui
                .lock()
                .map_err(|_| "session_failed")?
                .operations
                .get(id)
                .and_then(|(_, v)| v.clone())
                .ok_or("operation_unknown")
        }
        method if method.starts_with("gui/account/") => gui_accounts::handle(host, method, params),
        _ => Err("method_not_supported"),
    }
}
