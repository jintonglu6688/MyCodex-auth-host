//! Explicit preparation before starting Codex; account reads remain read-only.
use super::{lifecycle, runtime_paths, Result};
use crate::codex_config;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use toml_edit::DocumentMut;

/// Remove only settings Codex ignores: the legacy `disable_response_storage`
/// and `type` inside MCP server tables. Everything else stays byte for byte.
fn normalize(text: &str) -> Result<String> {
    let mut doc: DocumentMut = text.parse().map_err(|_| "invalid_live_config")?;
    let mut changed = doc.remove("disable_response_storage").is_some();
    if let Some(servers) = doc
        .get_mut("mcp_servers")
        .and_then(|v| v.as_table_like_mut())
    {
        for (_, server) in servers.iter_mut() {
            if let Some(table) = server.as_table_like_mut() {
                changed |= table.remove("type").is_some();
            }
        }
    }
    Ok(if changed {
        doc.to_string()
    } else {
        text.into()
    })
}

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
    let normalized = normalize(text)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ignored_fields_are_removed_without_changing_other_settings() {
        for input in [
            "# retained\nmodel = 'test' # model comment\ndisable_response_storage = true\n[mcp_servers.local]\ntype='stdio'\ncommand='test'\nenv={type='keep',TOKEN='synthetic'}\nstartup_timeout_sec=30\n[mcp_servers.remote]\ntype='http'\nurl='https://example.invalid/mcp'\nhttp_headers={Authorization='synthetic'}\n[future_extension]\ntype='keep'\ndisable_response_storage=true\n",
            "disable_response_storage=false\nmcp_servers={local={type='stdio',command='test',extension={type='keep'}}}\n",
        ] {
            let output = normalize(input).unwrap();
            let mut expected: toml::Table = input.parse().unwrap();
            expected.remove("disable_response_storage");
            for (_, server) in expected["mcp_servers"].as_table_mut().unwrap().iter_mut() {
                server.as_table_mut().unwrap().remove("type");
            }
            assert_eq!(output.parse::<toml::Table>().unwrap(), expected);
            assert_eq!(normalize(&output).unwrap(), output);
            if input.starts_with('#') {
                assert!(output.starts_with("# retained\nmodel = 'test' # model comment"));
            }
        }
        let untouched = "# comment\nmodel = 'test'\n[mcp_servers.local]\ncommand='test'\n";
        assert_eq!(normalize(untouched).unwrap(), untouched);
        assert!(normalize("invalid=[").is_err());
    }
}
