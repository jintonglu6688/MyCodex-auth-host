//! Platform policy around original Codex provider/proxy services.
use super::{service_error, Host, Result};
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

// Called only at the original takeover projection boundary, after its backup
// and provider snapshots have been prepared. Never replace PROXY_MANAGED: the
// core uses that sentinel to prevent a proxy credential becoming an API key.
pub(crate) fn project_route_auth(config: &str) -> Result<String> {
    let token = ROUTE_TOKEN.get().ok_or("route_credentials_missing")?;
    let mut doc = config
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "invalid_live_config")?;
    let source = doc
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("openai")
        .to_string();
    let providers = doc
        .entry("model_providers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or("invalid_live_config")?;
    let provider = providers
        .entry(&source)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or("invalid_live_config")?;
    let headers = provider
        .entry("http_headers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or("invalid_live_config")?;
    let reserved = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(ROUTE_HEADER))
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>();
    for key in reserved {
        headers.remove(&key);
    }
    headers.insert(ROUTE_HEADER, toml_edit::value(token.as_str()));
    Ok(doc.to_string())
}

pub(crate) fn remove_route_auth(config: &str) -> Result<String> {
    fn remove(table: &mut dyn toml_edit::TableLike) {
        let keys = table
            .iter()
            .map(|(key, _)| key.to_string())
            .collect::<Vec<_>>();
        for key in keys {
            if key.eq_ignore_ascii_case(ROUTE_HEADER) {
                table.remove(&key);
            } else if let Some(child) = table
                .get_mut(&key)
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                remove(child);
            }
        }
    }
    let mut doc = config
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "invalid_live_config")?;
    remove(doc.as_table_mut());
    Ok(doc.to_string())
}

fn live_route_token_matches(config: &str) -> bool {
    let Ok(doc) = toml::from_str::<toml::Value>(config) else {
        return false;
    };
    let source = doc
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    doc.get("model_providers")
        .and_then(|value| value.get(source))
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
        return Err("route_takeover_not_supported_in_probe");
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

fn takeover(state: &AppState) -> Result<bool> {
    let config = block_on(state.db.get_proxy_config_for_app("codex")).map_err(service_error)?;
    Ok(config.enabled
        || block_on(state.db.get_live_backup("codex"))
            .map_err(service_error)?
            .is_some()
        || state
            .proxy_service
            .detect_takeover_in_live_config_for_app(&AppType::Codex))
}

pub(super) fn stop(state: &AppState) -> Result<()> {
    ROUTING.store(false, Ordering::Release);
    if takeover(state)? {
        require_owned_live(state)?;
        // Unlike the global recovery helpers, this original helper only touches
        // Codex and also handles interrupted activation where enabled=false.
        let _guard = block_on(state.proxy_service.lock_switch_for_app("codex"));
        state
            .proxy_service
            .disable_takeover_for_app_sync(&AppType::Codex)
            .map_err(|_| "route_restore_failed")?;
    }
    if block_on(state.proxy_service.is_running()) {
        block_on(state.proxy_service.stop()).map_err(|_| "route_stop_failed")?;
    }
    let mut global = block_on(state.db.get_global_proxy_config()).map_err(service_error)?;
    if global.proxy_enabled {
        global.proxy_enabled = false;
        block_on(state.db.update_global_proxy_config(global)).map_err(service_error)?;
    }
    Ok(())
}

// Capture outgoing live edits before save/reapply or proxy backup restoration.
// Call only after request checks and before changing the provider's opt-in flag.
pub(super) fn capture_current_common(state: &AppState) -> Result<()> {
    let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let Some(provider) = state
        .db
        .get_provider_by_id(&current, "codex")
        .map_err(service_error)?
    else {
        return Ok(());
    };
    if provider.meta.as_ref().and_then(|m| m.common_config_enabled) != Some(true) {
        return Ok(());
    }
    let routed = takeover(state)?;
    if routed {
        let accepting = ROUTING.load(Ordering::Acquire);
        require_owned_live(state)?;
        ROUTING.store(accepting, Ordering::Release);
    }
    let live =
        ProviderService::read_live_settings(AppType::Codex).map_err(|_| "invalid_live_config")?;
    let previous = state
        .db
        .get_config_snippet("codex")
        .map_err(service_error)?;
    // Original extraction excludes provider routes, credentials, and MCP. Read
    // actual live tool preferences even during takeover, not its older backup.
    let mut result = crate::services::provider::SwitchResult::default();
    ProviderService::sync_common_config_snippet_from_live(
        state,
        &AppType::Codex,
        &provider,
        &live,
        &mut result,
    );
    if !result.warnings.is_empty() {
        return Err("save_outcome_unknown");
    }
    if routed
        && previous
            != state
                .db
                .get_config_snippet("codex")
                .map_err(|_| "save_outcome_unknown")?
    {
        // A direct switch restores backup before native switching. Refresh it
        // with the original effective-provider builder so that restoration and
        // its subsequent common capture cannot resurrect stale tool settings.
        block_on(
            state
                .proxy_service
                .update_live_backup_from_provider("codex", &provider),
        )
        .map_err(|_| "save_outcome_unknown")?;
    }
    Ok(())
}

pub(super) fn prepare(state: &AppState, provider: &Provider) -> Result<()> {
    if takeover(state)? && !require_owned_live(state)? {
        stop(state)?;
    }
    if !conversion(provider)? {
        stop(state)?;
    }
    Ok(())
}

pub(super) fn reconcile(state: &AppState) -> Result<()> {
    ROUTING.store(false, Ordering::Release);
    if takeover(state)? {
        require_owned_live(state)?;
    }
    let id = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
    let provider = state
        .db
        .get_provider_by_id(&id, "codex")
        .map_err(service_error)?;
    if provider
        .as_ref()
        .map(conversion)
        .transpose()?
        .unwrap_or(false)
    {
        // Failure must never leave a live listener forwarding to a direct card.
        ROUTING.store(false, Ordering::Release);
        if block_on(state.proxy_service.set_takeover_for_app("codex", true)).is_err() {
            // Original activation owns its rollback. Stop a leftover listener;
            // preserve any backup if recovery itself failed for later repair.
            if block_on(state.proxy_service.is_running()) {
                block_on(state.proxy_service.stop()).map_err(|_| "route_stop_failed")?;
            }
            return Err("route_start_failed");
        }
        ROUTING.store(true, Ordering::Release);
    } else {
        stop(state)?;
    }
    Ok(())
}

pub(super) fn restore(host: &Host) -> Result<()> {
    let state = &host.state;
    let _guard = mutation()?;
    if !takeover(state)? {
        return Ok(());
    }
    // Never overwrite an external direct switch with our old backup on launch.
    if require_owned_live(state)? {
        reconcile(state)
    } else {
        stop(state)
    }
}

fn require_owned_live(state: &AppState) -> Result<bool> {
    ROUTING.store(false, Ordering::Release);
    let live =
        ProviderService::read_live_settings(AppType::Codex).map_err(|_| "invalid_live_config")?;
    let marked = state
        .proxy_service
        .detect_takeover_in_live_config_for_app(&AppType::Codex);
    if marked
        && block_on(
            state
                .proxy_service
                .live_takeover_matches_current_proxy(&AppType::Codex),
        )
        .map_err(|_| "invalid_live_config")?
    {
        if live
            .get("config")
            .and_then(Value::as_str)
            .is_some_and(live_route_token_matches)
        {
            return Ok(true);
        }
        return Err("route_live_changed_externally");
    }
    // Stop may have restored the exact backup before a crash interrupted flag
    // cleanup. Finish cleanup without automatically re-enabling that route.
    if let Some(backup) = block_on(state.db.get_live_backup("codex")).map_err(service_error)? {
        let backup: Value =
            serde_json::from_str(&backup.original_config).map_err(|_| "invalid_route_backup")?;
        let config = |v: &Value| toml::from_str::<toml::Value>(v["config"].as_str().unwrap_or(""));
        if live["auth"] == backup["auth"]
            && config(&live)
                .ok()
                .zip(config(&backup).ok())
                .is_some_and(|(a, b)| a == b)
        {
            return Ok(false);
        }
    } else if !marked {
        // Crash after deleting the restored backup but before clearing enabled.
        // The original single-app helper skips live writes without takeover.
        return Ok(false);
    }
    Err("route_live_changed_externally")
}

pub(super) fn status(state: &AppState) -> Result<Value> {
    let status = block_on(state.proxy_service.get_status()).map_err(|_| "route_status_failed")?;
    Ok(
        json!({"running":status.running,"accepting":ROUTING.load(Ordering::Acquire),
        "port":status.port,"activeConnections":status.active_connections,
        "takeover":takeover(state)?}),
    )
}
