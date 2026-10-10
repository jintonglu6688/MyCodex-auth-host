//! Platform policy around the v4 Codex mode services.
//!
//! Phase 1 of docs/mycodex-auth-v4-port.md keeps the route credential, request
//! gate and target checks. Route activation moves onto the v4 mode controller in
//! phase 2; until then the gate rejects every routed request.
use super::{service_error, Result};
use crate::mode::state::Mode;
use crate::{AppState, AppType};
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

// Finish interrupted writes, then settle only this target's Codex mode.
pub(super) fn startup(state: &AppState) -> Result<()> {
    let _guard = mutation()?;
    crate::mode::operation::recover_on_startup(&state.db);
    block_on(crate::mode::controller::startup_single(
        state,
        &AppType::Codex,
    ))
    .map_err(|_| "mode_startup_failed")
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
    let mode = crate::mode::current::mode_state(&AppType::Codex);
    Ok(json!({
        "mode": match mode.mode {
            Some(Mode::Direct) => json!("direct"),
            Some(Mode::Proxy) => json!("proxy"),
            None => Value::Null,
        },
        "attached": mode.attached,
        "running": status.running,
        "accepting": ROUTING.load(Ordering::Acquire),
        "port": status.port,
        "activeConnections": status.active_connections,
    }))
}
