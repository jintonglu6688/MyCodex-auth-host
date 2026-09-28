use super::*;

fn provider(config: &str) -> Provider {
    Provider::with_id(
        "context-test".into(),
        "Original".into(),
        json!({"auth":{"OPENAI_API_KEY":"synthetic"},"config":config}),
        None,
    )
}

fn edit_with(provider: Provider, kind: &str, advanced: Value) -> Provider {
    edit(
        provider,
        &json!({"name":"Renamed","kind":kind,"model":"test-model",
        "accountId":"synthetic-account","baseUrl":"https://api.openai.com/v1","advanced":advanced}),
    )
    .unwrap()
}

#[test]
fn context_values_round_trip_for_every_provider_kind_without_a_route() {
    for kind in [
        "chatgpt",
        "official_api",
        "responses",
        "chat_completions",
        "anthropic",
    ] {
        let result = edit_with(
            provider(""),
            kind,
            json!({
                "modelContextWindow":1000000,"autoCompactTokenLimit":850000,"remoteCompaction":true
            }),
        );
        let cfg = config(&result).unwrap();
        assert_eq!(cfg["model_context_window"].as_integer(), Some(1000000));
        assert_eq!(
            cfg["model_auto_compact_token_limit"].as_integer(),
            Some(850000)
        );
        let advanced = masked(&read_advanced(&result).unwrap());
        assert_eq!(advanced["modelContextWindow"], 1000000);
        assert_eq!(advanced["autoCompactTokenLimit"], 850000);
        assert_eq!(advanced["remoteCompaction"], kind != "chatgpt");
        assert!(result.settings_config.get("modelCatalog").is_none());
    }
}

#[test]
fn old_clients_and_unrelated_edits_preserve_non_1m_values_and_remote_compaction() {
    let native = "model_context_window=400000\nmodel_auto_compact_token_limit=320000\nmodel_provider='custom'\n[model_providers.custom]\nname='OpenAI'\nbase_url='https://api.openai.com/v1'\n[model_providers.backup]\nname='Backup'\n";
    let result = edit_with(provider(native), "responses", json!({"envKey":"TEST_KEY"}));
    let cfg = config(&result).unwrap();
    assert_eq!(cfg["model_context_window"].as_integer(), Some(400000));
    assert_eq!(
        cfg["model_auto_compact_token_limit"].as_integer(),
        Some(320000)
    );
    assert_eq!(
        cfg["model_providers"]["custom"]["name"].as_str(),
        Some("OpenAI")
    );
    assert_eq!(
        cfg["model_providers"]["backup"]["name"].as_str(),
        Some("Backup")
    );
    assert_eq!(result.name, "Renamed");
}

#[test]
fn switching_off_removes_both_overrides_and_restores_provider_name() {
    let native = "model_context_window=1000000\nmodel_auto_compact_token_limit=900000\nkeep='unchanged'\nmodel_provider='custom'\n[model_providers.custom]\nname='OpenAI'\n";
    let result = edit_with(
        provider(native),
        "responses",
        json!({
            "modelContextWindow":null,"autoCompactTokenLimit":null,"remoteCompaction":false
        }),
    );
    let cfg = config(&result).unwrap();
    assert!(cfg.get("model_context_window").is_none());
    assert!(cfg.get("model_auto_compact_token_limit").is_none());
    assert_eq!(cfg["keep"].as_str(), Some("unchanged"));
    assert_eq!(
        cfg["model_providers"]["custom"]["name"].as_str(),
        Some("Renamed")
    );
}

#[test]
fn remote_compaction_does_not_rewrite_reserved_provider_names() {
    let native = "model_provider='openai'\n[model_providers.openai]\nname='Original'\n";
    let result = edit_with(
        provider(native),
        "official_api",
        json!({"remoteCompaction":true}),
    );
    assert_eq!(read_advanced(&result).unwrap()["remoteCompaction"], false);
    assert_eq!(
        config(&result).unwrap()["model_providers"]["openai"]["name"].as_str(),
        Some("Renamed")
    );
}

#[test]
fn context_validation_rejects_invalid_numbers_and_flags_before_editing() {
    for key in ["modelContextWindow", "autoCompactTokenLimit"] {
        for invalid in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("1000000"),
            json!(true),
            json!(u64::MAX),
        ] {
            let mut advanced = json!({});
            advanced[key] = invalid;
            assert_eq!(validate_advanced(&advanced), Err("invalid_params"));
        }
    }
    assert_eq!(
        validate_advanced(&json!({"remoteCompaction":"yes"})),
        Err("invalid_params")
    );
    assert!(
        validate_advanced(&json!({"modelContextWindow":null,"autoCompactTokenLimit":null})).is_ok()
    );
    assert_eq!(
        masked(&json!({"autoCompactTokenLimit":900000,"apiToken":"synthetic"})),
        json!({"autoCompactTokenLimit":900000,"apiToken":"********"})
    );
}

#[test]
fn preset_context_fields_match_their_native_configuration() {
    let list = gui_catalog::presets().unwrap();
    for row in list["presets"].as_array().unwrap() {
        if row["available"] != true {
            continue;
        }
        let Some(native) = gui_catalog::native_preset(row["id"].as_str().unwrap()).unwrap() else {
            continue;
        };
        let projected = read_context(&config(&native).unwrap()).unwrap();
        for key in [
            "remoteCompaction",
            "modelContextWindow",
            "autoCompactTokenLimit",
        ] {
            assert_eq!(
                row["advanced"][key], projected[key],
                "preset {} field {key}",
                row["id"]
            );
        }
    }
    assert_eq!(
        masked(&json!({"autoCompactTokenLimit":"synthetic-secret"}))["autoCompactTokenLimit"],
        "********"
    );
}
