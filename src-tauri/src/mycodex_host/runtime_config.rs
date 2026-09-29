//! Explicit preparation before starting Codex; account reads remain read-only.
use super::{lifecycle, runtime_paths, Result};
use crate::codex_config;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;

pub(super) fn prepare() -> Result<Value> {
    let _guard = lifecycle::mutation()?;
    let path = codex_config::get_codex_config_path();
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"changed":false}));
        }
        Err(_) => return Err("invalid_live_config"),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4 * 1024 * 1024
    {
        return Err("invalid_live_config");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("invalid_live_config");
        }
    }
    let original = fs::read(&path).map_err(|_| "invalid_live_config")?;
    let text = std::str::from_utf8(&original).map_err(|_| "invalid_live_config")?;
    let normalized =
        codex_config::normalize_live_config(text).map_err(|_| "invalid_live_config")?;
    if normalized == text {
        return Ok(json!({"changed":false}));
    }

    // Keep the original in the protected target store, never in diagnostics.
    let backup = runtime_paths()
        .ok_or("runtime_not_initialized")?
        .data_dir
        .join(format!(
            "config-before-compat-{:x}.toml",
            Sha256::digest(&original)
        ));
    crate::config::atomic_write_private(&backup, &original).map_err(|_| "config_backup_failed")?;
    if fs::read(&path).map_err(|_| "config_conflict")? != original {
        return Err("config_conflict");
    }
    crate::config::write_text_file(&path, &normalized).map_err(|_| "save_outcome_unknown")?;
    Ok(json!({"changed":true}))
}
