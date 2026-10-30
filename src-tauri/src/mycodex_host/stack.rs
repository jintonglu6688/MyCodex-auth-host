//! Aggregation: the v4 Stack mode for Codex.
//!
//! The listed providers publish their models under reserved `ccs-<key>/<model>`
//! ids in one model catalog; a request for such a model goes straight to that
//! provider, other requests to the default (the proxy route). The native mode
//! controller owns the list, the key registry and the client files; this module
//! only checks what MyCodex supports and projects the state for the GUI.
use super::{auto_capture, gui::valid_id, lifecycle, service_error, Host, Result};
use crate::mode::{controller, current};
use crate::services::provider::codex_direct;
use crate::{settings, AppState, AppType, Provider, ProviderService};
use futures::executor::block_on;
use serde_json::{json, Value};

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    match method {
        "gui/stack/get" => view(&host.state),
        "gui/stack/enter" => enter(&host.state, params),
        "gui/stack/exit" => exit(&host.state),
        "gui/stack/member/set" => set_member(&host.state, params),
        "gui/stack/catalog/adopt" => adopt_catalog(&host.state),
        "gui/stack/settings/set" => set_settings(&host.state, params),
        _ => Err("method_not_supported"),
    }
}

/// Why a provider cannot be listed, or `None` when it can. An official account
/// can only be the default; its login is the client's own.
pub(super) fn join_refusal(provider: &Provider) -> Option<&'static str> {
    if codex_direct::is_official(provider) {
        return Some("stack_official_cannot_join");
    }
    if let Err(code) = lifecycle::conversion(provider) {
        return Some(code);
    }
    codex_direct::check_stack_member(provider)
        .err()
        .map(|_| "invalid_provider")
}

fn provider(state: &AppState, params: &Value) -> Result<Provider> {
    let id = params["providerId"].as_str().ok_or("invalid_params")?;
    valid_id(id)?;
    state
        .db
        .get_provider_by_id(id, "codex")
        .map_err(service_error)?
        .ok_or("provider_not_found")
}

fn view(state: &AppState) -> Result<Value> {
    let stack = controller::stack_views(state, &AppType::Codex).map_err(|_| "stack_read_failed")?;
    let mode = current::mode_state(&AppType::Codex);
    let default = mode.proxy_route.clone().filter(|_| stack.active);
    let members: Vec<Value> = stack
        .members
        .iter()
        .map(|member| {
            json!({
                "providerId": member.provider_id,
                "modelIds": member.model_ids,
                "isDefault": default.as_deref() == Some(member.provider_id.as_str()),
            })
        })
        .collect();
    let mut providers = Vec::new();
    for row in ProviderService::list(state, AppType::Codex)
        .map_err(service_error)?
        .values()
    {
        providers.push(json!({
            "providerId": row.id,
            "joinRefusal": join_refusal(row),
            "defaultRefusal": lifecycle::check_stack_default(row).err(),
        }));
    }
    Ok(json!({
        "active": stack.active,
        "defaultProviderId": default,
        "members": members,
        "providers": providers,
        "notice": stack.notice,
        "classicSubagents": settings::codex_stack_classic_tools(),
        "unifiedSessionHistory": settings::unify_codex_session_history(),
    }))
}

fn with_applied(state: &AppState, applied: bool) -> Result<Value> {
    let mut result = view(state)?;
    // The client reads its model catalog at start, so a rewrite needs a reload.
    result["globalApplied"] = json!(applied);
    Ok(result)
}

fn write_member(state: &AppState, id: &str, enabled: bool) -> Result<()> {
    block_on(controller::set_stack_member(
        state,
        &AppType::Codex,
        id,
        enabled,
    ))
    .map(|_| ())
    .map_err(|error| {
        if error.partial {
            "stack_write_partial"
        } else {
            "stack_operation_failed"
        }
    })
}

/// Enter aggregation with a default and, optionally, more listed providers.
/// Outside the proxy the list is only stored, so it is written before entering
/// and the client files are rewritten once.
fn enter(state: &AppState, params: &Value) -> Result<Value> {
    let _guard = lifecycle::mutation()?;
    auto_capture::before_write(state)?;
    if lifecycle::stack_active() {
        return Err("stack_already_active");
    }
    let default = {
        let id = params["defaultProviderId"]
            .as_str()
            .ok_or("invalid_params")?;
        provider(state, &json!({ "providerId": id }))?
    };
    lifecycle::check_stack_default(&default)?;
    let mut members = Vec::new();
    if let Some(ids) = params.get("memberIds") {
        for id in ids.as_array().ok_or("invalid_params")? {
            let row = provider(state, &json!({ "providerId": id }))?;
            if row.id == default.id {
                continue;
            }
            if join_refusal(&row).is_some() {
                return Err("stack_member_rejected");
            }
            members.push(row.id);
        }
    }
    for id in &members {
        write_member(state, id, true)?;
    }
    lifecycle::enter_stack(state, &default)?;
    with_applied(state, true)
}

fn exit(state: &AppState) -> Result<Value> {
    let _guard = lifecycle::mutation()?;
    auto_capture::before_write(state)?;
    if !lifecycle::stack_active() {
        return with_applied(state, false);
    }
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let default = state
        .db
        .get_provider_by_id(&current, "codex")
        .map_err(service_error)?
        .ok_or("provider_not_found")?;
    lifecycle::exit_stack(state, &default)?;
    with_applied(state, true)
}

fn set_member(state: &AppState, params: &Value) -> Result<Value> {
    let enabled = params["enabled"].as_bool().ok_or("invalid_params")?;
    let _guard = lifecycle::mutation()?;
    auto_capture::before_write(state)?;
    let id = params["providerId"].as_str().ok_or("invalid_params")?;
    valid_id(id)?;
    if enabled {
        let row = provider(state, params)?;
        if let Some(code) = join_refusal(&row) {
            return Err(code);
        }
    } else if lifecycle::stack_active() && current::mode_state(&AppType::Codex).routes_to(id) {
        return Err("stack_default_cannot_be_removed");
    }
    write_member(state, id, enabled)?;
    lifecycle::sync_routing(state);
    with_applied(state, lifecycle::stack_active())
}

/// The default row points at its own catalog file, which hides the aggregated
/// models: drop that pointer and write the generated catalog.
fn adopt_catalog(state: &AppState) -> Result<Value> {
    let _guard = lifecycle::mutation()?;
    auto_capture::before_write(state)?;
    block_on(controller::adopt_codex_stack_catalog(state)).map_err(|_| "stack_operation_failed")?;
    lifecycle::sync_routing(state);
    with_applied(state, lifecycle::stack_active())
}

fn set_settings(state: &AppState, params: &Value) -> Result<Value> {
    let classic = params["classicSubagents"]
        .as_bool()
        .ok_or("invalid_params")?;
    let _guard = lifecycle::mutation()?;
    auto_capture::before_write(state)?;
    let existing = settings::get_settings();
    if existing.codex_stack_classic_tools == classic {
        return with_applied(state, false);
    }
    let mut next = existing.clone();
    next.codex_stack_classic_tools = classic;
    settings::update_settings(next).map_err(service_error)?;
    if block_on(controller::resync_codex_stack_catalog(state)).is_err() {
        let _ = settings::update_settings(existing);
        let _ = block_on(controller::resync_codex_stack_catalog(state));
        return Err("stack_operation_failed");
    }
    lifecycle::sync_routing(state);
    with_applied(state, lifecycle::stack_active())
}
