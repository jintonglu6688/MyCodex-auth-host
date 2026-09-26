//! Target-local management transport. One connection carries one request.
#[cfg(unix)]
pub(super) use unix::{lock_request_instance, rpc, serve};
#[cfg(windows)]
pub(super) use windows::{lock_request_instance, rpc, serve};

use super::identity::PROTOCOL_VERSION;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, &'static str> {
    let length = reader.read_u32().await.map_err(|_| "ipc_disconnected")? as usize;
    if length > super::MAX_REQUEST + 16_384 {
        return Err("request_too_large");
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "ipc_disconnected")?;
    Ok(bytes)
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &Value,
) -> Result<(), &'static str> {
    let bytes = serde_json::to_vec(value).map_err(|_| "invalid_response")?;
    if bytes.len() > super::MAX_REQUEST + 16_384 {
        return Err("response_too_large");
    }
    writer
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| "ipc_disconnected")?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| "ipc_disconnected")?;
    Ok(())
}

#[cfg(unix)]
mod unix {
    use super::super::{dispatch, initialize_state, lock_store, output, runtime_paths};
    use super::{read_frame, write_frame, PROTOCOL_VERSION};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{
        DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
    };
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);

    fn endpoint_dir() -> Result<PathBuf, &'static str> {
        let paths = runtime_paths().ok_or("runtime_not_initialized")?;
        let digest = format!(
            "{:x}",
            Sha256::digest(paths.codex_home.as_os_str().as_bytes())
        );
        Ok(Path::new("/tmp").join(format!(
            "mycodex-auth-core-{}-{}",
            unsafe { libc::geteuid() },
            &digest[..32]
        )))
    }

    fn socket_path() -> Result<PathBuf, &'static str> {
        Ok(endpoint_dir()?.join("management.sock"))
    }

    fn runtime() -> Result<tokio::runtime::Runtime, &'static str> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime_failed")
    }

    fn owned_private(metadata: &fs::Metadata) -> bool {
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
    }

    fn verify_endpoint(path: &Path) -> Result<(), &'static str> {
        let metadata = fs::symlink_metadata(path).map_err(|_| "backend_unavailable")?;
        if !metadata.file_type().is_dir() || !owned_private(&metadata) {
            return Err("ipc_security_failed");
        }
        Ok(())
    }

    fn lock_instance() -> Result<File, &'static str> {
        let directory = endpoint_dir()?;
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err("ipc_security_failed"),
        }
        verify_endpoint(&directory)?;
        let path = directory.join("management.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| "ipc_security_failed")?;
        let metadata = file.metadata().map_err(|_| "ipc_security_failed")?;
        if !metadata.is_file() || !owned_private(&metadata) {
            return Err("ipc_security_failed");
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("backend_already_running");
        }
        Ok(file)
    }

    pub(crate) fn lock_request_instance() -> Result<File, &'static str> {
        lock_instance()
    }

    pub(crate) fn serve() -> Result<(), &'static str> {
        let runtime = runtime()?;
        let _entered = runtime.enter();
        let _instance = lock_instance()?;
        let path = socket_path()?;
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.file_type().is_socket() || !owned_private(&metadata) {
                return Err("ipc_security_failed");
            }
            fs::remove_file(&path).map_err(|_| "ipc_security_failed")?;
        }
        let listener = UnixListener::bind(&path).map_err(|_| "ipc_failed")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "ipc_security_failed")?;
        let paths = runtime_paths().ok_or("runtime_not_initialized")?;
        let _store = lock_store(paths)?;
        let state = initialize_state()?;
        super::super::lifecycle::restore(&state)?;
        output(&super::super::identity::ready()?)?;
        runtime.block_on(async {
            loop {
                let (mut stream, _) = listener.accept().await.map_err(|_| "ipc_failed")?;
                let mut shutdown = false;
                if let Ok(Ok(bytes)) =
                    tokio::time::timeout(IO_TIMEOUT, read_frame(&mut stream)).await
                {
                    let reply = match serde_json::from_slice::<Value>(&bytes) {
                        Ok(envelope) if envelope["protocolVersion"] != PROTOCOL_VERSION => {
                            json!({"error":"protocol_mismatch"})
                        }
                        Ok(envelope) if !matches_paths(&envelope) => {
                            json!({"error":"target_mismatch"})
                        }
                        Ok(envelope) => {
                            let input = serde_json::to_vec(&envelope["request"])
                                .map_err(|_| "invalid_request")?;
                            // Keep synchronous upstream services off the async driver thread.
                            let state = std::sync::Arc::clone(&state);
                            let response =
                                tokio::task::spawn_blocking(move || dispatch(&state, &input, true))
                                    .await
                                    .map_err(|_| "request_failed")?;
                            shutdown = response["result"]["status"] == "shutting_down";
                            json!({"protocolVersion":PROTOCOL_VERSION,"response":response})
                        }
                        Err(_) => json!({"error":"invalid_envelope"}),
                    };
                    let _ =
                        tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &reply)).await;
                    let _ = tokio::time::timeout(IO_TIMEOUT, stream.read_u8()).await;
                }
                if shutdown {
                    return Ok(());
                }
            }
        })
    }

    fn matches_paths(envelope: &Value) -> bool {
        let Some(paths) = runtime_paths() else {
            return false;
        };
        envelope["codexHome"].as_str() == paths.codex_home.to_str()
            && envelope["dataDir"].as_str() == paths.data_dir.to_str()
    }

    pub(crate) fn rpc(input: &[u8]) -> Result<Value, &'static str> {
        let request: Value = serde_json::from_slice(input).map_err(|_| "parse_error")?;
        runtime()?.block_on(async {
            verify_endpoint(&endpoint_dir()?)?;
            let path = socket_path()?;
            let metadata = fs::symlink_metadata(&path).map_err(|_| "backend_unavailable")?;
            if !metadata.file_type().is_socket() || !owned_private(&metadata) {
                return Err("backend_identity_mismatch");
            }
            let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
            let mut stream = loop {
                match tokio::time::timeout_at(deadline, UnixStream::connect(&path)).await {
                    Ok(Ok(stream)) => break stream,
                    Ok(Err(_)) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(25)).await
                    }
                    _ => return Err("backend_unavailable"),
                }
            };
            let paths = runtime_paths().ok_or("runtime_not_initialized")?;
            let envelope = json!({"protocolVersion":PROTOCOL_VERSION,"dataDir":paths.data_dir,
                "codexHome":paths.codex_home,"request":request});
            tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &envelope))
                .await
                .map_err(|_| "ipc_timeout")??;
            let bytes = tokio::time::timeout(RESPONSE_TIMEOUT, read_frame(&mut stream))
                .await
                .map_err(|_| "ipc_timeout")??;
            let reply: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid_response")?;
            let _ = tokio::time::timeout(IO_TIMEOUT, stream.write_u8(1)).await;
            if reply["error"] == "protocol_mismatch" {
                return Err("protocol_mismatch");
            }
            if reply["error"] == "target_mismatch" {
                return Err("target_mismatch");
            }
            if reply["protocolVersion"] != PROTOCOL_VERSION || reply.get("response").is_none() {
                return Err("invalid_response");
            }
            Ok(reply["response"].clone())
        })
    }
}
#[cfg(windows)]
mod windows {
    use super::super::{
        dispatch, initialize_state, lock_store, output, runtime_paths,
        security::CurrentUserSecurity,
    };
    use super::{read_frame, write_frame, PROTOCOL_VERSION};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);

    fn normalized(path: &Path) -> String {
        path.to_string_lossy().to_lowercase()
    }

    fn pipe_name(user: &CurrentUserSecurity) -> Result<String, &'static str> {
        let paths = runtime_paths().ok_or("runtime_not_initialized")?;
        let digest =
            Sha256::digest(format!("{}:{}", user.sid, normalized(&paths.codex_home)).as_bytes());
        Ok(format!(r"\\.\pipe\mycodex-auth-core-{:x}", digest))
    }

    fn runtime() -> Result<tokio::runtime::Runtime, &'static str> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime_failed")
    }

    fn acquire_instance(
        user: &CurrentUserSecurity,
    ) -> Result<(String, NamedPipeServer), &'static str> {
        create_instance(user, true)
    }

    pub(crate) fn lock_request_instance() -> Result<NamedPipeServer, &'static str> {
        let (_, instance) = acquire_instance(&CurrentUserSecurity::new()?)?;
        Ok(instance)
    }

    fn create_instance(
        user: &CurrentUserSecurity,
        first: bool,
    ) -> Result<(String, NamedPipeServer), &'static str> {
        let name = pipe_name(user)?;
        let mut attributes = user.attributes();
        // SAFETY: attributes borrows a valid descriptor throughout creation.
        let pipe = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    &name,
                    (&mut attributes as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES)
                        .cast(),
                )
        }
        .map_err(|_| "backend_already_running")?;
        Ok((name, pipe))
    }

    pub(crate) fn serve() -> Result<(), &'static str> {
        let runtime = runtime()?;
        let _entered = runtime.enter();
        let user = CurrentUserSecurity::new()?;
        // Obtain the target-wide first instance before touching the store.
        let (_, mut listener) = acquire_instance(&user)?;
        let paths = runtime_paths().ok_or("runtime_not_initialized")?;
        let _store = lock_store(paths)?;
        let state = initialize_state()?;
        super::super::lifecycle::restore(&state)?;
        output(&super::super::identity::ready()?)?;
        runtime.block_on(async {
            loop {
                listener.connect().await.map_err(|_| "ipc_failed")?;
                // Keep a listening instance alive while processing this client.
                // Reusing a disconnected instance lets another client open its
                // stale handle before the next ConnectNamedPipe call.
                let (_, next) = create_instance(&user, false)?;
                let mut pipe = std::mem::replace(&mut listener, next);
                let mut shutdown = false;
                if let Ok(Ok(bytes)) = tokio::time::timeout(IO_TIMEOUT, read_frame(&mut pipe)).await
                {
                    let reply = match serde_json::from_slice::<Value>(&bytes) {
                        Ok(envelope) if envelope["protocolVersion"] != PROTOCOL_VERSION => {
                            json!({"error":"protocol_mismatch"})
                        }
                        Ok(envelope) if !matches_paths(&envelope) => {
                            json!({"error":"target_mismatch"})
                        }
                        Ok(envelope) => {
                            let input = serde_json::to_vec(&envelope["request"])
                                .map_err(|_| "invalid_request")?;
                            // Keep synchronous upstream services off the async driver thread.
                            let state = std::sync::Arc::clone(&state);
                            let response =
                                tokio::task::spawn_blocking(move || dispatch(&state, &input, true))
                                    .await
                                    .map_err(|_| "request_failed")?;
                            shutdown = response["result"]["status"] == "shutting_down";
                            json!({"protocolVersion":PROTOCOL_VERSION,"response":response})
                        }
                        Err(_) => json!({"error":"invalid_envelope"}),
                    };
                    let _ = tokio::time::timeout(IO_TIMEOUT, write_frame(&mut pipe, &reply)).await;
                    // Wait for the bridge to consume the reply before disconnect:
                    // Windows may otherwise discard buffered pipe output.
                    let _ = tokio::time::timeout(IO_TIMEOUT, pipe.read_u8()).await;
                }
                drop(pipe);
                if shutdown {
                    return Ok(());
                }
            }
        })
    }

    fn matches_paths(envelope: &Value) -> bool {
        let Some(paths) = runtime_paths() else {
            return false;
        };
        envelope["codexHome"]
            .as_str()
            .is_some_and(|p| normalized(Path::new(p)) == normalized(&paths.codex_home))
            && envelope["dataDir"]
                .as_str()
                .is_some_and(|p| normalized(Path::new(p)) == normalized(&paths.data_dir))
    }

    pub(crate) fn rpc(input: &[u8]) -> Result<Value, &'static str> {
        let request: Value = serde_json::from_slice(input).map_err(|_| "parse_error")?;
        runtime()?.block_on(async {
            let user = CurrentUserSecurity::new()?;
            let name = pipe_name(&user)?;
            let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
            let mut pipe = loop {
                match ClientOptions::new().open(&name) {
                    Ok(pipe) => break pipe,
                    Err(_) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(25)).await,
                    Err(_) => return Err("backend_unavailable"),
                }
            };
            user.verify_owner(pipe.as_raw_handle())?;
            let paths = runtime_paths().ok_or("runtime_not_initialized")?;
            let envelope = json!({"protocolVersion":PROTOCOL_VERSION,"dataDir":paths.data_dir,"codexHome":paths.codex_home,"request":request});
            tokio::time::timeout(IO_TIMEOUT, write_frame(&mut pipe, &envelope)).await.map_err(|_| "ipc_timeout")??;
            // A request can queue behind bounded upstream OAuth network work.
            let bytes = tokio::time::timeout(RESPONSE_TIMEOUT, read_frame(&mut pipe)).await.map_err(|_| "ipc_timeout")??;
            let reply: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid_response")?;
            let _ = tokio::time::timeout(IO_TIMEOUT, pipe.write_u8(1)).await;
            if reply["error"] == "protocol_mismatch" { return Err("protocol_mismatch"); }
            if reply["error"] == "target_mismatch" { return Err("target_mismatch"); }
            if reply["protocolVersion"] != PROTOCOL_VERSION || reply.get("response").is_none() { return Err("invalid_response"); }
            Ok(reply["response"].clone())
        })
    }
}
