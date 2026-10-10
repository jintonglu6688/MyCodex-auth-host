//! Phase 1 checks for the v4 port (docs/mycodex-auth-v4-port.md), using only
//! temporary directories and synthetic credentials.
//!
//! The migration test needs a released v3 host to create the old store. Set
//! `MYCODEX_V3_HOST` to a verified 0.2.3 `mycodex-auth-host` executable; without
//! it the test only reports that it was skipped.
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

struct Target {
    _root: TempDir,
    data: PathBuf,
    codex: PathBuf,
    home: PathBuf,
}

impl Target {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("store");
        let codex = root.path().join("codex");
        let home = root.path().join("unrelated-home");
        for dir in [&data, &codex, &home] {
            fs::create_dir(dir).unwrap();
        }
        Self {
            _root: root,
            data,
            codex,
            home,
        }
    }

    fn command(&self, executable: &Path, command: &str) -> Command {
        let mut process = Command::new(executable);
        process
            .args([command, "--data-dir"])
            .arg(&self.data)
            .arg("--codex-home")
            .arg(&self.codex)
            .env("CC_SWITCH_TEST_HOME", &self.home)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", &self.home)
            .env("CODEX_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process
    }

    fn exchange(&self, executable: &Path, command: &str, method: &str, params: Value) -> Value {
        let response = self.response(executable, command, method, params);
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    fn response(&self, executable: &Path, command: &str, method: &str, params: Value) -> Value {
        let mut child = self.command(executable, command).spawn().unwrap();
        let input =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let output = child.wait_with_output().unwrap();
        for stream in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(stream).contains("synthetic-secret"));
        }
        assert!(
            output.status.success(),
            "{method}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        self.exchange(
            Path::new(env!("CARGO_BIN_EXE_mycodex-auth-host")),
            "rpc",
            method,
            params,
        )
    }

    fn assert_home_untouched(&self) {
        let entries = fs::read_dir(&self.home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(
            entries.is_empty(),
            "host wrote outside its target: {entries:?}"
        );
    }

    fn live_text(&self) -> String {
        fs::read_to_string(self.codex.join("config.toml")).unwrap()
    }
}

struct Resident(Child);

impl Resident {
    fn start(target: &Target) -> Self {
        let mut child = target
            .command(Path::new(env!("CARGO_BIN_EXE_mycodex-auth-host")), "serve")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .unwrap();
            let _ = tx.send(line);
        });
        let mut resident = Self(child);
        let ready = rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        if ready.is_empty() {
            let mut error = String::new();
            resident
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            panic!("resident failed: {error}");
        }
        let ready: Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["event"], "ready");
        assert_eq!(ready["dataDir"], json!(target.data.canonicalize().unwrap()));
        resident
    }

    fn stop(mut self, target: &Target) {
        assert_eq!(
            target.rpc("backend/shutdown", json!({"forUpdate":true}))["status"],
            "shutting_down"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while self.0.try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "resident did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn api(id: &str, model: &str) -> Value {
    json!({"id":id,"name":id,"category":"custom",
        "meta":{"commonConfigEnabled":false,"apiFormat":"openai_responses"},
        "settingsConfig":{"auth":{"OPENAI_API_KEY":format!("synthetic-secret-{id}")},
        "config":format!("model = \"{model}\"\nmodel_provider = \"{id}\"\n[model_providers.{id}]\nname = \"{id}\"\nbase_url = \"https://{id}.example.invalid/v1\"\nwire_api = \"responses\"\n")}})
}

#[test]
fn empty_store_starts_on_v4_inside_its_target() {
    let target = Target::new();
    let resident = Resident::start(&target);

    let status = target.rpc("status", json!({}));
    assert_eq!(status["capabilities"], json!([]));
    assert!(status["route"]["mode"] == "direct" || status["route"]["mode"].is_null());
    assert_eq!(status["route"]["accepting"], false);
    let list = target.rpc("gui/provider/list", json!({}));
    assert_eq!(list["providers"], json!([]));
    let removed = target.response(
        Path::new(env!("CARGO_BIN_EXE_mycodex-auth-host")),
        "rpc",
        "gui/config/common/get",
        json!({}),
    );
    assert_eq!(removed["error"]["message"], "method_not_supported");
    resident.stop(&target);
    target.assert_home_untouched();
}

#[test]
fn v3_store_migrates_and_direct_switch_preserves_user_settings() {
    let Some(v3) = std::env::var_os("MYCODEX_V3_HOST").map(PathBuf::from) else {
        eprintln!("skipped: MYCODEX_V3_HOST is not set");
        return;
    };
    let target = Target::new();
    // Create the old store with the released v3 offline probe.
    for id in ["alpha", "beta"] {
        target.exchange(
            &v3,
            "request",
            "provider/add",
            json!({"provider":api(id, &format!("{id}-model"))}),
        );
    }
    target.exchange(
        &v3,
        "request",
        "provider/switch",
        json!({"providerId":"alpha"}),
    );
    let v3_live = target.live_text();
    assert!(v3_live.contains("model_provider = \"alpha\""), "{v3_live}");
    // Content a user or another tool adds after the switch.
    let user = "approval_policy = \"on-request\"\n# kept by the user\n";
    let mcp = "\n[mcp_servers.user_tool]\ncommand = \"user-tool\"\nargs = [\"--flag\"]\n";
    fs::write(
        target.codex.join("config.toml"),
        format!("{user}{v3_live}{mcp}"),
    )
    .unwrap();

    let resident = Resident::start(&target);
    let list = target.rpc("gui/provider/list", json!({}));
    let ids = list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        ids.contains(&"alpha".into()) && ids.contains(&"beta".into()),
        "{list}"
    );
    assert_eq!(list["currentProviderId"], "alpha");

    let switched = target.rpc("provider/switch", json!({"providerId":"beta"}));
    assert_eq!(switched["providerId"], "beta");
    let live = target.live_text();
    eprintln!("--- live after v4 switch ---\n{live}--- end ---");
    let parsed: toml::Value = toml::from_str(&live).unwrap();
    // v4 writes every third-party route as the fixed `custom` table.
    assert_eq!(parsed["model_provider"].as_str(), Some("custom"), "{live}");
    assert_eq!(parsed["model"].as_str(), Some("beta-model"), "{live}");
    assert_eq!(
        parsed["model_providers"]["custom"]["base_url"].as_str(),
        Some("https://beta.example.invalid/v1"),
        "{live}"
    );
    assert!(
        live.contains(user),
        "user lines must survive byte for byte:\n{live}"
    );
    assert!(
        live.contains(mcp.trim()),
        "user MCP table must survive:\n{live}"
    );
    assert_eq!(
        target.rpc("gui/provider/list", json!({}))["currentProviderId"],
        "beta"
    );
    resident.stop(&target);
    target.assert_home_untouched();
    assert!(
        target.data.join("live-state.json").exists(),
        "v4 device state belongs to the target store"
    );
}
