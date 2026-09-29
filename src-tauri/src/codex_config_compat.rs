//! Remove only known ignored fields at the Codex live-file boundary.
use crate::error::AppError;
use toml_edit::DocumentMut;

pub(crate) fn normalize_live_config(text: &str) -> Result<String, AppError> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|_| AppError::Config("Invalid Codex config TOML".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ignored_fields_are_removed_without_changing_other_settings() {
        for input in [
            "# retained\nmodel = 'test' # model comment\ndisable_response_storage = true\n[mcp_servers.local]\ntype='stdio'\ncommand='test'\nenv={type='keep',TOKEN='synthetic'}\nstartup_timeout_sec=30\n[mcp_servers.remote]\ntype='http'\nurl='https://example.invalid/mcp'\nhttp_headers={Authorization='synthetic'}\n[future_extension]\ntype='keep'\ndisable_response_storage=true\n",
            "disable_response_storage=false\nmcp_servers={local={type='stdio',command='test',extension={type='keep'}}}\n",
        ] {
            let output = normalize_live_config(input).unwrap();
            let mut expected: toml::Table = input.parse().unwrap();
            expected.remove("disable_response_storage");
            for (_, server) in expected["mcp_servers"].as_table_mut().unwrap().iter_mut() {
                server.as_table_mut().unwrap().remove("type");
            }
            assert_eq!(output.parse::<toml::Table>().unwrap(), expected);
            assert_eq!(normalize_live_config(&output).unwrap(), output);
            if input.starts_with('#') {
                assert!(output.starts_with("# retained\nmodel = 'test' # model comment"));
            }
        }
        let untouched = "# comment\nmodel = 'test'\n[mcp_servers.local]\ncommand='test'\n";
        assert_eq!(normalize_live_config(untouched).unwrap(), untouched);
        assert!(normalize_live_config("invalid=[").is_err());
    }
}
