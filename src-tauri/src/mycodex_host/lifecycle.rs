//! Platform policy around the v4 Codex mode services.
//!
//! Outside aggregation MyCodex has no mode switch of its own: a provider that
//! needs protocol conversion runs in v4 proxy mode with itself as the route,
//! every other provider runs direct. In aggregation (v4 Stack mode) the proxy
//! stays on and the provider in use is the default route. Routed requests must
//! carry this store's credential.
use super::session_choices::AfterExit;
use super::{service_error, Result};
use crate::live::engine::DeviceStore;
use crate::live::project::codex::ROUTE_ID;
use crate::mode::current::Purpose;
use crate::mode::state::{Mode, ModeState, PendingTarget};
use crate::{AppState, AppType, Provider, ProviderService};
use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use futures::executor::block_on;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, LazyLock, OnceLock,
};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

// ponytail: one gate per target process; split by app only if this host ever
// manages more than Codex. Read guards live through the complete HTTP body.
static REQUESTS: LazyLock<Arc<RwLock<()>>> = LazyLock::new(|| Arc::new(RwLock::new(())));
static ROUTING: AtomicBool = AtomicBool::new(false);
const ROUTE_HEADER: &str = "x-mycodex-route-token";
static ROUTE_TOKEN: OnceLock<String> = OnceLock::new();

fn initialize_route_token(state: &AppState) -> Result<()> {
    let token = match state
        .db
        .get_setting("mycodex.routeToken")
        .map_err(service_error)?
    {
        Some(token) => token,
        None => {
            let mut bytes = [0u8; 32];
            rustls::crypto::ring::default_provider()
                .secure_random
                .fill(&mut bytes)
                .map_err(|_| "route_credentials_failed")?;
            let token = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            state
                .db
                .set_setting("mycodex.routeToken", &token)
                .map_err(service_error)?;
            token
        }
    };
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid_route_credentials");
    }
    ROUTE_TOKEN
        .set(token)
        .map_err(|_| "runtime_already_initialized")
}

/// The header the v4 proxy route table must carry for this host.
pub(crate) fn route_credential() -> Option<(&'static str, &'static str)> {
    ROUTE_TOKEN
        .get()
        .map(|token| (ROUTE_HEADER, token.as_str()))
}

fn route_token_matches(value: &str) -> bool {
    ROUTE_TOKEN.get().is_some_and(|token| {
        value.len() == token.len()
            && value
                .bytes()
                .zip(token.bytes())
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0
    })
}

fn has_route_header(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .any(|(key, value)| key.eq_ignore_ascii_case(ROUTE_HEADER) || has_route_header(value)),
        toml::Value::Array(values) => values.iter().any(has_route_header),
        _ => false,
    }
}

pub(super) fn mutation() -> Result<OwnedRwLockWriteGuard<()>> {
    REQUESTS
        .clone()
        .try_write_owned()
        .map_err(|_| "route_requests_active")
}

pub(crate) async fn admit(mut request: Request, next: Next) -> Response {
    let Ok(guard) = REQUESTS.clone().try_read_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "configuration_changing").into_response();
    };
    if !ROUTING.load(Ordering::Acquire) {
        return (StatusCode::SERVICE_UNAVAILABLE, "route_not_active").into_response();
    }
    let mut credentials = request.headers().get_all(ROUTE_HEADER).iter();
    let authorized = credentials
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(route_token_matches)
        && credentials.next().is_none();
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "route_unauthorized").into_response();
    }
    // The host capability is never an upstream header or credential.
    request.headers_mut().remove(ROUTE_HEADER);
    // Keep other agents and upstream pass-through endpoints out of this host.
    if !matches!(
        request.uri().path(),
        "/responses"
            | "/v1/responses"
            | "/v1/v1/responses"
            | "/codex/v1/responses"
            | "/responses/compact"
            | "/v1/responses/compact"
            | "/v1/v1/responses/compact"
            | "/codex/v1/responses/compact"
            | "/models"
            | "/v1/models"
            | "/health"
    ) {
        return (StatusCode::NOT_FOUND, "route_not_supported").into_response();
    }
    let response = next.run(request).await;
    response.map(|body| {
        Body::new(body.map_frame(move |frame| {
            let _hold_until_body_drop = &guard;
            frame
        }))
    })
}

/// Validates a Codex provider for this host and reports whether it needs the
/// local conversion route.
pub(super) fn conversion(provider: &Provider) -> Result<bool> {
    use crate::proxy::providers::{
        is_codex_official_provider, should_convert_codex_responses_to_anthropic,
        should_convert_codex_responses_to_chat, CodexAdapter, ProviderAdapter,
    };
    let config = provider.settings_config["config"].as_str().unwrap_or("");
    let parsed: toml::Value = toml::from_str(config).map_err(|_| "invalid_provider")?;
    if has_route_header(&parsed) {
        return Err("reserved_route_header");
    }
    let source = parsed
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    // Use the same URL precedence as actual upstream forwarding.
    let base = CodexAdapter.extract_base_url(provider).ok();
    let official_api = match base.as_deref() {
        None => source == "openai",
        Some(base) => reqwest::Url::parse(base)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| {
                host.eq_ignore_ascii_case("api.openai.com")
                    || host.eq_ignore_ascii_case("chatgpt.com")
            }),
    };
    if provider.settings_config["auth"]["OPENAI_API_KEY"].as_str() == Some("PROXY_MANAGED")
        || crate::codex_config::extract_codex_experimental_bearer_token(config).as_deref()
            == Some("PROXY_MANAGED")
        || crate::codex_config::codex_config_has_official_proxy_route(config)
    {
        return Err("route_takeover_not_supported");
    }
    if let Some(binding) = provider.meta.as_ref().and_then(|m| m.auth_binding.as_ref()) {
        if binding.source == crate::provider::AuthBindingSource::ManagedAccount
            && binding.auth_provider.as_deref() != Some("codex_oauth")
        {
            return Err("unsupported_managed_account");
        }
    }
    if provider.uses_proxy_injected_oauth() {
        return Err("unsupported_managed_account");
    }
    let declared = [
        provider.meta.as_ref().and_then(|m| m.api_format.as_deref()),
        provider.settings_config["api_format"].as_str(),
        provider.settings_config["apiFormat"].as_str(),
    ];
    let mut format = None;
    for value in declared.into_iter().flatten() {
        if !matches!(value, "openai_responses" | "openai_chat" | "anthropic") {
            return Err("unsupported_api_format");
        }
        if format.is_some_and(|previous| previous != value) {
            return Err("conflicting_api_format");
        }
        format = Some(value);
    }
    let convert = should_convert_codex_responses_to_anthropic(provider, "/responses")
        || should_convert_codex_responses_to_chat(provider, "/responses");
    if official_api
        || is_codex_official_provider(provider)
        || provider.category.as_deref() == Some("official")
        || provider.is_codex_oauth()
        || provider
            .meta
            .as_ref()
            .and_then(|m| m.auth_binding.as_ref())
            .is_some_and(|b| b.source == crate::provider::AuthBindingSource::ManagedAccount)
    {
        if convert {
            return Err("official_requires_native_responses");
        }
        return Ok(false);
    }
    Ok(convert)
}

pub(super) fn configure(state: &AppState) -> Result<()> {
    // The private database belongs exclusively to this Codex target. Never run
    // upstream all-app startup/restore, which would inspect unrelated homes.
    for app in ["claude", "gemini", "grokbuild"] {
        let config = block_on(state.db.get_proxy_config_for_app(app)).map_err(service_error)?;
        if config.enabled
            || config.auto_failover_enabled
            || block_on(state.db.get_live_backup(app))
                .map_err(service_error)?
                .is_some()
        {
            return Err("other_app_route_not_supported");
        }
    }
    let app = block_on(state.db.get_proxy_config_for_app("codex")).map_err(service_error)?;
    if app.auto_failover_enabled {
        return Err("automatic_failover_not_supported");
    }
    let mut global = block_on(state.db.get_global_proxy_config()).map_err(service_error)?;
    if global.listen_address != "127.0.0.1" {
        return Err("route_must_be_loopback");
    }
    if state
        .db
        .get_setting("mycodex.routeConfigured")
        .map_err(service_error)?
        .is_none()
    {
        global.listen_port = 0;
        block_on(state.db.update_global_proxy_config(global)).map_err(service_error)?;
        state
            .db
            .set_setting("mycodex.routeConfigured", "1")
            .map_err(service_error)?;
    }
    initialize_route_token(state)
}

pub(super) fn mode() -> ModeState {
    crate::mode::current::mode_state(&AppType::Codex)
}

pub(super) fn is_proxy() -> bool {
    mode().is_proxy()
}

/// Codex is in aggregation (v4 Stack) mode: proxy mode with the list published.
/// The provider in use is then the default route, whatever its conversion need.
pub(super) fn stack_active() -> bool {
    crate::mode::stack::stack_mode_now(&AppType::Codex)
}

/// A provider that may be the aggregation default. An official default runs
/// through the proxy; only the shared-history mirror table can carry this
/// host's route credential, so it requires unified session history.
pub(super) fn check_stack_default(provider: &Provider) -> Result<()> {
    conversion(provider)?;
    if crate::services::provider::codex_direct::is_official(provider)
        && !crate::settings::unify_codex_session_history()
    {
        return Err("stack_official_requires_unified_history");
    }
    Ok(())
}

/// The provider in use needs another mode than the current one. Aggregation
/// keeps the proxy for every default.
pub(super) fn needs_mode_change(provider: &Provider) -> Result<bool> {
    let needs_route = conversion(provider)?;
    Ok(!stack_active() && needs_route != is_proxy())
}

/// Enter aggregation with `provider` as the default.
pub(super) fn enter_stack(state: &AppState, provider: &Provider) -> Result<()> {
    check_stack_default(provider)?;
    super::session_choices::backfill(state)?;
    super::shadowed_settings::around(state, provider, || {
        ROUTING.store(false, Ordering::Release);
        let result = block_on(crate::mode::controller::enter_with_route(
            state,
            &AppType::Codex,
            true,
            Some(&provider.id),
        ))
        .map_err(|_| "stack_enter_failed");
        sync_routing(state);
        result
    })
}

/// Leave aggregation and keep `provider` (the default) in use: routed when it
/// needs conversion, otherwise direct. The list stays for the next time.
pub(super) fn exit_stack(state: &AppState, provider: &Provider) -> Result<()> {
    let needs_route = conversion(provider)?;
    super::session_choices::backfill(state)?;
    super::shadowed_settings::around(state, provider, || {
        ROUTING.store(false, Ordering::Release);
        let result = if needs_route {
            block_on(crate::mode::controller::enter_with_route(
                state,
                &AppType::Codex,
                false,
                Some(&provider.id),
            ))
            .map_err(|_| "stack_exit_failed")
        } else {
            block_on(crate::mode::controller::exit(state, &AppType::Codex))
                .map_err(|_| "stack_exit_failed")
                .and_then(|_| {
                    ProviderService::switch(state, AppType::Codex, &provider.id)
                        .map(|_| ())
                        .map_err(service_error)
                })
        };
        sync_routing(state);
        result
    })
}

/// Accept routed requests only while the client is attached to a running proxy.
pub(super) fn sync_routing(state: &AppState) {
    let attached = mode().attached && block_on(state.proxy_service.is_running());
    ROUTING.store(attached, Ordering::Release);
}

/// Stop accepting routed requests as soon as the client no longer points here.
pub(super) fn pause_if_external() {
    if ROUTING.load(Ordering::Acquire) {
        let owned = super::auto_capture::live_config_text()
            .map(|text| owns_live(&text))
            .unwrap_or(false);
        if !owned {
            ROUTING.store(false, Ordering::Release);
        }
    }
}

/// Codex is attached to this host's proxy: the selected route table is the v4
/// route table and carries exactly this store's credential.
pub(super) fn owns_live(config_text: &str) -> bool {
    if !mode().attached {
        return false;
    }
    let Ok(doc) = toml::from_str::<toml::Value>(config_text) else {
        return false;
    };
    if doc.get("model_provider").and_then(toml::Value::as_str) != Some(ROUTE_ID) {
        return false;
    }
    doc.get("model_providers")
        .and_then(|value| value.get(ROUTE_ID))
        .and_then(|value| value.get("http_headers"))
        .and_then(toml::Value::as_table)
        .is_some_and(|headers| {
            let values = headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(ROUTE_HEADER))
                .collect::<Vec<_>>();
            values.len() == 1 && values[0].1.as_str().is_some_and(route_token_matches)
        })
}

/// Make `provider` the provider in use, choosing proxy or direct mode for it.
/// The session model choices of the provider being left are kept in its row,
/// and user values of exclusive settings it overrides are restored when it is left.
pub(super) fn activate(state: &AppState, provider: &Provider) -> Result<()> {
    super::session_choices::backfill(state)?;
    // The row may have just been updated with the live choices.
    let provider = state
        .db
        .get_provider_by_id(&provider.id, "codex")
        .map_err(service_error)?
        .unwrap_or_else(|| provider.clone());
    activate_saved(state, &provider)
}

/// Like [`activate`] for a row that was just saved: its values are newer than
/// the live file, so nothing is backfilled.
pub(super) fn activate_saved(state: &AppState, provider: &Provider) -> Result<()> {
    super::shadowed_settings::around(state, provider, || switch_to(state, provider))
}

fn switch_to(state: &AppState, provider: &Provider) -> Result<()> {
    let needs_route = conversion(provider)?;
    if stack_active() {
        // Aggregation: the provider becomes the default route and joins the list.
        check_stack_default(provider)?;
        ROUTING.store(false, Ordering::Release);
        let result = ProviderService::switch(state, AppType::Codex, &provider.id)
            .map(|_| ())
            .map_err(|_| "route_start_failed");
        sync_routing(state);
        return result;
    }
    ROUTING.store(false, Ordering::Release);
    let result = if needs_route {
        if mode().is_proxy() {
            ProviderService::switch(state, AppType::Codex, &provider.id)
                .map(|_| ())
                .map_err(|_| "route_start_failed")
        } else {
            block_on(crate::mode::controller::enter_with_route(
                state,
                &AppType::Codex,
                false,
                Some(&provider.id),
            ))
            .map_err(|_| "route_start_failed")
        }
    } else {
        let exited = if mode().is_proxy() {
            block_on(crate::mode::controller::exit(state, &AppType::Codex))
                .map_err(|_| "route_stop_failed")
        } else {
            Ok(())
        };
        exited.and_then(|_| {
            ProviderService::switch(state, AppType::Codex, &provider.id)
                .map(|_| ())
                .map_err(service_error)
        })
    };
    sync_routing(state);
    result
}

/// Rewrite the live route of an official provider in use, whose route depends on
/// the unified session history setting. Third-party routes do not depend on it.
pub(super) fn reapply_official(state: &AppState) -> Result<bool> {
    let Some(provider) = official_in_use(state)? else {
        return Ok(false);
    };
    activate(state, &provider)?;
    Ok(true)
}

/// The Codex provider in use when it is an official (ChatGPT) one.
pub(super) fn official_in_use(state: &AppState) -> Result<Option<Provider>> {
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let provider = state
        .db
        .get_provider_by_id(&current, "codex")
        .map_err(service_error)?;
    Ok(provider.filter(crate::services::provider::codex_direct::is_official))
}

/// After an edit, keep the mode consistent with the provider now in use.
pub(super) fn reconcile(state: &AppState) -> Result<()> {
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let Some(provider) = state
        .db
        .get_provider_by_id(&current, "codex")
        .map_err(service_error)?
    else {
        sync_routing(state);
        return Ok(());
    };
    if needs_mode_change(&provider)? {
        return activate_saved(state, &provider);
    }
    sync_routing(state);
    Ok(())
}

/// The client was switched by someone else while attached: keep the external
/// files and record direct mode without writing them.
pub(super) fn detach_external(state: &AppState) -> Result<()> {
    ROUTING.store(false, Ordering::Release);
    let current = mode();
    if !current.attached && !current.is_proxy() {
        return Ok(());
    }
    let _guard = crate::mode::controller::lock_settled_blocking(state, &AppType::Codex)
        .map_err(service_error)?;
    crate::mode::operation::commit_target(
        &state.db,
        &DeviceStore::for_device(),
        AppType::Codex.as_str(),
        &PendingTarget::mode(ModeState {
            mode: Some(Mode::Direct),
            attached: false,
            proxy_route: current.proxy_route,
            contract: None,
        }),
    )
    .map_err(service_error)?;
    stop_listener(state)
}

// Finish interrupted writes, then settle only this target's Codex mode. Never
// reattach over a client someone else switched while this host was stopped.
pub(super) fn startup(state: &AppState) -> Result<()> {
    {
        let _guard = mutation()?;
        crate::mode::operation::recover_on_startup(&state.db);
        if mode().attached
            && !super::auto_capture::live_config_text()
                .map(|text| owns_live(&text))
                .unwrap_or(false)
        {
            detach_external(state)?;
        }
        // Re-attaching writes the route row's model and reasoning choices.
        let kept = match super::session_choices::after_exit(state)? {
            Some(AfterExit::Unchanged(kept)) => Some(kept),
            // Like a switch made while attached: keep their files, run direct.
            Some(AfterExit::Replaced) => {
                detach_external(state)?;
                None
            }
            None => super::session_choices::keep_routed(),
        };
        let result = block_on(crate::mode::controller::startup_single(
            state,
            &AppType::Codex,
        ))
        .map_err(|_| "mode_startup_failed");
        sync_routing(state);
        result?;
        if let Some(kept) = &kept {
            // The route is up either way; a choice that cannot be restored only
            // falls back to the row's values, as before.
            let _ = super::session_choices::restore_routed(state, kept);
        }
    }
    // An official aggregation default lists the official models; refresh them
    // now and every 15 minutes (nothing happens outside that case).
    crate::services::provider::codex_official_models::start_background_checks(state.clone());
    // Adopt the effective identity once before serving requests.
    super::auto_capture::read(state).map(|_| ())
}

/// Application exit: point Codex back to the provider in use directly, as CC
/// Switch does when it quits. Mode, route and list stay, so the next start
/// attaches again and returns the routed choices. A provider that needs
/// conversion has no direct form and keeps the route.
pub(super) fn detach_for_exit(state: &AppState) -> Result<()> {
    let current = mode();
    if !current.attached || !current.is_proxy() {
        return Ok(());
    }
    let text = super::auto_capture::live_config_text()?;
    // Switched by someone else: the next start keeps their files.
    if !owns_live(&text) {
        return Ok(());
    }
    let Some(route) = current
        .proxy_route
        .as_deref()
        .map(|id| state.db.get_provider_by_id(id, "codex"))
        .transpose()
        .map_err(service_error)?
        .flatten()
    else {
        return Ok(());
    };
    if conversion(&route)? {
        return Ok(());
    }
    let routed = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "invalid_live_config")?;
    super::session_choices::backfill(state)?;
    ROUTING.store(false, Ordering::Release);
    {
        // The direct file is written for the direct pointer; make it the default.
        let _guard = crate::mode::controller::lock_settled_blocking(state, &AppType::Codex)
            .map_err(service_error)?;
        let pointer =
            crate::mode::current::provider_for(&state.db, &AppType::Codex, Purpose::Direct)
                .map_err(service_error)?;
        if pointer.as_deref() != Some(route.id.as_str()) {
            crate::mode::operation::commit_target(
                &state.db,
                &DeviceStore::for_device(),
                AppType::Codex.as_str(),
                &PendingTarget::pointer(Some(route.id.clone())),
            )
            .map_err(service_error)?;
        }
    }
    block_on(crate::mode::controller::detach(state, &AppType::Codex))
        .map_err(|_| "route_detach_failed")?;
    super::session_choices::remember_for_exit(state, &routed)
}

// An explicit maintenance stop must remain possible with malformed live files.
pub(super) fn stop_listener(state: &AppState) -> Result<()> {
    ROUTING.store(false, Ordering::Release);
    if block_on(state.proxy_service.is_running()) {
        block_on(state.proxy_service.stop()).map_err(|_| "route_stop_failed")?;
    }
    Ok(())
}

pub(super) fn status(state: &AppState) -> Result<Value> {
    let status = block_on(state.proxy_service.get_status()).map_err(|_| "route_status_failed")?;
    let mode = mode();
    Ok(json!({
        "mode": match mode.mode {
            Some(Mode::Direct) => json!("direct"),
            Some(Mode::Proxy) => json!("proxy"),
            None => Value::Null,
        },
        "attached": mode.attached,
        "stack": stack_active(),
        // Codex is routed through this host; restarting the host must resume it.
        "takeover": mode.attached,
        "running": status.running,
        "accepting": ROUTING.load(Ordering::Acquire),
        "port": status.port,
        "activeConnections": status.active_connections,
    }))
}
