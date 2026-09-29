//! Process-boundary checks using only temporary files and synthetic credentials.
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Probe {
    root: TempDir,
    data: PathBuf,
    codex: PathBuf,
    unrelated: PathBuf,
    resident: bool,
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
            resident: false,
        }
    }

    fn raw(&self, input: &[u8], target: &std::path::Path) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mycodex-auth-host"))
            .args([if self.resident { "rpc" } else { "request" }, "--data-dir"])
            .arg(&self.data)
            .arg("--codex-home")
            .arg(target)
            .env("CC_SWITCH_TEST_HOME", &self.unrelated)
            .env("HOME", &self.unrelated)
            .env("USERPROFILE", &self.unrelated)
            .env("LOCALAPPDATA", &self.unrelated)
            .env("CODEX_HOME", &self.unrelated)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
            "explicit target must cover settings, DB and Codex paths"
        );
        output
    }

    fn call(&self, method: &str, params: Value) -> Value {
        let input =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                .unwrap();
        let output = self.raw(&input, &self.codex);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["id"], 1);
        response
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn add(&self, provider: Value) {
        self.ok("provider/add", json!({"provider":provider}));
    }

    fn switch(&self, id: &str) {
        self.ok("provider/switch", json!({"providerId":id}));
    }

    fn live_text(&self) -> String {
        fs::read_to_string(self.codex.join("config.toml")).unwrap()
    }

    fn live(&self) -> toml::Value {
        toml::from_str(&self.live_text()).unwrap()
    }

    fn route_token(&self) -> String {
        let live = self.live();
        let source = live["model_provider"].as_str().unwrap();
        live["model_providers"][source]["http_headers"]["x-mycodex-route-token"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn api(id: &str, model: &str, shared: bool) -> Value {
    json!({"id":id,"name":id,"category":"custom",
        "meta":{"commonConfigEnabled":shared,"apiFormat":"openai_responses"},
        "settingsConfig":{"auth":{"OPENAI_API_KEY":format!("synthetic-secret-{id}")},
        "config":format!("model = \"{model}\"\nmodel_provider = \"{id}\"\n[model_providers.{id}]\nname = \"{id}\"\nbase_url = \"https://{id}.example.invalid/v1\"\nwire_api = \"responses\"\n")}})
}

fn account_token(id: &str, generation: u32) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let claims = json!({"sub":id,"email":format!("{id}@example.invalid"),"exp":4102444800u64,
        "generation":generation,"https://api.openai.com/auth":{"chatgpt_account_id":id}});
    format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}

fn account(id: &str) -> Value {
    json!({"id":id,"name":id,"category":"official",
        "meta":{"commonConfigEnabled":true},
        "settingsConfig":{"auth":{"auth_mode":"chatgpt","OPENAI_API_KEY":null,
            "tokens":{"access_token":account_token(id, 1),"id_token":account_token(id, 1),
                "refresh_token":format!("synthetic-secret-refresh-{id}"),"account_id":id},
            "last_refresh":"2024-01-01T00:00:00Z"},
        "config":"model = \"test-model\"\n"}})
}

struct Resident(std::process::Child);

impl Resident {
    fn start(p: &mut Probe) -> Self {
        use std::io::BufRead;
        let mut child = Command::new(env!("CARGO_BIN_EXE_mycodex-auth-host"))
            .args(["serve", "--data-dir"])
            .arg(&p.data)
            .arg("--codex-home")
            .arg(&p.codex)
            .env("CC_SWITCH_TEST_HOME", &p.unrelated)
            .env("HOME", &p.unrelated)
            .env("USERPROFILE", &p.unrelated)
            .env("LOCALAPPDATA", &p.unrelated)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
        let ready = rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        if ready.is_empty() {
            use std::io::Read;
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
        assert_eq!(ready["protocolVersion"], 2);
        assert_eq!(ready["hostVersion"], "0.2.3");
        assert_eq!(
            ready["sourceRevision"],
            env!("MYCODEX_CORE_SOURCE_REVISION")
        );
        assert_eq!(ready["codexHome"], json!(p.codex.canonicalize().unwrap()));
        assert_eq!(ready["dataDir"], json!(p.data.canonicalize().unwrap()));
        p.resident = true;
        resident
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn build_identity_is_independent_of_runtime_checkout_and_matches_resident() {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut p = Probe::new();
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
    assert_eq!(identity["protocolVersion"], 2);
    assert_eq!(identity["hostVersion"], "0.2.3");
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
        "e0f70019b2758f5b6b9a04dd60e4689481a0c0ac"
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
    let probe_status = p.ok("status", json!({}));
    let _resident = Resident::start(&mut p);
    let resident_status = p.ok("status", json!({}));
    for (key, expected) in identity.as_object().unwrap() {
        assert_eq!(probe_status[key], *expected, "probe field {key}");
        assert_eq!(resident_status[key], *expected, "resident field {key}");
    }
    assert_eq!(
        resident_status["codexHome"],
        json!(p.codex.canonicalize().unwrap())
    );
    p.ok("backend/shutdown", json!({}));
}

fn routed(id: &str, format: &str, upstream: &str) -> Value {
    let mut provider = api(id, "test-model", true);
    provider["meta"]["apiFormat"] = json!(format);
    provider["settingsConfig"]["config"] = json!(format!(
        "model = \"test-model\"\nmodel_provider = \"{id}\"\n[model_providers.{id}]\nname = \"{id}\"\nbase_url = \"{upstream}\"\nwire_api = \"responses\"\n"));
    provider
}

#[test]
fn resident_ipc_account_methods_and_target_lock_use_native_state() {
    let mut p = Probe::new();
    let _daemon = Resident::start(&mut p);
    assert_eq!(p.ok("account/list", json!({}))["accounts"], json!([]));
    assert_eq!(
        p.ok("account/login/cancel", json!({"deviceCode":"missing"}))["cancelled"],
        false
    );
    assert_eq!(
        p.call("account/login/poll", json!({"deviceCode":"missing"}))["error"]["message"],
        "login_failed"
    );
    assert_eq!(
        p.call("account/default", json!({"accountId":"missing"}))["error"]["message"],
        "account_not_found"
    );
    p.add(account("a"));
    p.add(account("b"));
    p.switch("b");
    assert_eq!(p.ok("status", json!({}))["route"]["running"], false);
    p.resident = false;
    let output = p.raw(br#"{"jsonrpc":"2.0","id":1,"method":"status"}"#, &p.codex);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "backend_already_running"
    );
    p.resident = true;
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn resident_routes_chat_and_anthropic_then_restores_direct_accounts() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let address = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let tx = tx.clone();
            async move {
                let path = request.uri().path().to_owned();
                assert!(!request.headers().contains_key("x-mycodex-route-token"));
                let body = axum::body::to_bytes(request.into_body(), 1_000_000).await.unwrap();
                tx.send((path.clone(), serde_json::from_slice::<Value>(&body).unwrap())).unwrap();
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
    let mut p = Probe::new();
    let daemon = Resident::start(&mut p);
    p.add(routed(
        "chat",
        "openai_chat",
        &format!("http://{address}/v1"),
    ));
    p.add(routed(
        "anthropic",
        "anthropic",
        &format!("http://{address}/v1"),
    ));
    p.add(account("official"));
    p.add(api("native", "native-model", true));
    for (id, path) in [
        ("chat", "/v1/chat/completions"),
        ("anthropic", "/v1/messages"),
    ] {
        p.switch(id);
        let status = p.ok("status", json!({}));
        assert_eq!(status["route"]["running"], true);
        let port = status["route"]["port"].as_u64().unwrap();
        assert!(p.live_text().contains(&port.to_string()));
        let response = runtime.block_on(async {
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/responses"))
                .header("x-mycodex-route-token", p.route_token())
                .json(&json!({"model":"test-model","input":"hi","stream":false}))
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        });
        assert_eq!(response["object"], "response", "{response}");
        let (actual_path, body) = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(actual_path, path);
        assert!(body.get("messages").is_some());
    }
    // Kill with active takeover, then recover the same original port/backup.
    let before = p.live_text();
    let port = p.ok("status", json!({}))["route"]["port"].clone();
    drop(daemon);
    let _restarted = Resident::start(&mut p);
    assert_eq!(p.ok("status", json!({}))["route"]["port"], port);
    assert_eq!(p.live_text(), before);
    p.switch("official");
    assert_eq!(p.ok("status", json!({}))["route"]["running"], false);
    assert!(p.live().get("model_providers").is_none());
    let auth: Value =
        serde_json::from_slice(&fs::read(p.codex.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["tokens"]["account_id"], "official");
    p.switch("chat");
    p.switch("native");
    assert_eq!(p.ok("status", json!({}))["route"]["takeover"], false);
    assert!(p.live_text().contains("native.example.invalid"));
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn streaming_requests_block_mutation_and_old_connections_cannot_route_direct_accounts() {
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
    let mut p = Probe::new();
    let _daemon = Resident::start(&mut p);
    let provider = routed("chat", "openai_chat", &format!("http://{address}/v1"));
    p.add(provider.clone());
    p.add(account("official"));
    let status = p.ok("status", json!({}));
    let port = status["route"]["port"].as_u64().unwrap();
    let url = format!("http://127.0.0.1:{port}/v1/responses");
    let client = reqwest::Client::new();
    let request_client = client.clone();
    let request_url = url.clone();
    let route_token = p.route_token();
    let request_token = route_token.clone();
    let request = runtime.spawn(async move {
        request_client
            .post(request_url)
            .header("x-mycodex-route-token", request_token)
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
    for (method, params) in [
        ("provider/switch", json!({"providerId":"official"})),
        ("provider/update", json!({"provider":provider})),
        ("backend/shutdown", json!({})),
        ("backend/shutdown", json!({"forUpdate":true})),
        ("backend/shutdown", json!({"forUpdate":false})),
    ] {
        assert_eq!(
            p.call(method, params)["error"]["message"],
            "route_requests_active"
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
                    .header("x-mycodex-route-token", &route_token)
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
        assert_eq!(p.live_text(), external_config);
        assert_eq!(
            fs::read_to_string(p.codex.join("auth.json")).unwrap(),
            external_auth
        );
    } else {
        p.switch("official");
    }
    // Reuse the HTTP pool: upstream stop alone leaves detached keep-alive tasks.
    let old = runtime.block_on(async {
        client
            .post(url)
            .header("x-mycodex-route-token", route_token)
            .json(&json!({"model":"test-model","input":"must not forward","stream":false}))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
    });
    if let Ok(old) = old {
        assert_eq!(old.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    }
    assert!(arrived_rx.try_recv().is_err());
    assert_eq!(p.ok("status", json!({}))["route"]["accepting"], false);
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn resident_rejects_official_conversion_duplicate_add_and_external_live_overwrite() {
    let mut p = Probe::new();
    let _daemon = Resident::start(&mut p);
    p.add(routed("chat", "openai_chat", "https://example.invalid/v1"));
    p.add(account("official"));
    let before = p.live_text();
    assert_eq!(
        p.call("provider/add", json!({"provider":api("chat", "new", true)}))["error"]["message"],
        "provider_already_exists"
    );
    assert_eq!(p.live_text(), before);
    for upstream in [
        "https://api.openai.com/v1",
        "https://chatgpt.com/backend-api/codex",
    ] {
        assert_eq!(
            p.call(
                "provider/add",
                json!({"provider":routed("bad", "openai_chat", upstream)})
            )["error"]["message"],
            "official_requires_native_responses"
        );
    }
    for key in ["base_url", "baseURL"] {
        let mut provider = routed(
            "bad-precedence",
            "openai_chat",
            "https://example.invalid/v1",
        );
        provider["settingsConfig"][key] = json!("https://api.openai.com/v1");
        assert_eq!(
            p.call("provider/add", json!({"provider":provider}))["error"]["message"],
            "official_requires_native_responses"
        );
    }
    let external = "model = \"externally-selected\"\n";
    fs::write(p.codex.join("config.toml"), external).unwrap();
    fs::write(p.codex.join("auth.json"), "{}").unwrap();
    assert_eq!(
        p.call("provider/switch", json!({"providerId":"official"}))["error"]["message"],
        "config_conflict"
    );
    assert_eq!(p.live_text(), external);
    assert_eq!(p.ok("provider/list", json!({}))["currentProviderId"], "");
    assert_eq!(p.ok("status", json!({}))["route"]["accepting"], false);
    p.ok("backend/shutdown", json!({}));
    assert_eq!(p.live_text(), external);
    assert_eq!(fs::read_to_string(p.codex.join("auth.json")).unwrap(), "{}");
}

#[test]
fn resident_loads_native_account_store_and_uses_original_removal() {
    let mut p = Probe::new();
    p.ok("status", json!({}));
    let store = json!({"version":2,"default_account_id":"saved", "accounts":{"saved":{
        "account_id":"saved","chatgpt_account_id":"workspace","email":"test@example.invalid",
        "refresh_token":"synthetic-secret-refresh","authenticated_at":1,"token_updated_at_ms":1}}});
    fs::write(
        p.data.join("codex_oauth_auth.json"),
        serde_json::to_vec(&store).unwrap(),
    )
    .unwrap();
    let _daemon = Resident::start(&mut p);
    let accounts = p.ok("account/list", json!({}));
    assert_eq!(accounts["accounts"][0]["id"], "saved");
    assert_eq!(accounts["accounts"][0]["reauthRequired"], true);
    p.ok("account/default", json!({"accountId":"saved"}));
    p.ok("account/remove", json!({"accountId":"saved"}));
    assert_eq!(p.ok("account/list", json!({}))["accounts"], json!([]));
    let stored: Value =
        serde_json::from_slice(&fs::read(p.data.join("codex_oauth_auth.json")).unwrap()).unwrap();
    assert_eq!(stored["accounts"], json!({}));
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn failed_route_bind_can_switch_back_to_direct_without_stale_proxy_flags() {
    let mut p = Probe::new();
    p.ok("status", json!({}));
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    connection
        .execute(
            "UPDATE proxy_config SET listen_port=?1",
            [occupied.local_addr().unwrap().port()],
        )
        .unwrap();
    drop(connection);
    let _daemon = Resident::start(&mut p);
    p.add(account("official"));
    p.add(routed("chat", "openai_chat", "https://example.invalid/v1"));
    assert_eq!(
        p.call("provider/switch", json!({"providerId":"chat"}))["error"]["message"],
        "route_start_failed"
    );
    assert_eq!(p.ok("status", json!({}))["route"]["accepting"], false);
    // Refresh observes the original service's partial activation outcome
    // before a new explicit switch (as the GUI preflight does).
    p.ok("gui/provider/list", json!({}));
    p.switch("official");
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    let flags: i64 = connection
        .query_row("SELECT SUM(proxy_enabled) FROM proxy_config", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(flags, 0);
    assert!(p.live().get("model_providers").is_none());
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn recovery_finishes_a_partially_restored_backup_without_reenabling_route() {
    let mut p = Probe::new();
    let daemon = Resident::start(&mut p);
    p.add(routed("chat", "openai_chat", "https://example.invalid/v1"));
    drop(daemon);
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    let backup: String = connection
        .query_row(
            "SELECT original_config FROM proxy_live_backup WHERE app_type='codex'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let backup: Value = serde_json::from_str(&backup).unwrap();
    fs::write(
        p.codex.join("config.toml"),
        backup["config"].as_str().unwrap(),
    )
    .unwrap();
    fs::write(
        p.codex.join("auth.json"),
        serde_json::to_vec(&backup["auth"]).unwrap(),
    )
    .unwrap();
    drop(connection);
    let _daemon = Resident::start(&mut p);
    let status = p.ok("status", json!({}));
    assert_eq!(status["route"]["running"], false);
    assert_eq!(status["route"]["takeover"], false);
    assert!(p.live_text().contains("example.invalid"));
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn recovery_clears_orphaned_enabled_flag_without_changing_external_direct_files() {
    let mut p = Probe::new();
    p.add(account("official"));
    let before = p.live_text();
    let auth = fs::read(p.codex.join("auth.json")).unwrap();
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    connection
        .execute(
            "UPDATE proxy_config SET enabled=1,proxy_enabled=1 WHERE app_type='codex'",
            [],
        )
        .unwrap();
    drop(connection);
    let _daemon = Resident::start(&mut p);
    let status = p.ok("status", json!({}));
    assert_eq!(status["route"]["running"], false);
    assert_eq!(status["route"]["takeover"], false);
    assert_eq!(p.live_text(), before);
    assert_eq!(fs::read(p.codex.join("auth.json")).unwrap(), auth);
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn native_services_persist_update_and_backfill_across_processes() {
    let p = Probe::new();
    p.add(api("a", "model-a", true));
    assert_eq!(p.ok("provider/list", json!({}))["currentProviderId"], "a");
    assert_eq!(p.live()["model"].as_str(), Some("model-a"));
    p.add(api("b", "model-b", true));
    p.ok(
        "provider/update",
        json!({"provider":api("b", "model-b2", true)}),
    );
    assert_eq!(p.live()["model"].as_str(), Some("model-a"));
    p.ok(
        "provider/update",
        json!({"provider":api("a", "model-a2", true)}),
    );
    assert_eq!(p.live()["model"].as_str(), Some("model-a2"));

    fs::write(
        p.codex.join("config.toml"),
        p.live_text().replace("model-a2", "external-model"),
    )
    .unwrap();
    assert_eq!(p.ok("provider/live", json!({}))["model"], "external-model");
    p.switch("b");
    assert_eq!(p.live()["model"].as_str(), Some("model-b2"));
    let providers = p.ok("provider/list", json!({}));
    let a = providers["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "a")
        .unwrap();
    assert_eq!(a["model"], "external-model");
    p.switch("a");
    assert_eq!(p.live()["model"].as_str(), Some("external-model"));
    assert!(p.data.join("settings.json").exists());
    assert!(p.data.join("cc-switch.db").exists());
}

#[test]
fn common_preferences_tools_and_mcp_keep_upstream_semantics() {
    let p = Probe::new();
    p.add(api("a", "model-a", true));
    p.add(api("b", "model-b", true));
    p.add(api("c", "model-c", false));
    let skill = p.codex.join("skills/example/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "synthetic installed skill").unwrap();
    let plugin = p.codex.join("plugins/example/manifest.json");
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    fs::write(&plugin, "{}").unwrap();
    let config = format!("model_reasoning_summary = \"detailed\"\n{}\n[plugins.example]\nenabled = true\n[[skills.config]]\npath = \"example\"\nenabled = false\n[mcp_servers.echo]\ncommand = \"echo\"\nargs = [\"test\"]\n", p.live_text());
    fs::write(p.codex.join("config.toml"), &config).unwrap();
    assert_eq!(p.ok("mcp/import", json!({}))["imported"], 1);
    assert_eq!(p.live_text(), config, "MCP import is read-only for live");
    p.switch("b");
    let b = p.live();
    assert_eq!(b["model_reasoning_summary"].as_str(), Some("detailed"));
    assert_eq!(b["plugins"]["example"]["enabled"].as_bool(), Some(true));
    assert_eq!(b["skills"]["config"][0]["enabled"].as_bool(), Some(false));
    assert_eq!(b["mcp_servers"]["echo"]["command"].as_str(), Some("echo"));
    p.switch("c");
    let c = p.live();
    for key in ["model_reasoning_summary", "plugins", "skills"] {
        assert!(
            c.get(key).is_none(),
            "opt-out must follow upstream for {key}"
        );
    }
    assert_eq!(c["mcp_servers"]["echo"]["command"].as_str(), Some("echo"));
    p.switch("b");
    assert_eq!(
        p.live()["model_reasoning_summary"].as_str(),
        Some("detailed")
    );
    fs::write(
        p.codex.join("config.toml"),
        p.live_text()
            .replace("model_reasoning_summary = \"detailed\"\n", ""),
    )
    .unwrap();
    p.switch("a");
    assert!(p.live().get("model_reasoning_summary").is_none());
    assert_eq!(
        fs::read_to_string(skill).unwrap(),
        "synthetic installed skill"
    );
    assert_eq!(fs::read_to_string(plugin).unwrap(), "{}");
}

#[test]
fn native_chatgpt_auth_switches_direct_and_retains_rotated_live_token() {
    let mut p = Probe::new();
    let _daemon = Resident::start(&mut p);
    p.add(account("account-a"));
    p.add(account("account-b"));
    let auth_path = p.codex.join("auth.json");
    let mut auth: Value = serde_json::from_slice(&fs::read(&auth_path).unwrap()).unwrap();
    let rotated = account_token("account-a", 2);
    auth["tokens"]["access_token"] = json!(rotated);
    auth["tokens"]["refresh_token"] = json!("synthetic-secret-rotated");
    auth["last_refresh"] = json!("2025-01-01T00:00:00Z");
    fs::write(&auth_path, serde_json::to_vec(&auth).unwrap()).unwrap();
    p.switch("account-b");
    let b: Value = serde_json::from_slice(&fs::read(&auth_path).unwrap()).unwrap();
    assert_eq!(b["tokens"]["account_id"], "account-b");
    p.switch("account-a");
    let a: Value = serde_json::from_slice(&fs::read(&auth_path).unwrap()).unwrap();
    assert_eq!(a["tokens"]["access_token"], rotated);
    let live = p.live_text();
    for marker in ["127.0.0.1", "localhost", "PROXY_MANAGED"] {
        assert!(!live.contains(marker));
    }
    assert!(p.live().get("model_providers").is_none());
}

#[test]
fn unsupported_conversion_and_takeover_are_rejected_without_live_writes() {
    let p = Probe::new();
    p.add(api("a", "model-a", true));
    let before = p.live_text();
    for method in ["provider/add", "provider/update"] {
        let mut managed = account(if method == "provider/add" { "new" } else { "a" });
        managed["meta"]["authBinding"] =
            json!({"source":"managed_account","authProvider":"codex_oauth","accountId":"missing"});
        assert_eq!(
            p.call(method, json!({"provider":managed}))["error"]["message"],
            "managed_oauth_not_supported_in_probe"
        );
    }
    assert_eq!(
        p.ok("provider/list", json!({}))["providers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut conversion = api("conversion", "model", true);
    conversion["meta"]["apiFormat"] = json!("chat_completions");
    let response = p.call("provider/add", json!({"provider":conversion}));
    assert_eq!(
        response["error"]["message"],
        "conversion_not_supported_in_probe"
    );
    assert_eq!(p.live_text(), before);
    for key in ["apiFormat", "api_format"] {
        for format in ["anthropic", "openai_chat"] {
            let mut conversion = api("conversion", "model", true);
            conversion["settingsConfig"][key] = json!(format);
            assert_eq!(
                p.call("provider/add", json!({"provider":conversion}))["error"]["message"],
                "conversion_not_supported_in_probe"
            );
        }
    }
    let mut takeover = api("takeover", "model", true);
    takeover["settingsConfig"]["auth"]["OPENAI_API_KEY"] = json!("PROXY_MANAGED");
    assert_eq!(
        p.call("provider/add", json!({"provider":takeover}))["error"]["message"],
        "route_takeover_not_supported_in_probe"
    );
    assert_eq!(p.live_text(), before);
    fs::write(
        p.codex.join("auth.json"),
        r#"{"OPENAI_API_KEY":"PROXY_MANAGED"}"#,
    )
    .unwrap();
    let response = p.call(
        "provider/update",
        json!({"provider":api("a", "replacement", true)}),
    );
    assert_eq!(
        response["error"]["message"],
        "route_takeover_not_supported_in_probe"
    );
    assert_eq!(p.live_text(), before);
    fs::write(p.codex.join("config.toml"), "[invalid").unwrap();
    assert!(p
        .call("provider/switch", json!({"providerId":"a"}))
        .get("error")
        .is_some());
    assert_eq!(p.live_text(), "[invalid");
}

#[test]
fn existing_store_cannot_redirect_upstream_writes_outside_its_directory() {
    let p = Probe::new();
    p.ok("status", json!({}));
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
    let output = p.raw(br#"{"jsonrpc":"2.0","id":1,"method":"status"}"#, &p.codex);
    // Remove only the link itself, never recursively remove its target.
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "data_dir_contains_link"
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "must stay unchanged");
    assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
}

#[test]
fn store_ownership_target_binding_and_request_validation_fail_closed() {
    let p = Probe::new();
    let status = br#"{"jsonrpc":"2.0","id":1,"method":"status"}"#;
    fs::write(p.data.join("existing.txt"), "unrelated data").unwrap();
    let output = p.raw(status, &p.codex);
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "data_dir_not_owned"
    );
    fs::remove_file(p.data.join("existing.txt")).unwrap();
    assert!(p.raw(status, &p.codex).status.success());
    let second = p.root.path().join("second-codex");
    fs::create_dir(&second).unwrap();
    let output = p.raw(status, &second);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "target_mismatch"
    );
    assert_eq!(fs::read_dir(second).unwrap().count(), 0);
    let output = p.raw(status, &p.data);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "overlapping_directories"
    );
    assert!(!p
        .raw(b"not json synthetic-secret", &p.codex)
        .status
        .success());
    assert_eq!(
        p.call("unknown", json!({}))["error"]["message"],
        "method_not_supported"
    );
    fs::write(p.data.join("codex_oauth_auth.json"), "invalid").unwrap();
    let output = p.raw(status, &p.codex);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "managed_oauth_not_supported_in_probe"
    );
}

#[test]
fn route_credentials_are_target_private_persistent_and_never_backfilled() {
    const HEADER: &str = "x-mycodex-route-token";
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut first = Probe::new();
    let daemon = Resident::start(&mut first);
    first.add(routed("chat", "openai_chat", "https://example.invalid/v1"));
    first.add(api("native", "native-model", true));
    let token = first.route_token();
    assert_eq!(token.len(), 64);
    assert!(first.live_text().contains("PROXY_MANAGED"));
    let mut other = Probe::new();
    let _other_daemon = Resident::start(&mut other);
    other.add(routed("other", "anthropic", "https://example.invalid/v1"));
    let other_token = other.route_token();
    assert!(token != other_token);
    let port = first.ok("status", json!({}))["route"]["port"]
        .as_u64()
        .unwrap();
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
    // Neither the private RPC summaries nor upstream provider/backup snapshots
    // contain the host capability. The actual upstream API key stays intact.
    assert!(!first.ok("status", json!({})).to_string().contains(&token));
    assert!(!first
        .ok("provider/list", json!({}))
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
    let backup: String = connection
        .query_row(
            "SELECT original_config FROM proxy_live_backup WHERE app_type='codex'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for value in [&stored, &backup] {
        assert!(!value.contains(&token) && !value.contains(HEADER));
    }
    assert!(stored.contains("synthetic-secret-chat"));
    drop(connection);
    let mut invalid = routed("bad", "openai_chat", "https://example.invalid/v1");
    invalid["settingsConfig"]["config"] = json!(format!(
        "{}\n[model_providers.bad.http_headers]\nX-MyCodex-Route-Token = \"{token}\"\n",
        invalid["settingsConfig"]["config"].as_str().unwrap()
    ));
    assert_eq!(
        first.call("provider/add", json!({"provider":invalid}))["error"]["message"],
        "reserved_route_header"
    );
    drop(daemon);
    let _restarted = Resident::start(&mut first);
    assert!(first.route_token() == token);
    first.switch("native");
    assert!(!first.live_text().contains(HEADER) && !first.live_text().contains(&token));
    first.switch("chat");
    assert!(first.route_token() == token);
    first.ok("backend/shutdown", json!({}));
    assert!(!first.live_text().contains(HEADER) && !first.live_text().contains(&token));
    other.ok("backend/shutdown", json!({}));
}

#[test]
fn altered_route_credentials_and_orphaned_route_are_preserved_for_repair() {
    let mut p = Probe::new();
    let daemon = Resident::start(&mut p);
    p.add(routed("chat", "openai_chat", "https://example.invalid/v1"));
    let token = p.route_token();
    let original = p.live_text();
    let changed = original.replace(&token, "external-change");
    fs::write(p.codex.join("config.toml"), &changed).unwrap();
    assert_eq!(
        p.ok("gui/provider/list", json!({}))["liveState"],
        "unavailable"
    );
    assert!(p.live_text() == changed);
    assert_eq!(p.ok("status", json!({}))["route"]["accepting"], false);
    p.ok("backend/shutdown", json!({}));
    assert!(p.live_text() == changed);
    fs::write(p.codex.join("config.toml"), original).unwrap();
    drop(daemon);
    // Without a provider or backup, keep the global files intact for repair.
    let connection = rusqlite::Connection::open(p.data.join("cc-switch.db")).unwrap();
    connection
        .execute("DELETE FROM proxy_live_backup WHERE app_type='codex'", [])
        .unwrap();
    connection
        .execute("DELETE FROM providers WHERE app_type='codex'", [])
        .unwrap();
    drop(connection);
    let _restarted = Resident::start(&mut p);
    assert!(p.live_text().contains("x-mycodex-route-token") && p.live_text().contains(&token));
    assert_eq!(
        p.ok("gui/provider/list", json!({}))["liveState"],
        "unavailable"
    );
    assert_eq!(p.ok("status", json!({}))["route"]["running"], false);
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn offline_probe_cannot_bypass_the_resident_target_lock_with_another_store() {
    let mut p = Probe::new();
    let _daemon = Resident::start(&mut p);
    p.add(api("native", "model-before", false));
    let before = p.live_text();
    let other = Probe::new();
    let input = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":"provider/add",
        "params":{"provider":api("injected", "must-not-write", false)}}))
    .unwrap();
    let output = other.raw(&input, &p.codex);
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "backend_already_running"
    );
    assert!(p.live_text() == before);
    assert!(!other.data.join("cc-switch.db").exists());
    p.ok("backend/shutdown", json!({}));
}

#[test]
fn maintenance_update_preserves_takeover_and_uninstall_requires_direct() {
    let mut p = Probe::new();
    let mut daemon = Resident::start(&mut p);
    p.add(routed("conversion", "openai_chat", "http://127.0.0.1:9/v1"));
    p.add(api("direct", "test-model", true));
    p.switch("conversion");
    let before = p.live_text();
    let auth = fs::read(p.codex.join("auth.json")).ok();
    let port = p.ok("status", json!({}))["route"]["port"].clone();
    assert_eq!(
        p.call("backend/shutdown", json!({"forUpdate":"true"}))["error"]["message"],
        "invalid_params"
    );
    assert_eq!(
        p.call("backend/shutdown", json!({"forUpdate":false}))["error"]["message"],
        "route_required"
    );
    assert_eq!(p.ok("status", json!({}))["route"]["running"], true);
    p.ok("backend/shutdown", json!({"forUpdate":true}));
    assert!(daemon.0.wait().unwrap().success());
    assert_eq!(p.live_text(), before);
    assert_eq!(fs::read(p.codex.join("auth.json")).ok(), auth);
    let mut replacement = Resident::start(&mut p);
    let route = p.ok("status", json!({}))["route"].clone();
    assert_eq!(route["port"], port);
    assert_eq!(route["accepting"], true);
    assert_eq!(p.live_text(), before);
    p.switch("direct");
    let direct = p.live_text();
    p.ok("backend/shutdown", json!({"forUpdate":false}));
    assert!(replacement.0.wait().unwrap().success());
    assert_eq!(p.live_text(), direct);
    assert!(p.data.join("cc-switch.db").exists());
}
