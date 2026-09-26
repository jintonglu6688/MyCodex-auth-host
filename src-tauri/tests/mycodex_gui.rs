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
