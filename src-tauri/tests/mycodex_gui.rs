//! Actual GUI RPC contracts using isolated stores and synthetic credentials only.
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};

struct Gui {
    root: tempfile::TempDir,
    child: Option<Child>,
}
impl Gui {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["store", "codex", "home"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        Self { root, child: None }
    }
    fn command(&self, mode: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mycodex-auth-host"));
        command
            .arg(mode)
            .arg("--data-dir")
            .arg(self.root.path().join("store"))
            .arg("--codex-home")
            .arg(self.root.path().join("codex"))
            .env("CC_SWITCH_TEST_HOME", self.root.path().join("home"))
            .env("HOME", self.root.path().join("home"))
            .env("USERPROFILE", self.root.path().join("home"))
            .env("LOCALAPPDATA", self.root.path().join("home"))
            .env("MYCODEX_SYNTHETIC_CAPTURE_KEY", "synthetic-secret-env")
            .env_remove("MYCODEX_SYNTHETIC_MISSING_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn start(&mut self) {
        let mut child = self.command("serve").spawn().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            std::io::BufReader::new(output)
                .read_line(&mut line)
                .unwrap();
            let _ = tx.send(line);
        });
        self.child = Some(child);
        let ready = rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        assert!(!ready.is_empty(), "backend exited before ready");
        assert_eq!(
            serde_json::from_str::<Value>(&ready).unwrap()["protocolVersion"],
            3
        );
    }
    fn call(&self, method: &str, params: Value) -> Value {
        let mut child = self.command("rpc").spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        // Explicit raw editors can display credentials; errors and ordinary RPCs cannot.
        if !matches!(
            method,
            "gui/config/provider/preview" | "gui/config/provider/view"
        ) || response.get("error").is_some()
        {
            assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret"));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("mcp-private-marker"));
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn ok(&self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    fn config(&self) -> String {
        std::fs::read_to_string(self.root.path().join("codex/config.toml")).unwrap()
    }
}
impl Drop for Gui {
    fn drop(&mut self) {
        self.stop();
    }
}

fn save(id: &str) -> Value {
    json!({"id":id,"name":id,"kind":"responses","baseUrl":"https://example.invalid/v1","apiKey":"synthetic-secret-key",
        "model":"model-a","presetId":"custom","commonConfigEnabled":true,
        "models":[{"model":"model-a","displayName":"Model A","contextWindow":32000,"reasoningLevels":["low","high"],"defaultReasoningLevel":"low","extension":true}],
        "advanced":{"requestHeaders":{"x-special":"synthetic-secret-header"},"queryParams":{"x-key":"synthetic-secret-query"}}})
}

#[test]
fn runtime_prepare_cleans_existing_fields_without_switching_accounts_or_losing_tools() {
    for routed in [false, true] {
        let mut gui = Gui::new();
        external_api(&gui, "synthetic-secret-before", "model-a");
        gui.start();
        let mut provider = save("prepare");
        if routed {
            provider["kind"] = json!("chat_completions");
        }
        gui.ok("gui/provider/save", provider);
        switch_gui(&gui, "prepare", "prepare-apply");
        gui.stop();
        gui.start();
        let auth = std::fs::read(gui.root.path().join("codex/auth.json")).ok();
        let original = format!("disable_response_storage=true\n{}\n[mcp_servers.legacy]\ntype='stdio'\ncommand='synthetic-tool'\nargs=['--keep']\nenv={{type='keep',KEY='synthetic-secret'}}\n", gui.config());
        std::fs::write(gui.root.path().join("codex/config.toml"), &original).unwrap();
        let before = gui.ok("gui/provider/list", json!({}));
        assert!(gui.ok("status", json!({}))["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("runtimeConfigPreparation")));
        assert_eq!(gui.ok("runtime/prepare", json!({}))["changed"], true);
        let cleaned = gui.config();
        let mut expected: toml::Value = original.parse().unwrap();
        expected
            .as_table_mut()
            .unwrap()
            .remove("disable_response_storage");
        expected["mcp_servers"]["legacy"]
            .as_table_mut()
            .unwrap()
            .remove("type");
        assert_eq!(cleaned.parse::<toml::Value>().unwrap(), expected);
        assert_eq!(
            std::fs::read(gui.root.path().join("codex/auth.json")).ok(),
            auth
        );
        let after = gui.ok("gui/provider/list", json!({}));
        assert_eq!(after["currentProviderId"], before["currentProviderId"]);
        assert_eq!(
            after["providers"].as_array().unwrap().len(),
            before["providers"].as_array().unwrap().len()
        );
        assert_eq!(after["route"], before["route"]);
        assert_eq!(gui.ok("runtime/prepare", json!({}))["changed"], false);
        assert_eq!(gui.config(), cleaned);
        let backups: Vec<_> = std::fs::read_dir(gui.root.path().join("store"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("config-before-compat-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);
        // A different client can reintroduce old fields while the host stays resident.
        std::fs::write(gui.root.path().join("codex/config.toml"), &original).unwrap();
        assert_eq!(gui.ok("runtime/prepare", json!({}))["changed"], true);
        assert_eq!(gui.config(), cleaned);
    }
}

#[test]
fn runtime_prepare_leaves_missing_or_invalid_config_untouched() {
    let mut gui = Gui::new();
    gui.start();
    assert_eq!(gui.ok("runtime/prepare", json!({}))["changed"], false);
    assert!(!gui.root.path().join("codex/config.toml").exists());
    std::fs::write(gui.root.path().join("codex/config.toml"), "invalid=[").unwrap();
    assert_eq!(
        gui.call("runtime/prepare", json!({}))["error"]["message"],
        "invalid_live_config"
    );
    assert_eq!(gui.config(), "invalid=[");
}

#[test]
fn browser_login_is_restorable_idempotent_and_cancellable_without_changing_live_auth() {
    let mut gui = Gui::new();
    gui.start();
    let snapshot = gui.ok("gui/account/list", json!({}));
    assert_eq!(snapshot["browserLoginAvailable"], cfg!(windows));
    let params = json!({"loginId":"browser-test","method":"browser"});
    if !cfg!(windows) {
        assert_eq!(
            gui.call("gui/account/login/start", params)["error"]["message"],
            "unsupported_target"
        );
        return;
    }
    let started = gui.ok("gui/account/login/start", params.clone());
    assert_eq!(started["status"], "pending");
    assert_eq!(started["method"], "browser");
    assert_eq!(started["userCode"], "");
    assert!(started["verificationUri"]
        .as_str()
        .unwrap()
        .starts_with("https://auth.openai.com/oauth/authorize?"));
    assert_eq!(gui.ok("gui/account/login/start", params), started);
    assert_eq!(
        gui.ok("gui/account/list", json!({}))["activeLogin"],
        started
    );
    assert_eq!(
        gui.call(
            "gui/account/login/start",
            json!({"loginId":"browser-test","method":"device"})
        )["error"]["message"],
        "login_id_conflict"
    );
    let cancelled = gui.ok(
        "gui/account/login/cancel",
        json!({"loginId":"browser-test"}),
    );
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["verificationUri"], "");
    assert_eq!(
        gui.ok("gui/account/login/get", json!({"loginId":"browser-test"})),
        cancelled
    );
    let snapshot = gui.ok("gui/account/list", json!({}));
    assert!(snapshot["activeLogin"].is_null());
    assert!(snapshot["accounts"].as_array().unwrap().is_empty());
    assert!(!gui.root.path().join("codex/auth.json").exists());
}

#[test]
fn context_settings_survive_save_restart_and_disable_with_or_without_conversion() {
    for kind in ["responses", "chat_completions", "anthropic"] {
        let mut gui = Gui::new();
        gui.start();
        assert!(gui.ok("status", json!({}))["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("contextSettings")));
        let mut params = save("context-settings");
        params["kind"] = json!(kind);
        params["commonConfigEnabled"] = json!(false);
        params["models"] = json!([]);
        params["advanced"] = json!({"modelContextWindow":1000000,
            "autoCompactTokenLimit":850000,"remoteCompaction":true});
        gui.ok("gui/provider/save", params.clone());
        let live: toml::Value = toml::from_str(&gui.config()).unwrap();
        assert_eq!(live["model_context_window"].as_integer(), Some(1000000));
        assert_eq!(
            live["model_auto_compact_token_limit"].as_integer(),
            Some(850000)
        );
        gui.stop();
        gui.start();
        let current = gui.ok("gui/provider/get", json!({"providerId":"context-settings"}));
        assert_eq!(current["advanced"]["autoCompactTokenLimit"], 850000);
        assert_eq!(current["advanced"]["remoteCompaction"], true);
        params["expectedVersion"] = current["version"].clone();
        params["name"] = json!("Renamed");
        params["advanced"] = json!({});
        let renamed = gui.ok("gui/provider/save", params.clone());
        assert_eq!(renamed["advanced"]["modelContextWindow"], 1000000);
        assert_eq!(renamed["advanced"]["remoteCompaction"], true);
        params["expectedVersion"] = renamed["version"].clone();
        params["advanced"] = json!({"modelContextWindow":null,
            "autoCompactTokenLimit":null,"remoteCompaction":false});
        let disabled = gui.ok("gui/provider/save", params);
        assert!(disabled["advanced"].get("modelContextWindow").is_none());
        assert!(disabled["advanced"].get("autoCompactTokenLimit").is_none());
        assert_eq!(disabled["advanced"]["remoteCompaction"], false);
        let live: toml::Value = toml::from_str(&gui.config()).unwrap();
        assert!(live.get("model_context_window").is_none());
        assert!(live.get("model_auto_compact_token_limit").is_none());
    }
}

fn switch_gui(gui: &Gui, id: &str, operation: &str) {
    let selected = gui.ok("gui/provider/get", json!({"providerId":id}));
    let preflight = gui.ok(
        "gui/provider/preflight",
        json!({"providerId":id,"expectedVersion":selected["version"]}),
    );
    gui.ok(
        "gui/provider/apply",
        json!({"providerId":id,"operationId":operation,
        "expectedVersion":selected["version"],"expectedFingerprint":preflight["fingerprint"]}),
    );
}

fn external_api(gui: &Gui, key: &str, model: &str) -> (Vec<u8>, Vec<u8>) {
    let config = format!("model='{model}'\nmodel_provider='external'\nmodel_reasoning_summary='detailed'\n[model_providers.external]\nbase_url='https://external.invalid/v1'\nwire_api='responses'\nrequires_openai_auth=true\n[mcp_servers.probe]\nurl='https://mcp.invalid'\nenabled=false\n");
    let auth = json!({"auth_mode":"apikey","OPENAI_API_KEY":key}).to_string();
    std::fs::write(gui.root.path().join("codex/config.toml"), &config).unwrap();
    std::fs::write(gui.root.path().join("codex/auth.json"), &auth).unwrap();
    (config.into_bytes(), auth.into_bytes())
}

fn assert_live_unchanged(gui: &Gui, before: &(Vec<u8>, Vec<u8>)) {
    assert_eq!(
        std::fs::read(gui.root.path().join("codex/config.toml")).unwrap(),
        before.0
    );
    assert_eq!(
        std::fs::read(gui.root.path().join("codex/auth.json")).unwrap(),
        before.1
    );
}

#[test]
fn auto_capture_api_reuses_identity_and_preserves_external_switch_and_tools() {
    let mut gui = Gui::new();
    let initial = external_api(&gui, "synthetic-secret-a", "model-a");
    gui.start();
    let first = gui.ok("gui/provider/list", json!({}));
    let a = first["currentProviderId"].as_str().unwrap().to_string();
    assert_eq!(first["providers"].as_array().unwrap().len(), 1);
    assert_eq!(first["liveState"], "current");
    assert_live_unchanged(&gui, &initial);
    assert_eq!(
        gui.ok("gui/mcp/list", json!({}))["servers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let changed = external_api(&gui, "synthetic-secret-a", "model-edited");
    let edited = gui.ok("gui/provider/list", json!({}));
    assert_eq!(edited["currentProviderId"], a);
    assert_eq!(edited["providers"][0]["model"], "model-edited");
    let b_live = external_api(&gui, "synthetic-secret-b", "model-b");
    let second = gui.ok("gui/provider/list", json!({}));
    let b = second["currentProviderId"].as_str().unwrap().to_string();
    assert_ne!(a, b);
    assert_eq!(second["providers"].as_array().unwrap().len(), 2);
    let saved_a = gui.ok("gui/provider/get", json!({"providerId":a}));
    assert_eq!(
        saved_a["model"], "model-a",
        "external B must not be backfilled into A"
    );
    assert_live_unchanged(&gui, &b_live);
    gui.stop();
    gui.start();
    assert_eq!(
        gui.ok("gui/provider/list", json!({}))["currentProviderId"],
        b
    );
    assert_live_unchanged(&gui, &b_live);
    external_api(&gui, "synthetic-secret-a", "model-edited");
    let returned = gui.ok("gui/provider/list", json!({}));
    assert_eq!(returned["currentProviderId"], a);
    assert_eq!(returned["providers"].as_array().unwrap().len(), 2);
    assert_live_unchanged(&gui, &changed);
}

fn external_account(gui: &Gui, subject: &str, workspace: &str, model: &str) -> (Vec<u8>, Vec<u8>) {
    use base64::Engine;
    let claims = json!({"sub":subject,"email":format!("{subject}@example.invalid"),
        "https://api.openai.com/auth":{"chatgpt_account_id":workspace},"exp":4102444800_i64});
    let token = format!(
        "{}.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let auth = json!({"tokens":{"account_id":workspace,"id_token":token,"access_token":token,
        "refresh_token":format!("synthetic-secret-{subject}")},"last_refresh":"2024-01-01T00:00:00Z"}).to_string();
    let config = format!("model='{model}'\nmodel_reasoning_summary='detailed'\n");
    std::fs::write(gui.root.path().join("codex/config.toml"), &config).unwrap();
    std::fs::write(gui.root.path().join("codex/auth.json"), &auth).unwrap();
    (config.into_bytes(), auth.into_bytes())
}

#[test]
fn auto_capture_accounts_bind_uuid_and_never_replace_previous_identity() {
    let mut gui = Gui::new();
    let a_live = external_account(&gui, "alice", "team", "model-a");
    gui.start();
    let a = gui.ok("gui/provider/list", json!({}));
    assert_eq!(a["providers"][0]["kind"], "chatgpt", "{a}");
    assert_eq!(a["providers"][0]["accountLabel"], "alice@example.invalid");
    assert_live_unchanged(&gui, &a_live);
    let b_live = external_account(&gui, "bob", "team", "model-b");
    let b = gui.ok("gui/provider/list", json!({}));
    assert_eq!(b["providers"].as_array().unwrap().len(), 2);
    assert_ne!(a["currentProviderId"], b["currentProviderId"]);
    assert_eq!(
        gui.ok("gui/account/list", json!({}))["accounts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_live_unchanged(&gui, &b_live);
    external_account(&gui, "alice", "team", "model-a");
    let returned = gui.ok("gui/provider/list", json!({}));
    assert_eq!(returned["currentProviderId"], a["currentProviderId"]);
    assert_eq!(returned["providers"].as_array().unwrap().len(), 2);
    gui.stop();
    gui.start();
    assert_eq!(
        gui.ok("gui/provider/list", json!({}))["providers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_live_unchanged(&gui, &a_live);
}

#[test]
fn workspaces_of_one_user_are_separate_accounts_and_switching_rewrites_auth() {
    let mut gui = Gui::new();
    external_account(&gui, "alice", "plus-workspace", "model-a");
    gui.start();
    let plus = gui.ok("gui/provider/list", json!({}))["currentProviderId"]
        .as_str()
        .unwrap()
        .to_string();
    external_account(&gui, "alice", "team-workspace", "model-a");
    let list = gui.ok("gui/provider/list", json!({}));
    let team = list["currentProviderId"].as_str().unwrap().to_string();
    assert_eq!(list["providers"].as_array().unwrap().len(), 2, "{list}");
    assert_ne!(plus, team);
    let workspace = || {
        let auth: Value = serde_json::from_slice(
            &std::fs::read(gui.root.path().join("codex/auth.json")).unwrap(),
        )
        .unwrap();
        auth["tokens"]["account_id"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    switch_gui(&gui, &plus, "to-plus");
    assert_eq!(workspace(), "plus-workspace");
    switch_gui(&gui, &team, "to-team");
    assert_eq!(workspace(), "team-workspace");
}

#[test]
fn chatgpt_session_model_choice_returns_after_switching_away() {
    let mut gui = Gui::new();
    external_account(&gui, "alice", "team-workspace", "model-a");
    gui.start();
    let account = gui.ok("gui/provider/list", json!({}))["currentProviderId"]
        .as_str()
        .unwrap()
        .to_string();
    let api = gui.ok("gui/provider/save", save("api"));
    assert_eq!(api["globalApplied"], false);
    // MyCodex writes the chosen model and effort through the App Server.
    let path = gui.root.path().join("codex/config.toml");
    let mut doc: toml_edit::DocumentMut = gui.config().parse().unwrap();
    doc["model"] = toml_edit::value("gpt-chosen");
    doc["model_reasoning_effort"] = toml_edit::value("xhigh");
    std::fs::write(&path, doc.to_string()).unwrap();
    switch_gui(&gui, "api", "to-api");
    let config: toml::Value = gui.config().parse().unwrap();
    assert_eq!(config["model"].as_str(), Some("model-a"), "{config}");
    assert!(config.get("model_reasoning_effort").is_none(), "{config}");
    switch_gui(&gui, &account, "to-account");
    let config: toml::Value = gui.config().parse().unwrap();
    assert_eq!(config["model"].as_str(), Some("gpt-chosen"), "{config}");
    assert_eq!(config["model_reasoning_effort"].as_str(), Some("xhigh"));
}

#[test]
fn third_party_catalog_does_not_inherit_the_codex_model_cache() {
    let mut gui = Gui::new();
    // Codex's own cache hides gpt-5.5 and announces its retirement.
    std::fs::write(
        gui.root.path().join("codex/models_cache.json"),
        json!({"client_version":"0.162.1","models":[{"slug":"gpt-5.5","display_name":"GPT-5.5",
            "visibility":"hide","tool_mode":"code_mode_only",
            "upgrade":{"model":"gpt-6.1-sol","retirement_at":"2026-10-14T19:00:00Z"}}]})
        .to_string(),
    )
    .unwrap();
    gui.start();
    let mut routed = save("routed");
    routed["kind"] = json!("chat_completions");
    gui.ok("gui/provider/save", routed);
    let config: toml::Value = gui.config().parse().unwrap();
    let catalog = config["model_catalog_json"].as_str().unwrap();
    let catalog = std::fs::read_to_string(gui.root.path().join("codex").join(catalog))
        .or_else(|_| std::fs::read_to_string(catalog))
        .unwrap();
    let catalog: Value = serde_json::from_str(&catalog).unwrap();
    let entry = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "model-a")
        .unwrap_or_else(|| panic!("{catalog}"));
    assert_ne!(entry["visibility"], "hide", "{entry}");
    assert!(entry.get("upgrade").is_none_or(Value::is_null), "{entry}");
    assert_ne!(entry["tool_mode"], "code_mode_only", "{entry}");
}

#[test]
fn auto_capture_unknown_live_keeps_saved_list_and_refuses_stale_edit() {
    let mut gui = Gui::new();
    external_api(&gui, "synthetic-secret-a", "model-a");
    gui.start();
    let initial = gui.ok("gui/provider/list", json!({}));
    let id = initial["currentProviderId"].as_str().unwrap();
    std::fs::write(gui.root.path().join("codex/config.toml"), "[broken").unwrap();
    let result = gui.ok("gui/provider/list", json!({}));
    assert_eq!(result["liveState"], "unavailable");
    assert_eq!(result["currentProviderId"], "");
    assert_eq!(result["providers"].as_array().unwrap().len(), 1);
    assert_eq!(
        gui.ok("gui/provider/get", json!({"providerId":id}))["model"],
        "model-a"
    );
    assert_eq!(
        gui.call("gui/provider/save", save(id))["error"]["message"],
        "invalid_live_config"
    );
    gui.stop();
    gui.start();
    assert_eq!(
        gui.ok("gui/provider/list", json!({}))["liveState"],
        "unavailable"
    );
}

#[test]
fn auto_capture_external_direct_retires_old_route_without_restoring_backup() {
    let mut gui = Gui::new();
    gui.start();
    gui.ok("gui/provider/save", save("direct"));
    let mut edit = save("routed");
    edit["kind"] = json!("chat_completions");
    gui.ok("gui/provider/save", edit);
    switch_gui(&gui, "routed", "route");
    let self_route = gui.ok("gui/provider/list", json!({}));
    assert_eq!(self_route["providers"].as_array().unwrap().len(), 2);
    assert_eq!(self_route["route"]["accepting"], true);
    let external = external_api(&gui, "synthetic-secret-external", "external-model");
    let captured = gui.ok("gui/provider/list", json!({}));
    assert_eq!(captured["route"]["running"], false);
    assert_eq!(captured["route"]["takeover"], false);
    assert_eq!(captured["providers"].as_array().unwrap().len(), 3);
    assert_live_unchanged(&gui, &external);
    gui.stop();
    gui.start();
    assert_eq!(
        gui.ok("gui/provider/list", json!({}))["currentProviderId"],
        captured["currentProviderId"]
    );
    assert_live_unchanged(&gui, &external);
}

#[test]
fn auto_capture_uses_effective_credentials_and_preserves_private_url_on_edit() {
    let mut gui = Gui::new();
    external_account(&gui, "unused", "workspace", "model-a");
    let path = gui.root.path().join("codex/config.toml");
    let config = "model='model-a'\nmodel_provider='external'\n[model_providers.external]\nbase_url='https://external.invalid/v1?key=synthetic-secret-query'\nwire_api='responses'\nrequires_openai_auth=false\nenv_key='MYCODEX_SYNTHETIC_CAPTURE_KEY'\nexperimental_bearer_token='synthetic-secret-unused'\n";
    std::fs::write(&path, config).unwrap();
    gui.start();
    let first = gui.ok("gui/provider/list", json!({}));
    let id = first["currentProviderId"].as_str().unwrap();
    assert_eq!(first["providers"][0]["kind"], "responses");
    assert_eq!(
        gui.ok("gui/account/list", json!({}))["accounts"],
        json!([]),
        "unused auth.json is not a login to import"
    );
    assert_eq!(gui.config(), config);
    std::fs::write(
        &path,
        config.replace("synthetic-secret-unused", "synthetic-secret-other"),
    )
    .unwrap();
    let list = gui.ok("gui/provider/list", json!({}));
    assert_eq!(
        list["currentProviderId"], id,
        "env reference takes precedence over bearer"
    );
    let mut edit = gui.ok("gui/provider/get", json!({"providerId":id}));
    edit["expectedVersion"] = edit["version"].clone();
    edit["name"] = json!("Renamed");
    gui.ok("gui/provider/save", edit);
    assert!(gui.config().contains("key=synthetic-secret-query"));
    let parsed: toml::Value = toml::from_str(&gui.config()).unwrap();
    assert_eq!(
        parsed["model_providers"]["external"]["requires_openai_auth"].as_bool(),
        Some(false)
    );
    std::fs::write(
        &path,
        config.replace(
            "MYCODEX_SYNTHETIC_CAPTURE_KEY",
            "MYCODEX_SYNTHETIC_MISSING_KEY",
        ),
    )
    .unwrap();
    let unavailable = gui.ok("gui/provider/list", json!({}));
    assert_eq!(unavailable["syncError"], "credential_reference_unavailable");
    assert_eq!(unavailable["currentProviderId"], "");
    assert_eq!(unavailable["providers"].as_array().unwrap().len(), 1);
}

#[test]
fn auto_capture_distinguishes_saved_variants_of_one_identity() {
    let mut gui = Gui::new();
    gui.start();
    let mut a = save("variant-a");
    a["models"] = json!([]);
    let mut b = a.clone();
    b["id"] = json!("variant-b");
    b["name"] = json!("variant-b");
    b["model"] = json!("model-b");
    gui.ok("gui/provider/save", a);
    gui.ok("gui/provider/save", b);
    switch_gui(&gui, "variant-b", "activate-b");
    let b_config = gui.config();
    switch_gui(&gui, "variant-a", "activate-a");
    // External tools may change the display label without changing the route.
    std::fs::write(
        gui.root.path().join("codex/config.toml"),
        b_config.replace("variant-b", "External display label"),
    )
    .unwrap();
    let current = gui.ok("gui/provider/list", json!({}));
    assert_eq!(current["currentProviderId"], "variant-b");
    assert_eq!(current["providers"].as_array().unwrap().len(), 2);
}

#[test]
fn auto_capture_retries_partial_private_commits_without_duplicating_login_or_provider() {
    let mut gui = Gui::new();
    gui.start();
    let db = rusqlite::Connection::open(gui.root.path().join("store/cc-switch.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_capture BEFORE INSERT ON providers BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").unwrap();
    let original = external_account(&gui, "partial", "team", "model-a");
    let failed = gui.ok("gui/provider/list", json!({}));
    assert_eq!(failed["liveState"], "unavailable");
    assert_eq!(failed["providers"].as_array().unwrap().len(), 0);
    assert_eq!(
        gui.ok("gui/account/list", json!({}))["accounts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_live_unchanged(&gui, &original);
    db.execute_batch("DROP TRIGGER fail_capture; CREATE TRIGGER fail_current BEFORE UPDATE OF is_current ON providers WHEN NEW.is_current=1 BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").unwrap();
    let failed = gui.ok("gui/provider/list", json!({}));
    assert_eq!(failed["liveState"], "unavailable");
    assert_eq!(failed["providers"].as_array().unwrap().len(), 1);
    assert_live_unchanged(&gui, &original);
    db.execute_batch("DROP TRIGGER fail_current;").unwrap();
    let recovered = gui.ok("gui/provider/list", json!({}));
    assert_eq!(recovered["liveState"], "current");
    assert_eq!(recovered["providers"].as_array().unwrap().len(), 1);
    assert_eq!(
        gui.ok("gui/account/list", json!({}))["accounts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_live_unchanged(&gui, &original);
}

#[test]
fn gui_mcp_native_archive_survives_switches_and_requires_explicit_import_and_fresh_version() {
    let mut gui = Gui::new();
    let path = gui.root.path().join("codex/config.toml");
    let initial = "model='test'\n[mcp_servers.remote]\nurl='https://example.invalid/mcp'\nhttp_headers={Authorization='mcp-private-marker'}\nextension={nested=[{keep=true}],empty=[]}\n[mcp_servers.local]\ncommand='never-executed'\nenv={TOKEN='mcp-private-marker'}\n";
    std::fs::write(&path, initial).unwrap();
    gui.start();
    let empty = gui.ok("gui/mcp/list", json!({}));
    assert_eq!(
        empty["servers"].as_array().unwrap().len(),
        0,
        "list must not import"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
    assert_eq!(
        gui.call(
            "gui/mcp/save",
            json!({"serverId":"remote",
        "config":{"url":"https://replacement.invalid/mcp"},"expectedVersion":empty["version"]})
        )["error"]["message"],
        "mcp_not_imported"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
    let imported = gui.ok(
        "gui/mcp/import",
        json!({"expectedVersion":empty["version"]}),
    );
    assert_eq!(imported["imported"], 2);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        initial,
        "import must not write live"
    );
    let list = gui.ok("gui/mcp/list", json!({}));
    let remote = list["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "remote")
        .unwrap();
    assert_eq!(remote["config"]["type"], "http");
    assert_eq!(
        remote["config"]["http_headers"]["Authorization"],
        "mcp-private-marker"
    );
    assert_eq!(remote["config"]["extension"]["nested"][0]["keep"], true);
    assert!(remote["config"].get("headers").is_none());
    gui.ok("gui/provider/save", save("mcp-a"));
    gui.ok("gui/provider/save", save("mcp-b"));
    switch_gui(&gui, "mcp-b", "mcp-switch-b");
    switch_gui(&gui, "mcp-a", "mcp-switch-a");
    let live: toml::Value = toml::from_str(&gui.config()).unwrap();
    for server in ["local", "remote"] {
        assert!(live["mcp_servers"][server].get("type").is_none());
    }
    assert_eq!(
        live["mcp_servers"]["remote"]["extension"]["nested"][0]["keep"].as_bool(),
        Some(true)
    );
    assert_eq!(
        live["mcp_servers"]["local"]["env"]["TOKEN"].as_str(),
        Some("mcp-private-marker")
    );

    let list = gui.ok("gui/mcp/list", json!({}));
    let mut edited = list["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "remote")
        .unwrap()["config"]
        .clone();
    edited["url"] = json!("https://edited.invalid/mcp");
    gui.ok(
        "gui/mcp/save",
        json!({"serverId":"remote","config":edited,"expectedVersion":list["version"]}),
    );
    assert_eq!(
        gui.call(
            "gui/mcp/delete",
            json!({"serverId":"local","expectedVersion":list["version"]})
        )["error"]["message"],
        "version_conflict"
    );
    let list = gui.ok("gui/mcp/list", json!({}));
    std::fs::write(
        &path,
        gui.config()
            .replace("https://edited.invalid/mcp", "https://external.invalid/mcp"),
    )
    .unwrap();
    assert_eq!(
        gui.call(
            "gui/mcp/save",
            json!({"serverId":"remote","config":edited,"expectedVersion":list["version"]})
        )["error"]["message"],
        "version_conflict"
    );
    let list = gui.ok("gui/mcp/list", json!({}));
    assert_eq!(
        gui.ok("gui/mcp/import", json!({"expectedVersion":list["version"]}))["imported"],
        0
    );
    let list = gui.ok("gui/mcp/list", json!({}));
    let remote = list["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "remote")
        .unwrap();
    assert_eq!(
        remote["config"]["url"], "https://edited.invalid/mcp",
        "existing import must retain archive parameters"
    );
    gui.ok("gui/mcp/save",json!({"serverId":"remote","config":remote["config"],"enabled":false,"expectedVersion":list["version"]}));
    let list = gui.ok("gui/mcp/list", json!({}));
    let remote = list["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "remote")
        .unwrap();
    assert_eq!(remote["enabled"], false);
    assert_eq!(remote["config"]["enabled"], false);
    assert!(!gui.config().contains("mcp_servers.remote"));
    switch_gui(&gui, "mcp-b", "disabled-switch");
    assert!(!gui.config().contains("mcp_servers.remote"));
    std::fs::write(
        &path,
        format!(
            "{}\n[mcp_servers.remote]\nurl='https://external.invalid/mcp'\n",
            gui.config()
        ),
    )
    .unwrap();
    let external = gui.ok("gui/mcp/list", json!({}));
    gui.ok("gui/mcp/save",json!({"serverId":"remote","config":remote["config"],"enabled":false,"expectedVersion":external["version"]}));
    assert!(
        !gui.config().contains("mcp_servers.remote"),
        "explicit disable removes an externally recreated entry too"
    );
    let list = gui.ok("gui/mcp/list", json!({}));
    gui.ok("gui/mcp/save",json!({"serverId":"remote","config":remote["config"],"enabled":true,"expectedVersion":list["version"]}));
    let list = gui.ok("gui/mcp/list", json!({}));
    gui.ok(
        "gui/mcp/delete",
        json!({"serverId":"local","expectedVersion":list["version"]}),
    );
    switch_gui(&gui, "mcp-a", "deleted-switch");
    let live: toml::Value = toml::from_str(&gui.config()).unwrap();
    assert!(live["mcp_servers"].get("local").is_none());
    assert_eq!(
        live["mcp_servers"]["remote"]["url"].as_str(),
        Some("https://edited.invalid/mcp")
    );
    assert_eq!(
        live["mcp_servers"]["remote"]["http_headers"]["Authorization"].as_str(),
        Some("mcp-private-marker")
    );
    assert!(live["mcp_servers"]["remote"].get("enabled").is_none());
    assert!(!gui.root.path().join("home/.claude.json").exists());
    assert!(!gui.root.path().join("home/.gemini").exists());
}

#[test]
fn gui_current_edits_use_fresh_snapshot_and_native_services_preserve_tools_and_secrets() {
    let mut gui = Gui::new();
    std::fs::write(
        gui.root.path().join("codex/config.toml"),
        "model_reasoning_summary = 'detailed'\n[mcp_servers.keep]\ncommand = 'never-executed'\n",
    )
    .unwrap();
    gui.start();
    let first = gui.ok("gui/provider/save", save("one"));
    assert_eq!(first["globalApplied"], true);
    assert_eq!(first["advanced"]["requestHeaders"]["x-special"], "********");
    assert_eq!(first["models"][0]["contextWindow"], 32000);
    assert!(gui.config().contains("mcp_servers.keep"));
    let other = gui.ok("gui/provider/save", save("two"));
    assert_eq!(other["globalApplied"], false);
    let changed = gui.config().replace("model-a", "model-external");
    std::fs::write(gui.root.path().join("codex/config.toml"), changed).unwrap();
    let fresh = gui.ok("gui/provider/get", json!({"providerId":"one"}));
    assert_eq!(fresh["model"], "model-external");
    assert_ne!(fresh["version"], first["version"]);
    let mut edit = save("one");
    edit.as_object_mut().unwrap().remove("apiKey");
    edit["expectedVersion"] = first["version"].clone();
    assert_eq!(
        gui.call("gui/provider/save", edit.clone())["error"]["message"],
        "version_conflict"
    );
    edit["expectedVersion"] = fresh["version"].clone();
    edit["model"] = fresh["model"].clone();
    edit["advanced"] = fresh["advanced"].clone();
    edit.as_object_mut().unwrap().remove("models");
    let saved = gui.ok("gui/provider/save", edit);
    assert_eq!(saved["globalApplied"], true);
    assert_eq!(saved["hasApiKey"], true);
    assert_eq!(saved["models"][0]["extension"], true);
    assert!(gui.config().contains("synthetic-secret-key"));
    assert!(gui.config().contains("synthetic-secret-header"));
    assert!(gui.config().contains("mcp_servers.keep"));
    assert_eq!(
        gui.call(
            "gui/provider/delete",
            json!({"providerId":"one","expectedVersion":saved["version"]})
        )["error"]["message"],
        "provider_in_use"
    );
    assert_eq!(
        std::fs::read_dir(gui.root.path().join("home"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn gui_apply_receipts_presets_copy_delete_and_account_failure_contracts() {
    let mut gui = Gui::new();
    gui.start();
    let presets = gui.ok("gui/preset/list", json!({}));
    assert!(presets["presets"].as_array().unwrap().len() > 50);
    let deep = presets["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "DeepSeek")
        .unwrap();
    let mut first = save("one");
    first["presetId"] = deep["id"].clone();
    first["kind"] = deep["kind"].clone();
    first["baseUrl"] = deep["baseUrl"].clone();
    first["model"] = deep["model"].clone();
    first["models"] = deep["models"].clone();
    first["advanced"] = deep["advanced"].clone();
    let first = gui.ok("gui/provider/save", first);
    assert_eq!(first["kind"], deep["kind"]);
    let second = gui.ok("gui/provider/save", save("two"));
    let plan = gui.ok(
        "gui/provider/preflight",
        json!({"providerId":"two","expectedVersion":second["version"]}),
    );
    let apply = json!({"providerId":"two","expectedVersion":plan["version"],"expectedFingerprint":plan["fingerprint"],"operationId":"apply-once"});
    let mut rejected = apply.clone();
    rejected["expectedFingerprint"] = json!("stale");
    assert_eq!(
        gui.call("gui/provider/apply", rejected)["error"]["message"],
        "config_conflict"
    );
    // A known preflight refusal must not poison the operation ID or imply a write.
    let receipt = gui.ok("gui/provider/apply", apply.clone());
    assert_eq!(receipt["status"], "applied");
    assert_eq!(gui.ok("gui/provider/apply", apply), receipt);
    assert_eq!(
        gui.ok("gui/operation/get", json!({"operationId":"apply-once"})),
        receipt
    );
    let one = gui.ok("gui/provider/get", json!({"providerId":"one"}));
    let copied = gui.ok(
        "gui/provider/copy",
        json!({"providerId":"one","expectedVersion":one["version"],"newId":"copy","name":"Copy"}),
    );
    assert_eq!(
        gui.ok(
            "gui/provider/delete",
            json!({"providerId":"copy","expectedVersion":copied["version"]})
        )["accountRemoved"],
        false
    );
    let failed = gui.ok(
        "gui/account/login/start",
        json!({"loginId":"offline-login","targetAccountId":"does-not-exist"}),
    );
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"], "account_not_found");
    assert_eq!(
        gui.ok("gui/account/login/get", json!({"loginId":"offline-login"})),
        failed
    );
    assert!(gui.ok("gui/account/list", json!({}))["accounts"]
        .as_array()
        .unwrap()
        .is_empty());
    for index in 0..34 {
        assert_eq!(
            gui.ok(
                "gui/account/login/start",
                json!({"loginId":format!("bounded-{index}"),"targetAccountId":"does-not-exist"})
            )["status"],
            "failed"
        );
    }
    assert_eq!(
        gui.call("gui/account/login/get", json!({"loginId":"offline-login"}))["error"]["message"],
        "login_not_found"
    );
    gui.stop();
    gui.start();
    assert_eq!(
        gui.call("gui/operation/get", json!({"operationId":"apply-once"}))["error"]["message"],
        "operation_unknown"
    );
    assert_eq!(
        gui.call("gui/account/login/get", json!({"loginId":"offline-login"}))["error"]["message"],
        "login_not_found"
    );
}

#[test]
fn gui_routed_switches_capture_live_tools_before_backup_restore_and_restart() {
    let mut gui = Gui::new();
    gui.start();
    gui.ok("gui/provider/save", save("direct"));
    for id in ["route-a", "route-b"] {
        let mut edit = save(id);
        edit["kind"] = json!("chat_completions");
        gui.ok("gui/provider/save", edit);
    }
    let path = gui.root.path().join("codex/config.toml");
    let tools = "\n[plugins.audit]\nenabled=false\n[[skills.config]]\npath='audit/SKILL.md'\nenabled=false\n";
    std::fs::write(&path, gui.config() + tools).unwrap();
    switch_gui(&gui, "route-a", "route-initial");
    std::fs::write(
        &path,
        gui.config()
            .replace("enabled = false", "enabled = true")
            .replace("enabled=false", "enabled=true"),
    )
    .unwrap();
    switch_gui(&gui, "route-b", "route-hot");
    let state: toml::Value = toml::from_str(&gui.config()).unwrap();
    assert_eq!(state["plugins"]["audit"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        state["skills"]["config"][0]["enabled"].as_bool(),
        Some(true)
    );
    gui.stop();
    gui.start();
    switch_gui(&gui, "direct", "restored-direct");
    let state: toml::Value = toml::from_str(&gui.config()).unwrap();
    assert_eq!(state["plugins"]["audit"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        state["skills"]["config"][0]["enabled"].as_bool(),
        Some(true)
    );
    assert!(!gui.config().contains("127.0.0.1"));
    switch_gui(&gui, "route-a", "route-delete");
    let mut state = gui.config().parse::<toml_edit::DocumentMut>().unwrap();
    state.remove("plugins");
    state.remove("skills");
    std::fs::write(&path, state.to_string()).unwrap();
    switch_gui(&gui, "route-b", "route-deleted-hot");
    gui.stop();
    gui.start();
    switch_gui(&gui, "direct", "deleted-direct");
    let state: toml::Value = toml::from_str(&gui.config()).unwrap();
    assert!(state.get("plugins").is_none());
    assert!(state.get("skills").is_none());
    switch_gui(&gui, "route-a", "deleted-route");
    let state: toml::Value = toml::from_str(&gui.config()).unwrap();
    assert!(state.get("plugins").is_none());
    assert!(state.get("skills").is_none());
    // Route state belongs to the client contract, never to provider rows.
    let db = rusqlite::Connection::open(gui.root.path().join("store/cc-switch.db")).unwrap();
    let mut rows = db
        .prepare("SELECT settings_config FROM providers WHERE app_type='codex'")
        .unwrap();
    for settings in rows.query_map([], |row| row.get::<_, String>(0)).unwrap() {
        let settings = settings.unwrap();
        for forbidden in ["127.0.0.1", "PROXY_MANAGED", "x-mycodex-route-token"] {
            assert!(
                !settings.contains(forbidden),
                "{forbidden} in a provider row"
            );
        }
    }
}

#[test]
fn chatgpt_catalog_repair_on_apply_preserves_live_auth_and_stops_discovery() {
    let mut gui = Gui::new();
    external_account(&gui, "alice", "team", "model-a");
    gui.start();
    let listed = gui.ok("gui/provider/list", json!({}));
    let id = listed["currentProviderId"].as_str().unwrap();
    let auth = std::fs::read(gui.root.path().join("codex/auth.json")).unwrap();
    // Seed the legacy shape an older host left: a catalog on the account row and
    // the generated catalog pointer in the live file.
    gui.stop();
    let db = rusqlite::Connection::open(gui.root.path().join("store/cc-switch.db")).unwrap();
    db.execute(
        "UPDATE providers SET settings_config=?1 WHERE id=?2 AND app_type='codex'",
        rusqlite::params![
            json!({"auth":{},"config":"model='model-a'\n",
                "modelCatalog":{"models":[{"model":"wrong-model"}]}})
            .to_string(),
            id
        ],
    )
    .unwrap();
    drop(db);
    let catalog = gui.root.path().join("codex/cc-switch-model-catalog.json");
    std::fs::write(&catalog, "{\"models\":[]}").unwrap();
    std::fs::write(
        gui.root.path().join("codex/config.toml"),
        format!(
            "{}model_catalog_json = {}\n",
            gui.config(),
            toml::Value::String(catalog.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    gui.start();
    assert!(gui.config().contains("model_catalog_json"));
    let before = gui.config();
    gui.ok("gui/provider/list", json!({}));
    assert_eq!(gui.config(), before, "Reading must not rewrite live files");
    let rejected = gui.call("gui/model/fetch", json!({"kind":"chatgpt"}));
    assert_eq!(rejected["error"]["message"], "invalid_params");
    switch_gui(&gui, id, "repair");
    assert!(!gui.config().contains("model_catalog_json"));
    let after_auth: Value =
        serde_json::from_slice(&std::fs::read(gui.root.path().join("codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        after_auth["tokens"],
        serde_json::from_slice::<Value>(&auth).unwrap()["tokens"]
    );
    let repaired = gui.ok("gui/provider/get", json!({"providerId":id}));
    assert_eq!(repaired["models"], json!([]));
    assert_eq!(repaired["model"], "model-a");
    gui.stop();
    gui.start();
    assert!(!gui.config().contains("model_catalog_json"));
}

#[test]
fn toml_provider_drafts_roundtrip_without_writing_until_save() {
    let mut gui = Gui::new();
    gui.start();
    let mut params = save("draft");
    params["kind"] = json!("chat_completions");
    params["commonConfigEnabled"] = json!(false);
    let saved = gui.ok("gui/provider/save", params.clone());
    params["expectedVersion"] = saved["version"].clone();
    let before = gui.config();
    let preview = gui.ok("gui/config/provider/preview", params.clone());
    let raw = preview["text"].as_str().unwrap();
    assert!(raw.contains("synthetic-secret-header"));
    assert!(!raw.contains("127.0.0.1"));
    let raw = raw
        .replace("model-a", "model-edited")
        .replace("https://example.invalid/v1", "https://edited.invalid/v1")
        .replace("synthetic-secret-header", "synthetic-secret-edited");
    params["editedText"] = json!(format!(
        "# keep comment\nmodel_context_window = 1000000\ncustom_preference = 'retained'\n{raw}"
    ));
    let edited = gui.ok("gui/config/provider/preview", params.clone());
    assert_eq!(edited["model"], "model-edited");
    assert_eq!(edited["baseUrl"], "https://edited.invalid/v1");
    assert_eq!(edited["advanced"]["modelContextWindow"], 1000000);
    assert_eq!(gui.config(), before);
    let unchanged = gui.ok("gui/provider/get", json!({"providerId":"draft"}));
    assert_eq!(unchanged["version"], saved["version"]);
    params["configToml"] = edited["text"].clone();
    params["model"] = edited["model"].clone();
    params["baseUrl"] = edited["baseUrl"].clone();
    params["advanced"] = edited["advanced"].clone();
    params.as_object_mut().unwrap().remove("editedText");
    let final_row = gui.ok("gui/provider/save", params.clone());
    params["expectedVersion"] = final_row["version"].clone();
    let reopened = gui.ok("gui/config/provider/preview", params.clone());
    assert!(reopened["text"]
        .as_str()
        .unwrap()
        .contains("synthetic-secret-edited"));
    assert!(reopened["text"]
        .as_str()
        .unwrap()
        .contains("# keep comment"));
    assert!(reopened["text"]
        .as_str()
        .unwrap()
        .contains("custom_preference"));
    for raw in [
        "bad = [synthetic-secret",
        "model_provider = 3",
        "[model_providers]\ncustom=3",
    ] {
        params["editedText"] = json!(raw);
        assert_eq!(
            gui.call("gui/config/provider/preview", params.clone())["error"]["message"],
            "invalid_config"
        );
    }
    let mut blank = save("new-draft");
    blank["name"] = json!("");
    blank["model"] = json!("");
    blank["baseUrl"] = json!("");
    gui.ok("gui/config/provider/preview", blank);
    assert_eq!(
        gui.ok("gui/provider/list", json!({}))["providers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

fn with(params: &Value, extra: Value) -> Value {
    let mut params = params.clone();
    for (key, value) in extra.as_object().unwrap() {
        params[key] = value.clone();
    }
    params
}

fn rewrite_config(gui: &Gui, from: &str, to: &str) {
    let path = gui.root.path().join("codex/config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{from} missing from {text}");
    std::fs::write(path, text.replace(from, to)).unwrap();
}

#[test]
fn full_config_editor_writes_only_changed_global_settings() {
    let mut gui = Gui::new();
    gui.start();
    let params = save("editor");
    let saved = gui.ok("gui/provider/save", params.clone());
    let params = with(&params, json!({"expectedVersion":saved["version"]}));
    // MyCodex options and the user's own settings live only in config.toml.
    let path = gui.root.path().join("codex/config.toml");
    std::fs::write(
        &path,
        format!(
            "model_reasoning_summary = \"auto\"\napproval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\n{}",
            gui.config()
        ),
    )
    .unwrap();
    let view = gui.ok("gui/config/provider/view", params.clone());
    let base = view["text"].as_str().unwrap().to_string();
    assert!(
        base.contains("model_reasoning_summary = \"auto\""),
        "{base}"
    );
    assert!(base.contains("model = \"model-a\""));
    assert!(base.contains("[model_providers.custom]"));
    let text = base
        .replace(
            "approval_policy = \"on-request\"",
            "approval_policy = \"never\"",
        )
        .replace("model = \"model-a\"", "model = \"model-b\"");
    let preview = gui.ok(
        "gui/config/provider/preview",
        with(&params, json!({"editorBase":base,"editedText":text})),
    );
    assert_eq!(preview["model"], "model-b");
    assert_eq!(preview["globalChanges"], json!(["approval_policy"]));
    let row = preview["text"].as_str().unwrap();
    assert!(!row.contains("approval_policy") && !row.contains("model_reasoning_summary"));
    // MyCodex options change a setting the editor left alone while it is open.
    rewrite_config(
        &gui,
        "model_reasoning_summary = \"auto\"",
        "model_reasoning_summary = \"detailed\"",
    );
    let saved = gui.ok(
        "gui/provider/save",
        with(
            &params,
            json!({"configToml":preview["text"],"model":"model-b","editor":{"base":base,"text":text}}),
        ),
    );
    assert!(saved.get("editorConflict").is_none(), "{saved}");
    assert_eq!(saved["globalApplied"], true);
    let config = gui.config();
    assert!(
        config.contains("model_reasoning_summary = \"detailed\""),
        "{config}"
    );
    assert!(config.contains("approval_policy = \"never\""));
    assert!(config.contains("sandbox_mode = \"read-only\""));
    assert!(config.contains("model = \"model-b\""));

    // The same key changed in the editor and outside it: nothing is written until the user chooses.
    let params = with(
        &params,
        json!({"expectedVersion":saved["version"],"model":"model-b","configToml":preview["text"]}),
    );
    let base = gui.ok("gui/config/provider/view", params.clone())["text"]
        .as_str()
        .unwrap()
        .to_string();
    let text = base
        .replace(
            "sandbox_mode = \"read-only\"",
            "sandbox_mode = \"workspace-write\"",
        )
        .replace(
            "approval_policy = \"never\"",
            "approval_policy = \"untrusted\"",
        );
    rewrite_config(
        &gui,
        "sandbox_mode = \"read-only\"",
        "sandbox_mode = \"danger-full-access\"",
    );
    let before = gui.config();
    let editor = |policy: &str| {
        with(
            &params,
            json!({"editor":{"base":base,"text":text,"onConflict":policy}}),
        )
    };
    let refused = gui.ok("gui/provider/save", editor("refuse"));
    assert_eq!(refused["editorConflict"]["keys"], json!(["sandbox_mode"]));
    assert_eq!(gui.config(), before, "a refused conflict writes nothing");
    let theirs = gui.ok("gui/provider/save", editor("keepTheirs"));
    assert!(theirs.get("editorConflict").is_none(), "{theirs}");
    let config = gui.config();
    assert!(
        config.contains("sandbox_mode = \"danger-full-access\""),
        "{config}"
    );
    assert!(config.contains("approval_policy = \"untrusted\""));
    let params = with(&params, json!({"expectedVersion":theirs["version"]}));
    let mine = gui.ok(
        "gui/provider/save",
        with(
            &params,
            json!({"editor":{"base":base,"text":text,"onConflict":"keepMine"}}),
        ),
    );
    assert!(mine.get("editorConflict").is_none(), "{mine}");
    assert!(gui.config().contains("sandbox_mode = \"workspace-write\""));

    // A form-only save never carries global edits.
    let params = with(
        &params,
        json!({"expectedVersion":mine["version"],"name":"renamed"}),
    );
    rewrite_config(
        &gui,
        "approval_policy = \"untrusted\"",
        "approval_policy = \"on-failure\"",
    );
    let renamed = gui.ok("gui/provider/save", params);
    assert!(renamed.get("editorConflict").is_none());
    assert!(gui.config().contains("approval_policy = \"on-failure\""));
}

#[test]
fn full_config_editor_keeps_routes_and_inactive_additions_off_live() {
    let mut gui = Gui::new();
    gui.start();
    let mut routed = save("routed");
    routed["kind"] = json!("chat_completions");
    let saved = gui.ok("gui/provider/save", routed.clone());
    let routed = with(&routed, json!({"expectedVersion":saved["version"]}));
    assert!(gui.config().contains("127.0.0.1"));
    let path = gui.root.path().join("codex/config.toml");
    std::fs::write(
        &path,
        format!("approval_policy = \"on-request\"\n{}", gui.config()),
    )
    .unwrap();
    let base = gui.ok("gui/config/provider/view", routed.clone())["text"]
        .as_str()
        .unwrap()
        .to_string();
    let text = base.replace(
        "approval_policy = \"on-request\"",
        "approval_policy = \"never\"",
    );
    let saved = gui.ok(
        "gui/provider/save",
        with(&routed, json!({"editor":{"base":base,"text":text}})),
    );
    assert!(saved.get("editorConflict").is_none(), "{saved}");
    let config = gui.config();
    assert!(config.contains("approval_policy = \"never\""), "{config}");
    assert!(
        config.contains("127.0.0.1"),
        "the route stays in place: {config}"
    );
    assert_eq!(
        gui.ok("status", json!({}))["route"]["running"],
        true,
        "editing the routed provider keeps routing"
    );

    // A new provider that does not become current only contributes global edits.
    let added = save("added");
    let base = gui.ok("gui/config/provider/view", added.clone())["text"]
        .as_str()
        .unwrap()
        .to_string();
    let text = base.replace(
        "approval_policy = \"never\"",
        "approval_policy = \"untrusted\"",
    );
    let saved = gui.ok(
        "gui/provider/save",
        with(&added, json!({"editor":{"base":base,"text":text}})),
    );
    assert!(saved.get("editorConflict").is_none(), "{saved}");
    let config = gui.config();
    assert!(
        config.contains("approval_policy = \"untrusted\""),
        "{config}"
    );
    assert!(
        config.contains("127.0.0.1"),
        "adding does not switch: {config}"
    );
    assert_eq!(
        gui.ok("gui/provider/list", json!({}))["currentProviderId"],
        "routed"
    );
}

#[test]
fn provider_editor_shows_the_rows_own_toml_and_leaves_global_settings_in_live() {
    let mut gui = Gui::new();
    external_account(&gui, "editor", "team", "model-a");
    gui.start();
    let list = gui.ok("gui/provider/list", json!({}));
    let row = &list["providers"][0];
    let preview = gui.ok(
        "gui/config/provider/preview",
        json!({
            "id": row["id"], "expectedVersion":row["version"], "name":"Named account",
            "kind":"chatgpt", "model":row["model"], "accountId":row["accountId"],
            "advanced":row["advanced"]
        }),
    );
    let config: toml::Value = preview["text"].as_str().unwrap().parse().unwrap();
    assert_eq!(config["model"].as_str(), Some("model-a"));
    assert!(
        config.get("model_reasoning_summary").is_none(),
        "Global settings stay in the live file, not in the provider row"
    );
    assert!(gui.config().contains("model_reasoning_summary"));
}

#[test]
fn known_chatgpt_variants_do_not_trigger_another_auto_capture() {
    for with_copy in [false, true] {
        let mut gui = Gui::new();
        external_account(&gui, "existing", "team", "model-a");
        gui.start();
        let list = gui.ok("gui/provider/list", json!({}));
        let row = &list["providers"][0];
        if with_copy {
            gui.ok(
                "gui/provider/copy",
                json!({"providerId":row["id"],"expectedVersion":row["version"],
        "newId":"manual-copy","name":"Manual variant"}),
            );
        }
        external_account(&gui, "other", "team", "other-model");
        gui.ok("gui/provider/list", json!({}));
        let live = external_account(&gui, "existing", "team", "externally-changed-model");
        let refreshed = gui.ok("gui/provider/list", json!({}));
        assert_eq!(
        refreshed["providers"].as_array().unwrap().len(),
        if with_copy { 3 } else { 2 },
        "An already-known account with ambiguous provider variants must not create another card"
    );
        assert_eq!(refreshed["liveState"], "current");
        assert!(
            refreshed["currentProviderId"] == row["id"]
                || refreshed["currentProviderId"] == "manual-copy"
        );
        assert_live_unchanged(&gui, &live);
        let again = gui.ok("gui/provider/list", json!({}));
        assert_eq!(again["currentProviderId"], refreshed["currentProviderId"]);
        assert_eq!(
            again["providers"].as_array().unwrap().len(),
            if with_copy { 3 } else { 2 }
        );
    }
}

#[test]
fn ignored_legacy_fields_do_not_break_exact_provider_matching() {
    let mut gui = Gui::new();
    external_account(&gui, "legacy-match", "team", "model-a");
    gui.start();
    let before = gui.ok("gui/provider/list", json!({}));
    std::fs::write(
        gui.root.path().join("codex/config.toml"),
        format!("disable_response_storage=true\n{}", gui.config()),
    )
    .unwrap();
    let after = gui.ok("gui/provider/list", json!({}));
    assert_eq!(after["currentProviderId"], before["currentProviderId"]);
    assert_eq!(after["providers"].as_array().unwrap().len(), 1);
    let diagnostics =
        std::fs::read_to_string(gui.root.path().join("store/auto-capture.jsonl")).unwrap();
    let last: Value = serde_json::from_str(diagnostics.lines().last().unwrap()).unwrap();
    assert_eq!(last["capture"]["exactCount"], 1);
    assert_eq!(last["capture"]["branch"], "exact_previous");
}

#[test]
fn native_cc_edit_of_single_chatgpt_card_keeps_existing_provider() {
    for use_other_current in [false, true] {
        let mut gui = Gui::new();
        external_account(&gui, "single-account", "team", "model-a");
        gui.start();
        let list = gui.ok("gui/provider/list", json!({}));
        let row = &list["providers"][0];
        gui.ok(
            "gui/provider/save",
            json!({"id":row["id"],"expectedVersion":row["version"],
            "name":"Manual account", "kind":"chatgpt", "model":"model-a",
            "accountId":row["accountId"], "commonConfigEnabled":true}),
        );
        if use_other_current {
            external_account(&gui, "other", "team", "other-model");
            gui.ok("gui/provider/list", json!({}));
        }
        external_account(&gui, "single-account", "team", "model-a");
        let config = gui.config();
        let path = gui.root.path().join("codex/config.toml");
        for enabled in [true, false] {
            std::fs::write(&path, if enabled {
                format!("{config}model_context_window=1000000\nmodel_auto_compact_token_limit=900000\n")
            } else { config.clone() }).unwrap();
            let expected = gui.config();
            let captured = gui.ok("gui/provider/list", json!({}));
            assert_eq!(
                captured["providers"].as_array().unwrap().len(),
                if use_other_current { 2 } else { 1 }
            );
            assert_eq!(captured["currentProviderId"], row["id"]);
            let current = captured["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == row["id"])
                .unwrap();
            assert_eq!(current["name"], "Manual account");
            assert_eq!(gui.config(), expected);
        }
    }
}

#[test]
fn capture_diagnostics_record_decisions_and_partial_failure_without_private_data() {
    let mut gui = Gui::new();
    external_account(
        &gui,
        "diagnostic-private-person",
        "diagnostic-private-team",
        "private-model",
    );
    gui.start();
    let initial = gui.ok("gui/provider/list", json!({}));
    let id = initial["currentProviderId"].as_str().unwrap();
    let path = gui.root.path().join("store/auto-capture.jsonl");
    let records = || -> Vec<Value> {
        let text = std::fs::read_to_string(&path).unwrap();
        for private in [
            "synthetic-secret",
            "diagnostic-private",
            "example.invalid",
            "private-model",
            id,
        ] {
            assert!(!text.contains(private), "Diagnostic leaked private input");
        }
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let first = records();
    let created = first.iter().find(|v| v["action"] == "created").unwrap();
    assert_eq!(created["capture"]["branch"], "new_identity");
    assert_eq!(created["capture"]["candidateCount"], 0);
    assert_eq!(created["capture"]["providerSaved"], true);
    assert_eq!(created["capture"]["accountImported"], true);
    assert_eq!(created["capture"]["stage"], "complete");
    assert_eq!(
        created["capture"]["providerRef"].as_str().unwrap().len(),
        64
    );
    assert_eq!(first.last().unwrap()["action"], "reused");
    external_account(
        &gui,
        "diagnostic-private-person",
        "diagnostic-private-team",
        "changed-private-model",
    );
    gui.ok("gui/provider/list", json!({}));
    let reused = records().pop().unwrap();
    assert_eq!(reused["capture"]["branch"], "identity_previous");
    assert_eq!(reused["capture"]["candidateCount"], 1);
    assert_eq!(reused["capture"]["exactCount"], 0);
    assert_eq!(
        reused["capture"]["providerRef"],
        created["capture"]["providerRef"]
    );
    assert_eq!(reused["capture"]["providerSaved"], false);
    let db = rusqlite::Connection::open(gui.root.path().join("store/cc-switch.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_capture_diagnostic BEFORE INSERT ON providers BEGIN SELECT RAISE(FAIL, 'synthetic-secret-failure'); END;").unwrap();
    external_account(
        &gui,
        "diagnostic-private-other",
        "diagnostic-private-team",
        "private-model",
    );
    let failed = gui.ok("gui/provider/list", json!({}));
    assert_eq!(failed["liveState"], "unavailable");
    let failure = records().pop().unwrap();
    assert_eq!(failure["action"], "failed");
    assert_eq!(failure["capture"]["stage"], "save_provider");
    assert_eq!(failure["capture"]["accountImported"], true);
    assert_eq!(failure["capture"]["providerSaved"], false);
    assert_eq!(failure["capture"]["currentSet"], false);
    assert!(failure["error"].is_string());
    db.execute_batch("DROP TRIGGER fail_capture_diagnostic;")
        .unwrap();
    gui.stop();
    gui.start();
    assert!(records().len() > first.len());
    assert!(records().iter().any(|v| v["action"] == "failed"));
    // A broken log destination must not block or roll back capture.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    external_account(
        &gui,
        "diagnostic-private-new",
        "diagnostic-private-team",
        "private-model",
    );
    let result = gui.ok("gui/provider/list", json!({}));
    assert_eq!(result["liveState"], "current");
    assert_eq!(result["providers"].as_array().unwrap().len(), 3);
}
