//! Process-boundary checks using only temporary files and synthetic credentials.
//!
//! The migration tests need a released v3 host to create old stores. Set
//! `MYCODEX_V3_HOST` to a verified 0.2.3 `mycodex-auth-host` executable; without
//! it those tests only report that they were skipped.
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use tempfile::TempDir;

const HEADER: &str = "x-mycodex-route-token";

struct Probe {
    root: TempDir,
    data: PathBuf,
    codex: PathBuf,
    unrelated: PathBuf,
}

impl Probe {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("store");
        let codex = root.path().join("codex");
        let unrelated = root.path().join("unrelated-home");
        for dir in [&data, &codex, &unrelated] {
            fs::create_dir(dir).unwrap();
        }
        Self {
            root,
            data,
            codex,
            unrelated,
        }
    }

    fn command(&self, executable: &Path, mode: &str, data: &Path, target: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .args([mode, "--data-dir"])
            .arg(data)
            .arg("--codex-home")
            .arg(target)
            .env("CC_SWITCH_TEST_HOME", &self.unrelated)
            .env("HOME", &self.unrelated)
            .env("USERPROFILE", &self.unrelated)
            .env("LOCALAPPDATA", &self.unrelated)
            .env("CODEX_HOME", &self.unrelated)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn host() -> &'static Path {
        Path::new(env!("CARGO_BIN_EXE_mycodex-auth-host"))
    }

    fn exchange(&self, executable: &Path, input: &[u8], target: &Path) -> Output {
        let mut child = self
            .command(executable, "rpc", &self.data, target)
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        for stream in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(stream).contains("synthetic-secret"));
        }
        assert_eq!(
            fs::read_dir(&self.unrelated).unwrap().count(),
            0,
            "explicit target must cover settings, DB, device state and Codex paths"
        );
        output
    }

    fn raw(&self, input: &[u8], target: &Path) -> Output {
        self.exchange(Self::host(), input, target)
    }

    fn call_with(&self, executable: &Path, method: &str, params: Value) -> Value {
        let input =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                .unwrap();
        let output = self.exchange(executable, &input, &self.codex);
        assert!(
            output.status.success(),
            "{method}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["id"], 1);
        response
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.call_with(Self::host(), method, params)
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    /// Exit output of a `serve` that is expected to fail before becoming ready.
    fn serve_failure(&self, data: &Path, target: &Path) -> String {
        let output = self
            .command(Self::host(), "serve", data, target)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty(), "unexpected ready line");
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn save(&self, params: Value) -> Value {
        self.ok("gui/provider/save", params)
    }

    fn apply(&self, id: &str) {
        let response = self.try_apply(id);
        assert!(response.get("error").is_none(), "apply {id}: {response}");
    }

    fn try_apply(&self, id: &str) -> Value {
        let selected = self.ok("gui/provider/get", json!({"providerId":id}));
        let preflight = self.call(
            "gui/provider/preflight",
            json!({"providerId":id,"expectedVersion":selected["version"]}),
        );
        if preflight.get("error").is_some() {
            return preflight;
        }
        let operation = format!("apply-{id}-{}", uuid_like());
        self.call(
            "gui/provider/apply",
            json!({"providerId":id,"expectedVersion":selected["version"],
                "expectedFingerprint":preflight["result"]["fingerprint"],"operationId":operation}),
        )
    }

    fn live_text(&self) -> String {
        fs::read_to_string(self.codex.join("config.toml")).unwrap()
    }

    fn live(&self) -> toml::Value {
        toml::from_str(&self.live_text()).unwrap()
    }

    fn route_token(&self) -> String {
        let live = self.live();
        assert_eq!(live["model_provider"].as_str(), Some("custom"), "{live}");
        live["model_providers"]["custom"]["http_headers"][HEADER]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn route(&self) -> Value {
        self.ok("status", json!({}))["route"].clone()
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn form(id: &str, kind: &str, base: &str, model: &str) -> Value {
    json!({"id":id,"name":id,"kind":kind,"baseUrl":base,"model":model,"presetId":"custom",
        "apiKey":format!("synthetic-secret-{id}")})
}

fn direct(id: &str) -> Value {
    form(
        id,
        "responses",
        &format!("https://{id}.example.invalid/v1"),
        &format!("{id}-model"),
    )
}

struct Resident(Child);

impl Resident {
    fn start(p: &Probe) -> Self {
        Self::start_with(p, Probe::host(), 3)
    }

    fn start_with(p: &Probe, executable: &Path, protocol: u64) -> Self {
        let mut child = p
            .command(executable, "serve", &p.data, &p.codex)
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
        assert_eq!(ready["protocolVersion"], protocol);
        if protocol == 3 {
            assert_eq!(ready["hostVersion"], "0.3.4");
            assert_eq!(
                ready["sourceRevision"],
                env!("MYCODEX_CORE_SOURCE_REVISION")
            );
        }
        assert_eq!(ready["codexHome"], json!(p.codex.canonicalize().unwrap()));
        assert_eq!(ready["dataDir"], json!(p.data.canonicalize().unwrap()));
        resident
    }

    fn shutdown(mut self, p: &Probe, params: Value) {
        assert_eq!(p.ok("backend/shutdown", params)["status"], "shutting_down");
        assert!(self.0.wait().unwrap().success());
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn v3_host() -> Option<PathBuf> {
    let host = std::env::var_os("MYCODEX_V3_HOST").map(PathBuf::from);
    if host.is_none() {
        eprintln!("skipped: MYCODEX_V3_HOST is not set");
    }
    host
}

/// A local upstream that answers Chat Completions and Anthropic Messages.
fn upstream(
    runtime: &tokio::runtime::Runtime,
) -> (
    std::net::SocketAddr,
    std::sync::mpsc::Receiver<(String, Value)>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let address = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let tx = tx.clone();
            async move {
                let path = request.uri().path().to_owned();
                assert!(!request.headers().contains_key(HEADER));
                let body = axum::body::to_bytes(request.into_body(), 1_000_000)
                    .await
                    .unwrap();
                tx.send((path.clone(), serde_json::from_slice::<Value>(&body).unwrap()))
                    .unwrap();
                let response = if path.ends_with("/messages") {
                    json!({"id":"msg_test","type":"message","role":"assistant","model":"test-model",
                        "content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
                } else {
                    json!({"id":"chat_test","object":"chat.completion","created":1,"model":"test-model",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
                };
                axum::Json(response)
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    });
    (address, rx)
}

fn route_request(runtime: &tokio::runtime::Runtime, port: u64, token: &str) -> Value {
    runtime.block_on(async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .header(HEADER, token)
            .json(&json!({"model":"test-model","input":"hi","stream":false}))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    })
}

#[test]
fn build_identity_is_independent_of_runtime_checkout_and_matches_resident() {
    use sha2::{Digest, Sha256};
    let p = Probe::new();
    let executable = env!("CARGO_BIN_EXE_mycodex-auth-host");
    let output = Command::new(executable)
        .arg("--version-json")
        .current_dir(&p.unrelated)
        .env("MYCODEX_CORE_SOURCE_REVISION", "runtime-spoof")
        .env("MYCODEX_CORE_SOURCE_DIRTY", "false")
        .env("MYCODEX_CORE_TARGET", "runtime-spoof")
        .output()
        .unwrap();
    assert!(output.status.success());
    let identity: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(identity["protocolVersion"], 3);
    assert_eq!(identity["hostVersion"], "0.3.4");
    assert_eq!(
        identity["sourceRevision"],
        env!("MYCODEX_CORE_SOURCE_REVISION")
    );
    assert_eq!(
        identity["sourceDirty"],
        env!("MYCODEX_CORE_SOURCE_DIRTY") != "false"
    );
    assert_eq!(identity["target"], env!("MYCODEX_CORE_TARGET"));
    assert_eq!(
        identity["upstreamRevision"],
        "790ed8009df809bedaf6c31b5ced05da46bb1f1f"
    );
    for dir in [&p.unrelated, &p.data, &p.codex] {
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }
    let mut file = fs::File::open(executable).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let length = file.read(&mut buffer).unwrap();
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    assert_eq!(identity["sha256"], format!("{:x}", digest.finalize()));
    let resident = Resident::start(&p);
    let status = p.ok("status", json!({}));
    for (key, expected) in identity.as_object().unwrap() {
        assert_eq!(status[key], *expected, "resident field {key}");
    }
    assert_eq!(status["codexHome"], json!(p.codex.canonicalize().unwrap()));
    resident.shutdown(&p, json!({}));
}

#[test]
fn empty_store_starts_inside_its_target_and_removed_methods_fail_closed() {
    let p = Probe::new();
    let resident = Resident::start(&p);
    let status = p.ok("status", json!({}));
    assert!(status["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("globalAuthCapture")));
    assert!(!status["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("commonConfig")));
    assert!(status["route"]["mode"] == "direct" || status["route"]["mode"].is_null());
    assert_eq!(status["route"]["accepting"], false);
    assert_eq!(status["route"]["takeover"], false);
    assert_eq!(p.ok("gui/provider/list", json!({}))["providers"], json!([]));
    for method in [
        "gui/config/common/get",
        "gui/config/common/save",
        "provider/add",
        "provider/switch",
        "account/list",
        "mcp/import",
    ] {
        assert_eq!(
            p.call(method, json!({}))["error"]["message"],
            "method_not_supported",
            "{method}"
        );
    }
    resident.shutdown(&p, json!({"forUpdate":true}));
    assert!(p.data.join("live-state.json").exists());
}

#[test]
fn a_second_resident_for_the_same_target_is_refused() {
    let p = Probe::new();
    let resident = Resident::start(&p);
    p.save(direct("native"));
    let before = p.live_text();
    // Same store, then a different store claiming the same Codex home.
    assert_eq!(
        p.serve_failure(&p.data, &p.codex),
        "backend_already_running"
    );
    let other = p.root.path().join("other-store");
    fs::create_dir(&other).unwrap();
    assert_eq!(p.serve_failure(&other, &p.codex), "backend_already_running");
    assert!(!other.join("cc-switch.db").exists());
    assert_eq!(p.live_text(), before);
    resident.shutdown(&p, json!({}));
}

#[test]
fn mycodex_official_settings_survive_provider_switches() {
    let p = Probe::new();
    let resident = Resident::start(&p);
    p.save(direct("base"));
    p.save(direct("other"));
    p.save(form(
        "routed",
        "chat_completions",
        "https://routed.example.invalid/v1",
        "routed-model",
    ));
    // MiMo is on the web-search reject list; its row does not set web_search.
    p.save(form(
        "mimo",
        "responses",
        "https://api.xiaomimimo.com/v1",
        "mimo-v2-pro",
    ));
    let settings = "model_reasoning_summary = \"detailed\"\nmodel_verbosity = \"low\"\nweb_search = \"live\"\n";
    fs::write(
        p.codex.join("config.toml"),
        format!("{settings}{}", p.live_text()),
    )
    .unwrap();
    for id in ["other", "routed", "base", "mimo", "routed", "other"] {
        p.apply(id);
        let live = p.live();
        for (key, value) in [
            ("model_reasoning_summary", "detailed"),
            ("model_verbosity", "low"),
            ("web_search", "live"),
        ] {
            assert_eq!(
                live[key].as_str(),
                Some(value),
                "{key} after switching to {id}"
            );
        }
    }
    resident.shutdown(&p, json!({}));
}
#[test]
fn exclusive_settings_a_provider_overrides_return_to_the_users_value() {
    let p = Probe::new();
    let mut resident = Resident::start(&p);
    p.save(direct("base"));
    p.save(direct("other"));
    // The SudoCode.us preset row sets model_verbosity = "high".
    let presets = p.ok("gui/preset/list", json!({}));
    let sudo = presets["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|preset| preset["name"] == "SudoCode.us")
        .unwrap()
        .clone();
    let mut row = form(
        "sudo",
        "responses",
        sudo["baseUrl"].as_str().unwrap(),
        sudo["model"].as_str().unwrap(),
    );
    row["presetId"] = sudo["id"].clone();
    p.save(row);
    fs::write(
        p.codex.join("config.toml"),
        format!("model_verbosity = \"low\"\n{}", p.live_text()),
    )
    .unwrap();
    let verbosity = || {
        p.live()
            .get("model_verbosity")
            .and_then(|v| v.as_str().map(str::to_string))
    };

    p.apply("sudo");
    assert_eq!(verbosity().as_deref(), Some("high"));
    p.apply("base");
    assert_eq!(
        verbosity().as_deref(),
        Some("low"),
        "leaving the provider restores the user value"
    );

    // The remembered value survives a resident restart.
    p.apply("sudo");
    resident.shutdown(&p, json!({}));
    resident = Resident::start(&p);
    p.apply("other");
    assert_eq!(verbosity().as_deref(), Some("low"));

    // A choice made while the provider is active wins over the remembered one.
    p.apply("sudo");
    let text = p
        .live_text()
        .replace("model_verbosity = \"high\"", "model_verbosity = \"medium\"");
    fs::write(p.codex.join("config.toml"), text).unwrap();
    p.apply("base");
    assert_eq!(verbosity().as_deref(), Some("medium"));
    p.apply("sudo");
    assert_eq!(verbosity().as_deref(), Some("high"));
    p.apply("other");
    assert_eq!(verbosity().as_deref(), Some("medium"));
    resident.shutdown(&p, json!({}));
}

#[test]
fn session_model_choices_are_kept_per_provider_across_switches() {
    let p = Probe::new();
    let resident = Resident::start(&p);
    p.save(direct("a"));
    p.save(direct("b"));
    p.save(form(
        "routed",
        "chat_completions",
        "https://routed.example.invalid/v1",
        "routed-model",
    ));
    let choice = || {
        let live = p.live();
        (
            live["model"].as_str().unwrap().to_string(),
            live.get("model_reasoning_effort")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        )
    };
    // MyCodex's session settings write these keys through the App Server.
    let choose = |model: &str, effort: Option<&str>| {
        let mut doc: toml_edit::DocumentMut = p.live_text().parse().unwrap();
        doc["model"] = toml_edit::value(model);
        match effort {
            Some(effort) => doc["model_reasoning_effort"] = toml_edit::value(effort),
            None => {
                doc.remove("model_reasoning_effort");
            }
        }
        fs::write(p.codex.join("config.toml"), doc.to_string()).unwrap();
    };
    let expect =
        |model: &str, effort: Option<&str>| (model.to_string(), effort.map(str::to_string));

    choose("a-chosen", Some("high"));
    p.apply("b");
    assert_eq!(choice(), expect("b-model", None));
    p.apply("a");
    assert_eq!(
        choice(),
        expect("a-chosen", Some("high")),
        "direct choice returns"
    );

    p.apply("routed");
    choose("routed-chosen", Some("xhigh"));
    p.apply("a");
    assert_eq!(choice(), expect("a-chosen", Some("high")));
    p.apply("routed");
    assert_eq!(
        choice(),
        expect("routed-chosen", Some("xhigh")),
        "routed choice returns"
    );

    // Resetting the effort to the default is a choice too.
    choose("routed-chosen", None);
    p.apply("b");
    p.apply("routed");
    assert_eq!(choice(), expect("routed-chosen", None));

    // Saving the current provider with a new model and routing need keeps the form's value.
    p.apply("a");
    choose("a-live", Some("low"));
    let current = p.ok("gui/provider/get", json!({"providerId":"a"}));
    let mut edit = form(
        "a",
        "chat_completions",
        "https://a.example.invalid/v1",
        "a-form",
    );
    edit["expectedVersion"] = current["version"].clone();
    p.save(edit);
    assert_eq!(choice().0, "a-form");
    p.apply("b");
    p.apply("a");
    assert_eq!(choice().0, "a-form");
    resident.shutdown(&p, json!({}));
}
#[cfg(unix)]
fn endpoint_dir(p: &Probe) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let codex = p.codex.canonicalize().unwrap();
    let digest = format!("{:x}", Sha256::digest(codex.as_os_str().as_bytes()));
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from("/tmp").join(format!("mycodex-auth-core-{uid}-{}", &digest[..32]))
}

#[cfg(unix)]
#[test]
fn resident_restores_endpoint_files_removed_from_tmp() {
    let p = Probe::new();
    let resident = Resident::start(&p);
    p.save(direct("native"));
    let endpoint = endpoint_dir(&p);
    assert!(endpoint.join("management.sock").exists());
    // macOS tmp_cleaner and systemd-tmpfiles delete idle files under /tmp.
    fs::remove_file(endpoint.join("management.sock")).unwrap();
    fs::remove_file(endpoint.join("management.lock")).unwrap();
    // A second process must not replace the endpoint of the resident that owns the store.
    assert_eq!(
        p.serve_failure(&p.data, &p.codex),
        "backend_already_running"
    );
    let status = br#"{"jsonrpc":"2.0","id":1,"method":"status","params":{}}"#;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    loop {
        let output = p.raw(status, &p.codex);
        if output.status.success() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "endpoint was not restored: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(endpoint.join("management.lock").exists());
    resident.shutdown(&p, json!({}));
}

#[test]
fn resident_routes_chat_and_anthropic_then_returns_direct() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (address, rx) = upstream(&runtime);
    let p = Probe::new();
    let daemon = Resident::start(&p);
    let base = format!("http://{address}/v1");
    p.save(direct("native"));
    p.save(form("chat", "chat_completions", &base, "test-model"));
    p.save(form("anthropic", "anthropic", &base, "test-model"));
    let mut token = String::new();
    for (id, path) in [
        ("chat", "/v1/chat/completions"),
        ("anthropic", "/v1/messages"),
    ] {
        p.apply(id);
        let route = p.route();
        assert_eq!(route["mode"], "proxy");
        assert_eq!(route["takeover"], true);
        assert_eq!(route["running"], true);
        assert_eq!(route["accepting"], true);
        let port = route["port"].as_u64().unwrap();
        assert!(p.live_text().contains(&format!("127.0.0.1:{port}")));
        token = p.route_token();
        let response = route_request(&runtime, port, &token);
        assert_eq!(response["object"], "response", "{response}");
        let (actual_path, body) = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(actual_path, path);
        assert!(body.get("messages").is_some());
    }
    // A restarted host reattaches the route on a newly bound port.
    drop(daemon);
    let restarted = Resident::start(&p);
    let route = p.route();
    assert_eq!(route["accepting"], true);
    let port = route["port"].as_u64().unwrap();
    assert!(p.live_text().contains(&format!("127.0.0.1:{port}")));
    assert_eq!(p.route_token(), token);
    let response = route_request(&runtime, port, &token);
    assert_eq!(response["object"], "response", "{response}");
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    p.apply("native");
    let route = p.route();
    assert_eq!(route["takeover"], false);
    assert_eq!(route["accepting"], false);
    assert!(p.live_text().contains("native.example.invalid"));
    assert!(!p.live_text().contains("127.0.0.1"));
    assert!(!p.live_text().contains(HEADER));
    restarted.shutdown(&p, json!({"forUpdate":false}));
}

#[test]
fn streaming_requests_block_mutation_and_old_connections_cannot_route_direct() {
    streaming_switch(false);
}

#[test]
fn external_switch_during_stream_drains_original_request_and_blocks_new_requests() {
    streaming_switch(true);
}

fn streaming_switch(external: bool) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let released = release.clone();
    let address = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(move || {
            let arrived_tx = arrived_tx.clone();
            let released = released.clone();
            async move {
                let stream = async_stream::stream! {
                    arrived_tx.send(()).unwrap();
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n"));
                    released.notified().await;
                    yield Ok(bytes::Bytes::from_static(b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"));
                };
                axum::response::Response::builder().header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream)).unwrap()
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    });
    let p = Probe::new();
    let _daemon = Resident::start(&p);
    let chat = form(
        "chat",
        "chat_completions",
        &format!("http://{address}/v1"),
        "test-model",
    );
    p.save(chat.clone());
    p.save(direct("native"));
    let port = p.route()["port"].as_u64().unwrap();
    let url = format!("http://127.0.0.1:{port}/v1/responses");
    let client = reqwest::Client::new();
    let request_client = client.clone();
    let request_url = url.clone();
    let route_token = p.route_token();
    let request_token = route_token.clone();
    let request = runtime.spawn(async move {
        request_client
            .post(request_url)
            .header(HEADER, request_token)
            .json(&json!({"model":"test-model","input":"hi","stream":true}))
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    arrived_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let before = p.live_text();
    let selected = p.ok("gui/provider/get", json!({"providerId":"native"}));
    let mut edit = chat.clone();
    edit["expectedVersion"] =
        p.ok("gui/provider/get", json!({"providerId":"chat"}))["version"].clone();
    for (method, params) in [
        (
            "gui/provider/apply",
            json!({"providerId":"native","expectedVersion":selected["version"],
            "expectedFingerprint":"unused","operationId":"blocked-apply"}),
        ),
        ("gui/provider/save", edit),
        ("backend/shutdown", json!({})),
        ("backend/shutdown", json!({"forUpdate":true})),
        ("backend/shutdown", json!({"forUpdate":false})),
    ] {
        assert_eq!(
            p.call(method, params)["error"]["message"],
            "route_requests_active",
            "{method}"
        );
        assert_eq!(p.live_text(), before);
    }
    let external_config = "model='external'\nmodel_provider='external'\n[model_providers.external]\nname='External'\nbase_url='https://external.example.invalid/v1'\nrequires_openai_auth=true\n";
    let external_auth = r#"{"OPENAI_API_KEY":"synthetic-secret-external"}"#;
    if external {
        fs::write(p.codex.join("config.toml"), external_config).unwrap();
        fs::write(p.codex.join("auth.json"), external_auth).unwrap();
        let state = p.ok("gui/provider/list", json!({}));
        assert_eq!(state["liveState"], "unavailable");
        assert_eq!(state["syncError"], "route_requests_active");
        assert_eq!(state["route"]["accepting"], false);
        let blocked = runtime
            .block_on(
                client
                    .post(&url)
                    .header(HEADER, &route_token)
                    .json(&json!({"input":"must not forward"}))
                    .send(),
            )
            .unwrap();
        assert_eq!(blocked.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    }
    release.notify_one();
    let response = runtime.block_on(request).unwrap();
    assert!(
        response.contains("response.completed"),
        "stream did not finish: {response}"
    );
    if external {
        let state = p.ok("gui/provider/list", json!({}));
        assert_eq!(state["liveState"], "current");
        assert_eq!(state["providers"].as_array().unwrap().len(), 3);
        assert_eq!(state["route"]["takeover"], false);
        assert_eq!(p.live_text(), external_config);
        assert_eq!(
            fs::read_to_string(p.codex.join("auth.json")).unwrap(),
            external_auth
        );
    } else {
        p.apply("native");
    }
    // Reuse the HTTP pool: upstream stop alone leaves detached keep-alive tasks.
    let old = runtime.block_on(async {
        client
            .post(url)
            .header(HEADER, route_token)
            .json(&json!({"model":"test-model","input":"must not forward","stream":false}))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
    });
    if let Ok(old) = old {
        assert_eq!(old.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    }
    assert!(arrived_rx.try_recv().is_err());
    assert_eq!(p.route()["accepting"], false);
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn official_conversion_reserved_header_and_external_overwrite_are_rejected() {
    let p = Probe::new();
    let _daemon = Resident::start(&p);
    p.save(form(
        "chat",
        "chat_completions",
        "https://example.invalid/v1",
        "test-model",
    ));
    let before = p.live_text();
    for upstream in [
        "https://api.openai.com/v1",
        "https://chatgpt.com/backend-api/codex",
    ] {
        assert_eq!(
            p.call(
                "gui/provider/save",
                form("bad", "chat_completions", upstream, "test-model")
            )["error"]["message"],
            "official_requires_native_responses"
        );
    }
    let mut reserved = form(
        "reserved",
        "chat_completions",
        "https://example.invalid/v1",
        "test-model",
    );
    reserved["advanced"] = json!({"requestHeaders":{"X-MyCodex-Route-Token":"synthetic-secret-x"}});
    assert_eq!(
        p.call("gui/provider/save", reserved)["error"]["message"],
        "reserved_route_header"
    );
    assert_eq!(p.live_text(), before);
    p.save(direct("native"));
    // An external switch after preflight is adopted, never overwritten by the
    // stale apply that was planned before it.
    let selected = p.ok("gui/provider/get", json!({"providerId":"native"}));
    let preflight = p.ok(
        "gui/provider/preflight",
        json!({"providerId":"native","expectedVersion":selected["version"]}),
    );
    let external = "model = \"externally-selected\"\n";
    fs::write(p.codex.join("config.toml"), external).unwrap();
    fs::write(p.codex.join("auth.json"), "{}").unwrap();
    assert_eq!(
        p.call(
            "gui/provider/apply",
            json!({"providerId":"native","expectedVersion":selected["version"],
                "expectedFingerprint":preflight["fingerprint"],"operationId":"stale-apply"})
        )["error"]["message"],
        "config_conflict"
    );
    assert_eq!(p.live_text(), external);
    let state = p.ok("gui/provider/list", json!({}));
    assert_eq!(state["currentProviderId"], "");
    assert_eq!(state["liveState"], "signed_out");
    assert_eq!(state["route"]["accepting"], false);
    assert_eq!(state["route"]["takeover"], false);
    p.ok("backend/shutdown", json!({}));
    assert_eq!(p.live_text(), external);
    assert_eq!(fs::read_to_string(p.codex.join("auth.json")).unwrap(), "{}");
}

#[test]
fn failed_route_bind_can_return_direct_without_stale_mode() {
    let p = Probe::new();
    Resident::start(&p).shutdown(&p, json!({}));
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    connection
        .execute(
            "UPDATE proxy_config SET listen_port=?1",
            [occupied.local_addr().unwrap().port()],
        )
        .unwrap();
    drop(connection);
    let _daemon = Resident::start(&p);
    p.save(direct("native"));
    p.save(form(
        "chat",
        "chat_completions",
        "https://example.invalid/v1",
        "test-model",
    ));
    let before = p.live_text();
    assert_eq!(p.try_apply("chat")["error"]["message"], "operation_unknown");
    let route = p.route();
    assert_eq!(route["accepting"], false);
    assert_eq!(route["takeover"], false);
    assert_eq!(
        p.live_text(),
        before,
        "a failed start must not publish a route"
    );
    p.ok("gui/provider/list", json!({}));
    p.apply("native");
    assert!(!p.live_text().contains("127.0.0.1"));
    assert_eq!(p.route()["mode"], "direct");
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn external_switch_while_stopped_is_kept_on_restart() {
    let p = Probe::new();
    let daemon = Resident::start(&p);
    p.save(form(
        "chat",
        "chat_completions",
        "https://example.invalid/v1",
        "test-model",
    ));
    assert_eq!(p.route()["takeover"], true);
    daemon.shutdown(&p, json!({"forUpdate":true}));
    let external_config = "model='external'\nmodel_provider='external'\n[model_providers.external]\nname='External'\nbase_url='https://external.example.invalid/v1'\nrequires_openai_auth=true\n";
    let external_auth = r#"{"OPENAI_API_KEY":"synthetic-secret-external"}"#;
    fs::write(p.codex.join("config.toml"), external_config).unwrap();
    fs::write(p.codex.join("auth.json"), external_auth).unwrap();
    let restarted = Resident::start(&p);
    assert_eq!(p.live_text(), external_config);
    assert_eq!(
        fs::read_to_string(p.codex.join("auth.json")).unwrap(),
        external_auth
    );
    let route = p.route();
    assert_eq!(route["takeover"], false);
    assert_eq!(route["running"], false);
    let state = p.ok("gui/provider/list", json!({}));
    assert_eq!(state["liveState"], "current");
    assert_eq!(state["providers"].as_array().unwrap().len(), 2);
    restarted.shutdown(&p, json!({"forUpdate":false}));
    assert_eq!(p.live_text(), external_config);
}

#[test]
fn existing_store_cannot_redirect_upstream_writes_outside_its_directory() {
    let p = Probe::new();
    Resident::start(&p).shutdown(&p, json!({}));
    let outside = p.root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, "must stay unchanged").unwrap();
    let link = p.data.join("redirect");
    #[cfg(windows)]
    {
        // Directory junctions do not require symlink privileges on Windows.
        let output = Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(output.status.success(), "cannot create test junction");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let error = p.serve_failure(&p.data, &p.codex);
    // Remove only the link itself, never recursively remove its target.
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
    assert_eq!(error, "data_dir_contains_link");
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "must stay unchanged");
    assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
}

#[test]
fn store_ownership_target_binding_and_request_validation_fail_closed() {
    let p = Probe::new();
    fs::write(p.data.join("existing.txt"), "unrelated data").unwrap();
    assert_eq!(p.serve_failure(&p.data, &p.codex), "data_dir_not_owned");
    fs::remove_file(p.data.join("existing.txt")).unwrap();
    Resident::start(&p).shutdown(&p, json!({}));
    let second = p.root.path().join("second-codex");
    fs::create_dir(&second).unwrap();
    assert_eq!(p.serve_failure(&p.data, &second), "target_mismatch");
    assert_eq!(fs::read_dir(&second).unwrap().count(), 0);
    assert_eq!(p.serve_failure(&p.data, &p.data), "overlapping_directories");
    let daemon = Resident::start(&p);
    assert!(!p
        .raw(b"not json synthetic-secret", &p.codex)
        .status
        .success());
    assert_eq!(
        p.call("unknown", json!({}))["error"]["message"],
        "method_not_supported"
    );
    daemon.shutdown(&p, json!({}));
    fs::write(p.data.join("codex_oauth_auth.json"), "invalid").unwrap();
    assert_eq!(p.serve_failure(&p.data, &p.codex), "account_store_invalid");
}

#[test]
fn route_credentials_are_target_private_persistent_and_never_backfilled() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let first = Probe::new();
    let daemon = Resident::start(&first);
    first.save(direct("native"));
    first.save(form(
        "chat",
        "chat_completions",
        "https://example.invalid/v1",
        "test-model",
    ));
    first.apply("chat");
    let token = first.route_token();
    assert_eq!(token.len(), 64);
    assert!(first.live_text().contains("PROXY_MANAGED"));
    let other = Probe::new();
    let other_daemon = Resident::start(&other);
    other.save(form(
        "other",
        "anthropic",
        "https://example.invalid/v1",
        "test-model",
    ));
    let other_token = other.route_token();
    assert!(token != other_token);
    let port = first.route()["port"].as_u64().unwrap();
    let client = reqwest::Client::new();
    for credential in [None, Some("wrong-token"), Some(other_token.as_str())] {
        for path in ["/health", "/v1/responses"] {
            let response = runtime.block_on(async {
                let mut request = client
                    .post(format!("http://127.0.0.1:{port}{path}"))
                    .json(&json!({"model":"test-model","input":"must not forward"}));
                if let Some(value) = credential {
                    request = request.header(HEADER, value);
                }
                request.send().await.unwrap()
            });
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
    }
    let response = runtime.block_on(async {
        client
            .get(format!("http://127.0.0.1:{port}/health"))
            .header(HEADER, &token)
            .send()
            .await
            .unwrap()
    });
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = runtime.block_on(async {
        client
            .get(format!("http://127.0.0.1:{port}/health"))
            .header(HEADER, &token)
            .header(HEADER, &token)
            .send()
            .await
            .unwrap()
    });
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    // Neither private RPC summaries nor provider rows contain the host capability.
    assert!(!first.ok("status", json!({})).to_string().contains(&token));
    assert!(!first
        .ok("gui/provider/list", json!({}))
        .to_string()
        .contains(&token));
    let connection = rusqlite::Connection::open(first.data.join("cc-switch.db")).unwrap();
    let stored: String = connection
        .query_row(
            "SELECT settings_config FROM providers WHERE id='chat' AND app_type='codex'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stored.contains(&token) && !stored.contains(HEADER));
    assert!(stored.contains("synthetic-secret-chat"));
    drop(connection);
    drop(daemon);
    let restarted = Resident::start(&first);
    assert!(first.route_token() == token);
    first.apply("native");
    assert!(!first.live_text().contains(HEADER) && !first.live_text().contains(&token));
    first.apply("chat");
    assert!(first.route_token() == token);
    first.apply("native");
    restarted.shutdown(&first, json!({}));
    assert!(!first.live_text().contains(HEADER) && !first.live_text().contains(&token));
    other_daemon.shutdown(&other, json!({"forUpdate":true}));
}

#[test]
fn altered_route_credentials_are_treated_as_an_external_change() {
    let p = Probe::new();
    let daemon = Resident::start(&p);
    p.save(form(
        "chat",
        "chat_completions",
        "https://example.invalid/v1",
        "test-model",
    ));
    let token = p.route_token();
    let changed = p.live_text().replace(&token, "external-change");
    fs::write(p.codex.join("config.toml"), &changed).unwrap();
    assert_eq!(
        p.ok("gui/provider/list", json!({}))["liveState"],
        "unavailable"
    );
    assert!(p.live_text() == changed);
    assert_eq!(p.route()["accepting"], false);
    daemon.shutdown(&p, json!({"forUpdate":true}));
    assert!(p.live_text() == changed);
    // The changed client still targets this host's proxy placeholder: the route
    // is not reattached, and v4 retires the unusable route key fields instead of
    // leaving Codex pointed at a proxy that rejects it.
    let _restarted = Resident::start(&p);
    assert_eq!(p.route()["takeover"], false);
    assert_eq!(p.route()["running"], false);
    let live = p.live();
    assert!(live.get("model_provider").is_none(), "{live}");
    assert!(!p.live_text().contains("PROXY_MANAGED"));
    assert!(!p.live_text().contains(&token));
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn maintenance_update_preserves_the_route_and_uninstall_requires_direct() {
    let p = Probe::new();
    let daemon = Resident::start(&p);
    p.save(form(
        "conversion",
        "chat_completions",
        "http://127.0.0.1:9/v1",
        "test-model",
    ));
    p.save(direct("direct"));
    let token = p.route_token();
    let auth = fs::read(p.codex.join("auth.json")).ok();
    assert_eq!(
        p.call("backend/shutdown", json!({"forUpdate":"true"}))["error"]["message"],
        "invalid_params"
    );
    assert_eq!(
        p.call("backend/shutdown", json!({"forUpdate":false}))["error"]["message"],
        "route_required"
    );
    assert_eq!(p.route()["running"], true);
    let before = p.live_text();
    daemon.shutdown(&p, json!({"forUpdate":true}));
    assert_eq!(p.live_text(), before);
    assert_eq!(fs::read(p.codex.join("auth.json")).ok(), auth);
    let replacement = Resident::start(&p);
    let route = p.route();
    assert_eq!(route["accepting"], true);
    assert_eq!(route["takeover"], true);
    assert_eq!(p.route_token(), token);
    let port = route["port"].as_u64().unwrap();
    assert!(p.live_text().contains(&format!("127.0.0.1:{port}")));
    p.apply("direct");
    let direct = p.live_text();
    replacement.shutdown(&p, json!({"forUpdate":false}));
    assert_eq!(p.live_text(), direct);
    assert!(p.data.join("cc-switch.db").exists());
}

#[test]
fn v3_store_migrates_and_direct_switch_preserves_user_settings() {
    let Some(v3) = v3_host() else {
        return;
    };
    let p = Probe::new();
    let old = Resident::start_with(&p, &v3, 2);
    for id in ["alpha", "beta"] {
        p.call_with(&v3, "gui/provider/save", direct(id));
    }
    old.shutdown_with(&p, &v3);
    let v3_live = p.live_text();
    assert!(v3_live.contains("alpha.example.invalid"), "{v3_live}");
    // Content a user or another tool adds after the switch.
    let user = "approval_policy = \"on-request\"\n# kept by the user\n";
    let mcp = "\n[mcp_servers.user_tool]\ncommand = \"user-tool\"\nargs = [\"--flag\"]\n";
    fs::write(p.codex.join("config.toml"), format!("{user}{v3_live}{mcp}")).unwrap();

    let resident = Resident::start(&p);
    let list = p.ok("gui/provider/list", json!({}));
    assert_eq!(list["providers"].as_array().unwrap().len(), 2, "{list}");
    assert_eq!(list["currentProviderId"], "alpha");
    p.apply("beta");
    let live = p.live_text();
    let parsed: toml::Value = toml::from_str(&live).unwrap();
    // v4 writes every third-party route as the fixed `custom` table.
    assert_eq!(parsed["model_provider"].as_str(), Some("custom"), "{live}");
    assert_eq!(parsed["model"].as_str(), Some("beta-model"), "{live}");
    assert_eq!(
        parsed["model_providers"]["custom"]["base_url"].as_str(),
        Some("https://beta.example.invalid/v1")
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
        p.ok("gui/provider/list", json!({}))["currentProviderId"],
        "beta"
    );
    resident.shutdown(&p, json!({"forUpdate":true}));
}

#[test]
fn v3_route_migrates_and_keeps_routing_on_v4() {
    let Some(v3) = v3_host() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (address, rx) = upstream(&runtime);
    let p = Probe::new();
    let old = Resident::start_with(&p, &v3, 2);
    p.call_with(
        &v3,
        "gui/provider/save",
        form(
            "chat",
            "chat_completions",
            &format!("http://{address}/v1"),
            "test-model",
        ),
    );
    let v3_live = p.live_text();
    assert!(v3_live.contains(HEADER), "{v3_live}");
    old.shutdown_with(&p, &v3);
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    let backups: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM proxy_live_backup WHERE app_type='codex'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backups, 1, "v3 keeps a takeover backup");
    drop(connection);

    let resident = Resident::start(&p);
    let route = p.route();
    assert_eq!(route["takeover"], true, "{route}");
    assert_eq!(route["accepting"], true);
    let port = route["port"].as_u64().unwrap();
    let token = p.route_token();
    let response = route_request(&runtime, port, &token);
    assert_eq!(response["object"], "response", "{response}");
    let (path, _) = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(path, "/v1/chat/completions");
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    let backups: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM proxy_live_backup WHERE app_type='codex'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backups, 0, "the legacy backup moves into the store");
    drop(connection);
    let drained = p.data.join("backups").join("proxy-live-backup");
    assert_eq!(fs::read_dir(drained).unwrap().count(), 1);
    resident.shutdown(&p, json!({"forUpdate":true}));
}

impl Resident {
    fn shutdown_with(mut self, p: &Probe, executable: &Path) {
        let response = p.call_with(executable, "backend/shutdown", json!({"forUpdate":true}));
        assert_eq!(response["result"]["status"], "shutting_down", "{response}");
        assert!(self.0.wait().unwrap().success());
    }
}
