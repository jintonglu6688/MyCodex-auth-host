//! Read-only Unix deployment discovery. Actual startup still requires explicit paths.
use super::{Result, Value};
use std::ffi::OsString;

#[cfg(unix)]
pub(super) fn resolve(mut args: impl Iterator<Item = OsString>) -> Result<Value> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    let home = dirs::home_dir()
        .ok_or("invalid_home")?
        .canonicalize()
        .map_err(|_| "invalid_home")?;
    if !home.is_dir() {
        return Err("invalid_home");
    }
    let mut codex = None;
    let mut data = None;
    while let Some(option) = args.next() {
        let slot = match option.to_str() {
            Some("--codex-home") => &mut codex,
            Some("--data-dir") => &mut data,
            _ => return Err("invalid_arguments"),
        };
        if slot.is_some() {
            return Err("invalid_arguments");
        }
        let path = PathBuf::from(args.next().ok_or("invalid_arguments")?);
        if !path.is_absolute() {
            return Err("absolute_directory_required");
        }
        *slot = Some(path);
    }
    let codex = codex
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .unwrap_or_else(|| home.join(".codex"));
    if !codex.is_absolute() || !codex.is_dir() {
        return Err("invalid_codex_home");
    }
    let codex = codex.canonicalize().map_err(|_| "invalid_codex_home")?;
    let root = home.join(".local/share/MyCodex/auth-core");
    let digest = format!("{:x}", Sha256::digest(codex.as_os_str().as_bytes()));
    let data = data.unwrap_or_else(|| root.join("targets").join(digest));
    if data
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return Err("invalid_data_dir");
    }
    // Resolve existing ancestors too, so discovery and startup agree across symlinks.
    let mut ancestor = data.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(
            ancestor
                .file_name()
                .ok_or("invalid_data_dir")?
                .to_os_string(),
        );
        ancestor = ancestor.parent().ok_or("invalid_data_dir")?;
    }
    if !ancestor.is_dir() {
        return Err("invalid_data_dir");
    }
    let mut data = ancestor.canonicalize().map_err(|_| "invalid_data_dir")?;
    for part in missing.into_iter().rev() {
        data.push(part);
    }
    if crate::config::path_is_within(&data, &codex) || crate::config::path_is_within(&codex, &data)
    {
        return Err("overlapping_directories");
    }
    Ok(json!({"codexHome":codex,"dataDir":data,
        "executablePath":root.join("bin/mycodex-auth-host")}))
}

#[cfg(not(unix))]
pub(super) fn resolve(_: impl Iterator<Item = OsString>) -> Result<Value> {
    Err("unsupported_platform")
}
