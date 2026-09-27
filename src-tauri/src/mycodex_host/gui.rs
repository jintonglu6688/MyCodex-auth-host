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

fn fingerprint(host: &Host) -> Result<String> {
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
        "current":ProviderService::current(state,AppType::Codex).map_err(service_error)?,
        "common":state.db.get_config_snippet("codex").map_err(service_error)?})))
}

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    let state = &host.state;
    match method {
        "gui/preset/list" => gui_catalog::presets(),
        "gui/model/fetch" => gui_catalog::fetch_models(state, params),
        method if method.starts_with("gui/mcp/") => super::gui_mcp::handle(host, method, params),
        "gui/provider/list" => {
            let captured = auto_capture::read(state)?;
            let current = captured.current;
            let mut current_version = String::new();
            let mut rows = Vec::new();
            for stored in ProviderService::list(state, AppType::Codex)
                .map_err(service_error)?
                .values()
            {
                let edited = if current == stored.id && captured.error.is_none() {
                    form::snapshot(state, stored)?
                } else {
                    stored.clone()
                };
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
            let _guard = lifecycle::mutation()?;
            auto_capture::before_write(state)?;
            let id = params["id"].as_str().ok_or("invalid_params")?;
            valid_id(id)?;
            let previous = state
                .db
                .get_provider_by_id(id, "codex")
                .map_err(service_error)?;
            let edited = if let Some(stored) = &previous {
                let edited = form::snapshot(state, stored)?;
                if params["expectedVersion"].as_str()
                    != Some(form::version(stored, &edited).as_str())
                {
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
            let mut edited = form::edit(edited, params)?;
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
            lifecycle::conversion(&edited)?;
            let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
            let applied = current == id || (previous.is_none() && current.is_empty());
            if current == id {
                if let Some(stored) = &previous {
                    lifecycle::capture_current_common(state)?;
                    // Strip the newly captured common values before applying the
                    // requested flag, including true -> false and false -> true.
                    edited = form::edit(form::snapshot(state, stored)?, params)?;
                }
            }
            if applied {
                if current.is_empty() {
                    McpService::import_from_codex(state).map_err(service_error)?;
                }
                lifecycle::prepare(state, &edited)?;
            }
            let result = if previous.is_some() {
                ProviderService::update(state, AppType::Codex, None, edited)
            } else {
                ProviderService::add(state, AppType::Codex, edited, false)
            };
            if current.is_empty() && result.is_ok() {
                McpService::sync_enabled_for_app(state, &AppType::Codex)
                    .map_err(|_| "save_outcome_unknown")?;
            }
            if result.is_err() && applied {
                lifecycle::stop_listener(state).map_err(|_| "save_outcome_unknown")?;
            }
            // A failed native write must never reapply an old conversion row.
            result.map_err(|_| "save_outcome_unknown")?;
            let reconciled = if applied {
                lifecycle::reconcile(state)
            } else {
                Ok(())
            };
            // A native write may have occurred before either error; never advertise safe retry.
            reconciled.map_err(|_| "save_outcome_unknown")?;
            let stored = form::stored(state, id).map_err(|_| "save_outcome_unknown")?;
            let edited = form::snapshot(state, &stored).map_err(|_| "save_outcome_unknown")?;
            let mut result =
                form::summary(state, &stored, &edited).map_err(|_| "save_outcome_unknown")?;
            result["globalApplied"] = json!(applied);
            Ok(result)
        }
        "gui/provider/copy" => {
            let _guard = lifecycle::mutation()?;
            auto_capture::before_write(state)?;
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
            ProviderService::add(state, AppType::Codex, edited, false)
                .map_err(|_| "save_outcome_unknown")?;
            let stored = form::stored(state, id)?;
            form::summary(state, &stored, &stored)
        }
        "gui/provider/delete" => {
            let _guard = lifecycle::mutation()?;
            auto_capture::before_write(state)?;
            let (stored, _) = selected(host, params, true)?;
            if ProviderService::current(state, AppType::Codex).map_err(service_error)? == stored.id
                || state
                    .db
                    .get_current_provider("codex")
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
            auto_capture::before_write(state)?;
            let (stored, edited) = selected(host, params, true)?;
            let route_required = lifecycle::conversion(&edited)?;
            Ok(
                json!({"providerId":stored.id,"version":form::version(&stored,&edited),"fingerprint":fingerprint(host)?,"routeRequired":route_required}),
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
            auto_capture::before_write(state)?;
            let (stored, edited) = selected(host, params, true)?;
            if params["expectedFingerprint"].as_str() != Some(fingerprint(host)?.as_str()) {
                return Err("config_conflict");
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
            lifecycle::capture_current_common(state)?;
            lifecycle::prepare(state, &edited)?;
            let result = ProviderService::switch(state, AppType::Codex, &stored.id);
            if result.is_err() {
                lifecycle::stop_listener(state).map_err(|_| "operation_unknown")?;
            }
            result.map_err(|_| "operation_unknown")?;
            lifecycle::reconcile(state).map_err(|_| "operation_unknown")?;
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
