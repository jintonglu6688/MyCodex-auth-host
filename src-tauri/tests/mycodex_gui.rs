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
            2
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
        assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret"));
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
fn gui_current_save_and_reapply_capture_tool_preferences_before_common_merge() {
    for routed in [true, false] {
        let mut gui = Gui::new();
        gui.start();
        let mut edit = save("tools");
        if routed {
            edit["kind"] = json!("chat_completions");
        }
        gui.ok("gui/provider/save", edit.clone());
        let path = gui.root.path().join("codex/config.toml");
        let tools = "\n[plugins.audit]\nenabled=true\n[[skills.config]]\npath='audit/SKILL.md'\nenabled=true\n";
        std::fs::write(&path, gui.config() + tools).unwrap();
        switch_gui(&gui, "tools", "seed-tools");
        let db = rusqlite::Connection::open(gui.root.path().join("store/cc-switch.db")).unwrap();
        let common = || -> String {
            db.query_row(
                "SELECT value FROM settings WHERE key='common_config_codex'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        let flags = |text: &str| {
            let doc: toml::Value = toml::from_str(text).unwrap();
            (
                doc["plugins"]["audit"]["enabled"].as_bool().unwrap(),
                doc["skills"]["config"][0]["enabled"].as_bool().unwrap(),
            )
        };
        assert_eq!(flags(&common()), (true, true));
        let changed = gui
            .config()
            .replace("enabled = true", "enabled = false")
            .replace("enabled=true", "enabled=false");
        std::fs::write(&path, &changed).unwrap();
        let before_common = common();
        let fresh = gui.ok("gui/provider/get", json!({"providerId":"tools"}));
        gui.ok("gui/provider/list", json!({}));
        assert_eq!(common(), before_common, "reads do not capture live changes");
        edit["name"] = json!("Renamed tools");
        edit["expectedVersion"] = json!("stale");
        assert_eq!(
            gui.call("gui/provider/save", edit.clone())["error"]["message"],
            "version_conflict"
        );
        assert_eq!(
            common(),
            before_common,
            "version rejection is before capture"
        );
        assert_eq!(gui.config(), changed);
        edit["expectedVersion"] = fresh["version"].clone();
        gui.ok("gui/provider/save", edit.clone());
        assert_eq!(
            flags(&gui.config()),
            (false, false),
            "rename keeps live tools: routed={routed}"
        );
        assert_eq!(flags(&common()), (false, false));
        assert!(!common().contains("127.0.0.1"));
        assert!(!common().contains("PROXY_MANAGED"));
        assert!(!common().contains("x-mycodex-route-token"));
        assert!(!common().contains("synthetic-secret"));

        std::fs::write(
            &path,
            gui.config()
                .replace("enabled = false", "enabled = true")
                .replace("enabled=false", "enabled=true"),
        )
        .unwrap();
        switch_gui(&gui, "tools", "reapply-tools");
        assert_eq!(flags(&gui.config()), (true, true));
        assert_eq!(flags(&common()), (true, true));

        // Turning common off captures the old opted-in state, then removes it.
        std::fs::write(
            &path,
            gui.config()
                .replace("enabled = true", "enabled = false")
                .replace("enabled=true", "enabled=false"),
        )
        .unwrap();
        edit["expectedVersion"] =
            gui.ok("gui/provider/get", json!({"providerId":"tools"}))["version"].clone();
        edit["commonConfigEnabled"] = json!(false);
        gui.ok("gui/provider/save", edit.clone());
        let doc: toml::Value = toml::from_str(&gui.config()).unwrap();
        assert!(doc.get("plugins").is_none());
        assert!(doc.get("skills").is_none());
        assert_eq!(flags(&common()), (false, false));

        // A private opt-out edit must not replace shared preferences on opt-in.
        std::fs::write(&path, gui.config() + tools).unwrap();
        edit["expectedVersion"] =
            gui.ok("gui/provider/get", json!({"providerId":"tools"}))["version"].clone();
        edit["commonConfigEnabled"] = json!(true);
        gui.ok("gui/provider/save", edit);
        assert_eq!(flags(&gui.config()), (false, false));
        assert_eq!(flags(&common()), (false, false));

        // Respect upstream's explicitly-cleared marker rather than recreate it.
        db.execute(
            "UPDATE settings SET value='' WHERE key='common_config_codex'",
            [],
        )
        .unwrap();
        db.execute("INSERT OR REPLACE INTO settings(key,value) VALUES('common_config_codex_cleared','true')", []).unwrap();
        switch_gui(&gui, "tools", "cleared-tools");
        assert_eq!(common(), "");
    }
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
    let db = rusqlite::Connection::open(gui.root.path().join("store/cc-switch.db")).unwrap();
    let common: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='common_config_codex'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for forbidden in [
        "127.0.0.1",
        "PROXY_MANAGED",
        "x-mycodex-route-token",
        "synthetic-secret",
    ] {
        assert!(!common.contains(forbidden));
    }
}
