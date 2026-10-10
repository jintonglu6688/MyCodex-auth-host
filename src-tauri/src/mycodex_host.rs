//! Minimal Codex-only stdio adapter over the original service layer.
//! No desktop bootstrap or parallel provider persistence engine.
//!
//! Phase 1 of docs/mycodex-auth-v4-port.md: this host runs on the v4 kernel
//! with a reduced protocol. The v3 adapter modules (`accounts`, `auto_capture`,
//! `gui*`, `runtime_config`) stay in the tree but are not compiled until phase 2
//! rebuilds them on the v4 key-field and mode services.
use crate::{AppState, AppType, Database, Provider, ProviderService};
use serde::Deserialize;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

mod identity;
mod ipc;
pub(crate) mod lifecycle;
mod paths;
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
    if command == "--paths-json" {
        return output(&paths::resolve(args)?);
    }
    if !matches!(command, "serve" | "rpc") {
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
    if command == "serve" {
        // Native services create credential-bearing files under the private store.
        #[cfg(unix)]
        unsafe {
            libc::umask(0o077);
        }
        security::prepare_directory(&paths.data_dir)?;
    }
    PATHS
        .set(paths)
        .map_err(|_| "runtime_already_initialized")?;
    if command == "serve" {
        return ipc::serve();
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(std::io::stdin(), MAX_REQUEST as u64 + 1),
        &mut bytes,
    )
    .map_err(|_| "input_failed")?;
    parse_request(&bytes)?;
    output(&ipc::rpc(&bytes)?)
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
    Ok(Arc::new(Host { state }))
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

fn dispatch(host: &Host, bytes: &[u8]) -> Value {
    let request = match parse_request(bytes) {
        Ok(request) => request,
        Err(code) => {
            return json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":code}})
        }
    };
    match handle(host, &request) {
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

fn handle(host: &Host, request: &Request) -> Result<Value> {
    let state = &host.state;
    let p = &request.params;
    match request.method.as_str() {
        "status" => {
            let mut value = identity::target_identity()?;
            value["stage"] = json!("upstream-core-resident");
            value["capabilities"] = json!(Vec::<&str>::new());
            value["route"] = lifecycle::status(state)?;
            Ok(value)
        }
        // Read-only list for phase 1; phase 2 restores capture and editing.
        "gui/provider/list" => {
            let providers = ProviderService::list(state, AppType::Codex).map_err(service_error)?;
            let current = ProviderService::current(state, AppType::Codex).map_err(service_error)?;
            Ok(json!({"currentProviderId":current,
                "providers":providers.values().map(summary).collect::<Result<Vec<_>>>()?}))
        }
        // Temporary phase 1 direct switch; phase 2 replaces it with gui/provider/apply.
        "provider/switch" => {
            let id = p["providerId"].as_str().ok_or("invalid_params")?;
            let _guard = lifecycle::mutation()?;
            state
                .db
                .get_provider_by_id(id, "codex")
                .map_err(service_error)?
                .ok_or("provider_not_found")?;
            let switched =
                ProviderService::switch(state, AppType::Codex, id).map_err(service_error)?;
            // Warnings may contain upstream-controlled text; expose only their count.
            Ok(json!({"providerId":id,"warningCount":switched.warnings.len()}))
        }
        "backend/shutdown" => {
            p.get("forUpdate")
                .map(|v| v.as_bool().ok_or("invalid_params"))
                .transpose()?;
            let _guard = lifecycle::mutation()?;
            lifecycle::stop_listener(state)?;
            Ok(json!({"status":"shutting_down"}))
        }
        _ => Err("method_not_supported"),
    }
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
    Ok(result)
}

fn service_error(error: crate::AppError) -> &'static str {
    match error {
        crate::AppError::Message(message) if message == "config_conflict" => "config_conflict",
        _ => "upstream_operation_failed",
    }
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
