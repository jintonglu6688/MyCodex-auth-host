//! Codex-only MCP editor projection; the original service owns import and persistence.
use super::{gui_provider::hash, lifecycle, Host, Result};
use crate::app_config::{McpApps, McpServer};
use crate::McpService;
use serde_json::{json, Value};

fn live() -> Result<Value> {
    let text = crate::codex_config::read_and_validate_codex_config_text()
        .map_err(|_| "invalid_mcp_config")?;
    let root: toml::Table = toml::from_str(&text).map_err(|_| "invalid_mcp_config")?;
    for value in [
        root.get("mcp_servers"),
        root.get("mcp").and_then(|m| m.get("servers")),
    ]
    .into_iter()
    .flatten()
    {
        if !value.is_table() {
            return Err("invalid_mcp_config");
        }
    }
    Ok(
        json!({"servers":root.get("mcp_servers"),"legacy":root.get("mcp").and_then(|m|m.get("servers"))}),
    )
}

fn version(host: &Host) -> Result<String> {
    let servers = McpService::get_all_servers(&host.state).map_err(|_| "mcp_read_failed")?;
    Ok(hash(&json!({"stored":servers,"live":live()?})))
}

fn enabled(server: &McpServer) -> bool {
    server.apps.codex && server.server.get("enabled").and_then(Value::as_bool) != Some(false)
}

// A headless store belongs to one Codex target. Never let an unexpected archive
// flag invoke the original service's writes into other applications' real homes.
fn codex_only(host: &Host) -> Result<()> {
    if McpService::get_all_servers(&host.state)
        .map_err(|_| "mcp_read_failed")?
        .values()
        .any(|s| {
            s.apps
                .enabled_apps()
                .iter()
                .any(|app| *app != crate::AppType::Codex)
        })
    {
        return Err("unsupported_target");
    }
    Ok(())
}

fn config(server: &McpServer) -> Result<Value> {
    let mut config = server.server.clone();
    let obj = config.as_object_mut().ok_or("invalid_mcp_config")?;
    if let Some(headers) = obj.remove("headers") {
        obj.insert("http_headers".into(), headers);
    }
    obj.insert("enabled".into(), json!(enabled(server)));
    Ok(config)
}

fn native_config(config: &Value) -> Result<Value> {
    let mut config = config.clone();
    let obj = config.as_object_mut().ok_or("invalid_mcp_config")?;
    obj.remove("enabled");
    if let Some(headers) = obj.remove("http_headers") {
        obj.insert("headers".into(), headers);
    }
    if !obj.contains_key("type") {
        let kind = if obj.get("url").is_some_and(Value::is_string) {
            "http"
        } else {
            "stdio"
        };
        obj.insert("type".into(), json!(kind));
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("stdio") => {
            if obj.contains_key("url") {
                return Err("invalid_mcp_config");
            }
            if !obj
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
            {
                return Err("invalid_mcp_config");
            }
        }
        Some("http" | "sse") => {
            if obj.contains_key("command") {
                return Err("invalid_mcp_config");
            }
            let url = obj
                .get("url")
                .and_then(Value::as_str)
                .ok_or("invalid_mcp_config")?;
            let url = reqwest::Url::parse(url).map_err(|_| "invalid_mcp_config")?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("invalid_mcp_config");
            }
        }
        _ => return Err("invalid_mcp_config"),
    }
    for key in ["env", "headers", "env_http_headers"] {
        if let Some(map) = obj.get(key) {
            if !map
                .as_object()
                .is_some_and(|m| m.values().all(Value::is_string))
            {
                return Err("invalid_mcp_config");
            }
        }
    }
    if obj
        .get("args")
        .is_some_and(|v| !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
    {
        return Err("invalid_mcp_config");
    }
    // Reject null/unsupported TOML before the service can partially save the DB.
    toml::to_string(&config).map_err(|_| "invalid_mcp_config")?;
    Ok(config)
}

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    if method == "gui/mcp/list" {
        let rows = McpService::get_all_servers(&host.state)
            .map_err(|_| "mcp_read_failed")?
            .values()
            .map(|s| Ok(json!({"id":s.id,"name":s.name,"config":config(s)?,"enabled":enabled(s)})))
            .collect::<Result<Vec<_>>>()?;
        return Ok(json!({"servers":rows,"version":version(host)?}));
    }
    if !matches!(method, "gui/mcp/save" | "gui/mcp/delete" | "gui/mcp/import") {
        return Err("method_not_supported");
    }
    let _guard = lifecycle::mutation()?;
    super::auto_capture::before_write(&host.state)?;
    if params["expectedVersion"].as_str() != Some(version(host)?.as_str()) {
        return Err("version_conflict");
    }
    codex_only(host)?;
    if method == "gui/mcp/import" {
        let count = McpService::import_from_codex(&host.state).map_err(|_| "operation_unknown")?;
        return Ok(json!({"imported":count}));
    }
    let id = params["serverId"].as_str().ok_or("invalid_params")?;
    if id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err("invalid_params");
    }
    let mut existing = McpService::get_all_servers(&host.state).map_err(|_| "mcp_read_failed")?;
    if method == "gui/mcp/delete" {
        if !existing.contains_key(id) {
            return Err("mcp_not_found");
        }
        McpService::delete_server(&host.state, id).map_err(|_| "operation_unknown")?;
        return Ok(json!({"deleted":true}));
    }
    if !existing.contains_key(id) {
        let live = live()?;
        if ["servers", "legacy"]
            .iter()
            .any(|key| live[key].get(id).is_some())
        {
            return Err("mcp_not_imported");
        }
    }
    let mut server = existing.shift_remove(id).unwrap_or_else(|| McpServer {
        id: id.into(),
        name: id.into(),
        server: json!({}),
        apps: McpApps {
            codex: true,
            ..Default::default()
        },
        description: None,
        homepage: None,
        docs: None,
        tags: Vec::new(),
    });
    let enable = params
        .get("enabled")
        .or_else(|| params["config"].get("enabled"));
    let enable = match enable {
        Some(v) => v.as_bool().ok_or("invalid_params")?,
        None => enabled(&server),
    };
    server.server = native_config(&params["config"])?;
    server.apps.codex = enable;
    McpService::upsert_server(&host.state, server).map_err(|_| "operation_unknown")?;
    if !enable {
        // Upsert removes only on true -> false. An external writer may have
        // recreated an already-disabled entry; an explicit disable must remove it.
        McpService::toggle_app(&host.state, id, crate::AppType::Codex, false)
            .map_err(|_| "operation_unknown")?;
    }
    Ok(json!({"saved":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_editor_mapping_preserves_extensions_and_rejects_lossy_values() {
        let input = json!({"url":"https://example.invalid/mcp","http_headers":{"X-Key":"fake"},
            "enabled":false,"extension":{"nested":[{"keep":true}]}});
        let output = native_config(&input).unwrap();
        assert_eq!(output["type"], "http");
        assert_eq!(output["headers"]["X-Key"], "fake");
        assert_eq!(output["extension"], input["extension"]);
        assert!(output.get("enabled").is_none());
        assert!(native_config(&json!({"command":"test","extension":null})).is_err());
        assert!(native_config(&json!({"command":"test","env":{"secret":42}})).is_err());
    }
}
