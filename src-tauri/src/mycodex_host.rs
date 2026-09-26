//! Minimal Codex-only stdio adapter over the original service layer.
//! No desktop bootstrap or parallel provider persistence engine.
use crate::{AppState, AppType, Database, McpService, Provider, ProviderService};
use serde::Deserialize;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

mod accounts;
mod gui;
mod gui_accounts;
mod gui_catalog;
mod gui_mcp;
mod gui_provider;
mod identity;
mod ipc;
pub(crate) mod lifecycle;
mod security;

type Result<T> = std::result::Result<T, &'static str>;
const MAX_REQUEST: usize = 1_048_576;
const UPSTREAM: &str = "e0f70019b2758f5b6b9a04dd60e4689481a0c0ac";

pub(crate) struct RuntimePaths {
    pub data_dir: PathBuf,
    pub codex_home: PathBuf,
}

static PATHS: OnceLock<RuntimePaths> = OnceLock::new();

pub(crate) fn runtime_paths() -> Option<&'static RuntimePaths> {
    PATHS.get()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

pub(super) struct Host {
    state: AppState,
    gui: std::sync::Mutex<gui::Session>,
}

pub fn run(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let command = args.next().ok_or("invalid_arguments")?;
    let command = command.to_str().ok_or("invalid_arguments")?;
    if command == "--version-json" {
        if args.next().is_some() {
            return Err("invalid_arguments");
        }
        return output(&identity::identity()?);
    }
    if !matches!(command, "request" | "serve" | "rpc") {
        return Err("invalid_arguments");
    }
    let mut data_dir = None;
    let mut codex_home = None;
    while let Some(option) = args.next() {
        let slot = match option.to_str() {
            Some("--data-dir") => &mut data_dir,
            Some("--codex-home") => &mut codex_home,
            _ => return Err("invalid_arguments"),
        };
        if slot.is_some() {
            return Err("invalid_arguments");
        }
        let path = PathBuf::from(args.next().ok_or("invalid_arguments")?);
        if !path.is_absolute() || !path.is_dir() {
            return Err("existing_absolute_directory_required");
        }
        *slot = Some(path.canonicalize().map_err(|_| "invalid_directory")?);
    }
    let paths = RuntimePaths {
        data_dir: data_dir.ok_or("invalid_arguments")?,
        codex_home: codex_home.ok_or("invalid_arguments")?,
    };
    if crate::config::path_is_within(&paths.data_dir, &paths.codex_home)
        || crate::config::path_is_within(&paths.codex_home, &paths.data_dir)
    {
        return Err("overlapping_directories");
    }
    if command != "rpc" {
        security::prepare_directory(&paths.data_dir)?;
    }
    PATHS
        .set(paths)
        .map_err(|_| "runtime_already_initialized")?;
    let paths = runtime_paths().ok_or("runtime_not_initialized")?;
    if command == "serve" {
        return ipc::serve();
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "input_failed")?;
    parse_request(&bytes)?;
    if command == "rpc" {
        return output(&ipc::rpc(&bytes)?);
    }
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "runtime_failed")?;
    let _entered = runtime.enter();
    // All writers use target -> store lock order, including the offline probe.
    let _instance = ipc::lock_request_instance()?;
    let _lock = lock_store(paths)?;
    // This offline probe uses native auth.json fixtures. Managed OAuth requires
    // a resident host; do not silently accept an existing account archive here.
    if paths.data_dir.join("codex_oauth_auth.json").exists() {
        return Err("managed_oauth_not_supported_in_probe");
    }
    let host = initialize_state()?;
    output(&dispatch(&host, &bytes, false))
}

fn initialize_state() -> Result<Arc<Host>> {
    let paths = runtime_paths().ok_or("runtime_not_initialized")?;
    let state = AppState::new(Arc::new(Database::init().map_err(|_| "database_failed")?));
    state
        .codex_oauth_manager
        .load_from_disk_sync()
        .map_err(|_| "account_store_invalid")?;
    // Bind one private store to one actual target; another process cannot reuse
    // its current-provider state for a different CodexHome.
    let target = paths.codex_home.to_str().ok_or("invalid_directory")?;
    match state
        .db
        .get_setting("mycodex.target")
        .map_err(|_| "database_failed")?
    {
        Some(saved) if saved != target => return Err("target_mismatch"),
        None => state
            .db
            .set_setting("mycodex.target", target)
            .map_err(|_| "database_failed")?,
        _ => (),
    }
    lifecycle::configure(&state)?;
    Ok(Arc::new(Host { state, gui: Default::default() }))
}

fn parse_request(bytes: &[u8]) -> Result<Request> {
    if bytes.len() > MAX_REQUEST {
        return Err("request_too_large");
    }
    let request: Request = serde_json::from_slice(bytes).map_err(|_| "invalid_request")?;
    if request.jsonrpc != "2.0"
        || !(request.id.is_string() || request.id.is_number() || request.id.is_null())
        || !(request.params.is_object() || request.params.is_null())
    {
        return Err("invalid_request");
    }
    Ok(request)
}

fn dispatch(host: &Host, bytes: &[u8], resident: bool) -> Value {
    let request = match parse_request(bytes) {
        Ok(request) => request,
        Err(code) => {
            return json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":code}})
        }
    };
    match handle(host, &request, resident) {
        Ok(result) => json!({"jsonrpc":"2.0", "id":request.id, "result":result}),
        Err(code) => json!({"jsonrpc":"2.0", "id":request.id,
            "error":{"code":-32000,"message":code}}),
    }
}

fn output(response: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &response).map_err(|_| "output_failed")?;
    writeln!(stdout).map_err(|_| "output_failed")
}

fn handle(host: &Host, request: &Request, resident: bool) -> Result<Value> {
    let state = &host.state;
    let p = &request.params;
    match request.method.as_str() {
        "status" => {
            let mut value = identity::target_identity()?;
            value["stage"] = json!(if resident {"upstream-core-resident"} else {"upstream-core-probe"});
            value["capabilities"] = json!(if resident {
                vec!["codexNativeServices","codexManagedAccounts","codexConversionLifecycle","commonConfig","guiProviderManagement","guiMcpManagement"]
            } else {vec!["codexNativeServices"]});
            value["route"] = lifecycle::status(state)?;
            Ok(value)
        }
        "provider/list" => {
            let providers = ProviderService::list(state, AppType::Codex).map_err(service_error)?;
            let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
            Ok(json!({"currentProviderId":current,
                "providers":providers.values().map(summary).collect::<Result<Vec<_>>>()?}))
        }
        "provider/live" => {
            let settings =
                ProviderService::read_live_settings(AppType::Codex).map_err(service_error)?;
            configuration_summary(&settings)
        }
        "provider/add" | "provider/update" => {
            let provider: Provider =
                serde_json::from_value(p["provider"].clone()).map_err(|_| "invalid_provider")?;
            let _guard = lifecycle::mutation()?;
            if !resident {
                require_native_state(state)?;
                require_direct(&provider)?;
            }
            lifecycle::conversion(&provider)?;
            let id = provider.id.clone();
            if request.method == "provider/add"
                && state
                    .db
                    .get_provider_by_id(&id, "codex")
                    .map_err(service_error)?
                    .is_some()
            {
                return Err("provider_already_exists");
            }
            let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
            let affects_live =
                current == id || (request.method == "provider/add" && current.is_empty());
            if resident && affects_live {
                lifecycle::prepare(state, &provider)?;
            }
            let changed = if request.method == "provider/add" {
                // Keep upstream first-provider activation semantics unchanged.
                ProviderService::add(state, AppType::Codex, provider, false).map_err(service_error)
            } else {
                ProviderService::update(state, AppType::Codex, None, provider)
                    .map_err(service_error)
            };
            if resident && affects_live {
                lifecycle::reconcile(state)?;
            }
            changed?;
            summary(
                &state
                    .db
                    .get_provider_by_id(&id, "codex")
                    .map_err(service_error)?
                    .ok_or("provider_not_found")?,
            )
        }
        "provider/switch" => {
            let id = p["providerId"].as_str().ok_or("invalid_params")?;
            let provider = state
                .db
                .get_provider_by_id(id, "codex")
                .map_err(service_error)?
                .ok_or("provider_not_found")?;
            let _guard = lifecycle::mutation()?;
            if !resident {
                require_native_state(state)?;
                require_direct(&provider)?;
            } else {
                lifecycle::prepare(state, &provider)?;
            }
            let switched =
                ProviderService::switch(state, AppType::Codex, id).map_err(service_error);
            if resident {
                lifecycle::reconcile(state)?;
            }
            let switched = switched?;
            // Warnings may contain upstream-controlled text. Expose only their
            // count at this stage; do not leak config or credentials via errors.
            Ok(json!({"providerId":id,"warningCount":switched.warnings.len()}))
        }
        "mcp/import" => {
            let _guard = lifecycle::mutation()?;
            require_native_state(state)?;
            Ok(json!({"imported":McpService::import_from_codex(state).map_err(service_error)?}))
        }
        "backend/shutdown" if resident => {
            let _guard = lifecycle::mutation()?;
            lifecycle::stop(state)?;
            Ok(json!({"status":"shutting_down"}))
        }
        method if method.starts_with("account/") && resident => accounts::handle(state, method, p),
        method if method.starts_with("gui/") && resident => gui::handle(host, method, p),
        _ => Err("method_not_supported"),
    }
}

fn require_native_state(state: &AppState) -> Result<()> {
    let global =
        futures::executor::block_on(state.db.get_global_proxy_config()).map_err(service_error)?;
    let app = futures::executor::block_on(state.db.get_proxy_config_for_app("codex"))
        .map_err(service_error)?;
    let backup =
        futures::executor::block_on(state.db.get_live_backup("codex")).map_err(service_error)?;
    if global.proxy_enabled || app.enabled || app.auto_failover_enabled || backup.is_some() {
        return Err("route_takeover_not_supported_in_probe");
    }
    if crate::codex_config::get_codex_config_path().exists()
        || crate::codex_config::get_codex_auth_path().exists()
    {
        ProviderService::read_live_settings(AppType::Codex).map_err(service_error)?;
    }
    if state
        .proxy_service
        .detect_takeover_in_live_config_for_app(&AppType::Codex)
    {
        return Err("route_takeover_not_supported_in_probe");
    }
    Ok(())
}

fn require_direct(provider: &Provider) -> Result<()> {
    if provider.uses_managed_account_auth()
        || provider
            .meta
            .as_ref()
            .and_then(|m| m.auth_binding.as_ref())
            .is_some_and(|b| b.source == crate::provider::AuthBindingSource::ManagedAccount)
    {
        return Err("managed_oauth_not_supported_in_probe");
    }
    let formats = [
        provider.meta.as_ref().and_then(|m| m.api_format.as_deref()),
        provider.settings_config["apiFormat"].as_str(),
        provider.settings_config["api_format"].as_str(),
    ];
    if formats
        .into_iter()
        .flatten()
        .any(|format| format != "openai_responses")
        || crate::proxy::providers::resolve_codex_catalog_tool_profile(provider)
            != crate::codex_config::CodexCatalogToolProfile::NativeResponses
    {
        return Err("conversion_not_supported_in_probe");
    }
    let config_text = provider.settings_config["config"].as_str().unwrap_or("");
    if provider.settings_config["auth"]["OPENAI_API_KEY"].as_str() == Some("PROXY_MANAGED")
        || crate::codex_config::extract_codex_experimental_bearer_token(config_text).as_deref()
            == Some("PROXY_MANAGED")
        || crate::codex_config::codex_config_has_official_proxy_route(config_text)
    {
        return Err("route_takeover_not_supported_in_probe");
    }
    let config: toml::Value = toml::from_str(config_text).map_err(|_| "invalid_provider")?;
    let source = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    let wire = config
        .get("model_providers")
        .and_then(|v| v.get(source))
        .and_then(|v| v.get("wire_api"))
        .or_else(|| config.get("wire_api"))
        .and_then(toml::Value::as_str);
    if wire.is_some_and(|wire| wire != "responses") {
        return Err("conversion_not_supported_in_probe");
    }
    Ok(())
}

fn configuration_summary(settings: &Value) -> Result<Value> {
    let config: toml::Value =
        toml::from_str(settings["config"].as_str().unwrap_or("")).map_err(|_| "invalid_config")?;
    Ok(
        json!({"model":config.get("model").and_then(toml::Value::as_str).unwrap_or(""),
        "modelProvider":config.get("model_provider").and_then(toml::Value::as_str).unwrap_or("openai")}),
    )
}

fn summary(provider: &Provider) -> Result<Value> {
    let mut result = configuration_summary(&provider.settings_config)?;
    result["id"] = json!(provider.id);
    result["name"] = json!(provider.name);
    result["commonConfigEnabled"] =
        json!(provider.meta.as_ref().and_then(|m| m.common_config_enabled));
    Ok(result)
}

fn service_error(_: crate::AppError) -> &'static str {
    "upstream_operation_failed"
}

fn lock_store(paths: &RuntimePaths) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let file = options
        .open(paths.data_dir.join("management.lock"))
        .map_err(|_| "store_busy")?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: file owns the descriptor until the complete request returns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("store_busy");
        }
    }
    Ok(file)
}
