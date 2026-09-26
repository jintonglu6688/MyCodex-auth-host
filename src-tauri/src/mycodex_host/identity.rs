//! Build provenance is embedded by build.rs; runtime never consults a checkout.
use super::{json, runtime_paths, Result, Value, UPSTREAM};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::OnceLock;

pub(super) const PROTOCOL_VERSION: u32 = 2;

pub(super) fn identity() -> Result<Value> {
    static HASH: OnceLock<Result<String>> = OnceLock::new();
    let hash = HASH
        .get_or_init(|| {
            let mut file = std::fs::File::open(
                std::env::current_exe().map_err(|_| "executable_identity_failed")?,
            )
            .map_err(|_| "executable_identity_failed")?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let length = file
                    .read(&mut buffer)
                    .map_err(|_| "executable_identity_failed")?;
                if length == 0 {
                    break;
                }
                digest.update(&buffer[..length]);
            }
            Ok(format!("{:x}", digest.finalize()))
        })
        .as_ref()
        .map_err(|code| *code)?;
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "hostVersion": "0.2.0",
        "upstreamRevision": UPSTREAM,
        "sourceRevision": env!("MYCODEX_CORE_SOURCE_REVISION"),
        "sourceDirty": env!("MYCODEX_CORE_SOURCE_DIRTY") != "false",
        "target": env!("MYCODEX_CORE_TARGET"),
        "sha256": hash
    }))
}

pub(super) fn target_identity() -> Result<Value> {
    let paths = runtime_paths().ok_or("runtime_not_initialized")?;
    let mut value = identity()?;
    value["codexHome"] = json!(paths.codex_home);
    value["dataDir"] = json!(paths.data_dir);
    value["executablePath"] =
        json!(std::env::current_exe().map_err(|_| "executable_identity_failed")?);
    Ok(value)
}

pub(super) fn ready() -> Result<Value> {
    let mut value = target_identity()?;
    value["event"] = json!("ready");
    value["stage"] = json!("upstream-core-resident");
    Ok(value)
}
