use super::Gui;
use serde_json::json;

#[test]
fn legacy_provider_migration_preserves_original_thread_and_history() {
    let mut gui = Gui::new();
    let home = gui.root.path().join("codex");
    let id = "11111111-1111-4111-8111-111111111111";
    let old = "mycodex_resp_0123456789ab";
    std::fs::write(home.join("config.toml"), "model_provider='custom'\n[model_providers.custom]\nname='Synthetic'\nbase_url='http://127.0.0.1:9/v1'\nwire_api='responses'\nrequires_openai_auth=false\n").unwrap();
    let folder = home.join("sessions/2026/11/10");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join(format!("rollout-2026-11-10T01-00-00-{id}.jsonl"));
    let original = format!(
        "{}\n{}\n{}\n",
        json!({"type":"session_meta","ordinal":0,"payload":{"id":id,"model_provider":old,"history_mode":"paginated"}}),
        json!({"type":"event_msg","ordinal":1,"payload":{"type":"thread_settings_applied","thread_id":id,"thread_settings":{"model_provider_id":old,"model":"synthetic"}}}),
        json!({"type":"response_item","ordinal":2,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"history must survive"}]}})
    );
    std::fs::write(&file, &original).unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT,rollout_path TEXT);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?1,?2,?3)",
        rusqlite::params![id, old, file.to_str().unwrap()],
    )
    .unwrap();
    gui.start();
    let args = json!({"threadId":id,"modelProvider":"custom","apply":false});
    let preview = gui.ok("gui/sessionHistory/provider", args.clone());
    assert_eq!(preview["needsMigration"], true);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    let migrated = gui.ok(
        "gui/sessionHistory/provider",
        json!({"threadId":id,"modelProvider":"custom","apply":true}),
    );
    assert_eq!(migrated["changed"], true);
    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(after.len(), original.len());
    let rows: Vec<serde_json::Value> = after
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows[0]["payload"]["model_provider"], "custom");
    assert_eq!(
        rows[1]["payload"]["thread_settings"]["model_provider_id"],
        "custom"
    );
    assert_eq!(after.lines().nth(2), original.lines().nth(2));
    assert_eq!(
        db.query_row(
            "SELECT model_provider FROM threads WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "custom"
    );
    assert_eq!(
        gui.ok("gui/sessionHistory/provider", args)["needsMigration"],
        false
    );
}

fn fixture(gui: &Gui, source: &str, compressed: bool) -> (std::path::PathBuf, String, String) {
    let id = "22222222-2222-4222-8222-222222222222".to_string();
    let home = gui.root.path().join("codex");
    std::fs::write(home.join("config.toml"), "model_provider='custom'\n[model_providers.custom]\nname='Synthetic'\nbase_url='http://127.0.0.1:9/v1'\nrequires_openai_auth=false\n").unwrap();
    let folder = home.join("sessions/2026/11/10");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join(format!(
        "rollout-2026-11-10T01-00-00-{id}.jsonl{}",
        if compressed { ".zst" } else { "" }
    ));
    let text = format!(
        "{}\n{}\n",
        json!({"type":"session_meta","payload":{"id":id,"model_provider":source}}),
        json!({"type":"response_item","payload":{"text":"unchanged","encrypted_content":"opaque"}})
    );
    std::fs::write(
        &file,
        if compressed {
            zstd::stream::encode_all(text.as_bytes(), 3).unwrap()
        } else {
            text.as_bytes().to_vec()
        },
    )
    .unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT);")
        .unwrap();
    db.execute("INSERT INTO threads VALUES(?1,?2)", [&id, source])
        .unwrap();
    (file, text, id)
}

#[test]
fn provider_migration_supports_compressed_history_and_recovers_interrupted_index_commit() {
    let mut gui = Gui::new();
    let (file, original, id) = fixture(&gui, "mycodex_resp_0123456789ab", true);
    gui.start();
    let args = json!({"threadId":id,"modelProvider":"custom","apply":true});
    assert_eq!(gui.ok("gui/sessionHistory/provider", args)["changed"], true);
    let text = String::from_utf8(
        zstd::stream::decode_all(std::fs::read(&file).unwrap().as_slice()).unwrap(),
    )
    .unwrap();
    assert_eq!(text.len(), original.len());
    assert_eq!(text.lines().nth(1), original.lines().nth(1));
    let root = gui
        .root
        .path()
        .join("store/backups/codex-provider-migration-v1")
        .join(&id);
    let generation = std::fs::read_dir(root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let journal_path = generation.join("journal.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    journal["complete"] = json!(false);
    std::fs::write(journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let db = rusqlite::Connection::open(gui.root.path().join("codex/state_5.sqlite")).unwrap();
    db.execute(
        "UPDATE threads SET model_provider='mycodex_resp_0123456789ab'",
        [],
    )
    .unwrap();
    gui.stop();
    gui.start();
    gui.ok("runtime/prepare", json!({}));
    assert_eq!(
        db.query_row("SELECT model_provider FROM threads", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "custom"
    );
    assert_eq!(
        gui.ok(
            "gui/sessionHistory/provider",
            json!({"threadId":id,"modelProvider":"custom","apply":false})
        )["needsMigration"],
        false
    );
    assert!(generation.join("original.rollout").exists());
    assert!(generation.join("index-0.sqlite").exists());
}

#[test]
fn locked_index_and_unknown_provider_never_rewrite_history() {
    for source in ["mycodex_resp_0123456789ab", "unrelated_provider", "openai"] {
        let mut gui = Gui::new();
        let (file, original, id) = fixture(&gui, source, false);
        gui.start();
        let db = rusqlite::Connection::open(gui.root.path().join("codex/state_5.sqlite")).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let result = gui.call(
            "gui/sessionHistory/provider",
            json!({"threadId":id,"modelProvider":"custom","apply":true}),
        );
        if source.starts_with("mycodex_resp_") {
            assert_eq!(result["error"]["message"], "provider_index_busy");
        } else {
            assert_eq!(result["result"]["needsMigration"], false);
            assert_eq!(result["result"]["changed"], false);
        }
        assert_eq!(std::fs::read_to_string(file).unwrap(), original);
        db.execute_batch("ROLLBACK").unwrap();
    }
}

#[test]
fn malformed_recovery_journal_blocks_startup_without_touching_history() {
    let mut gui = Gui::new();
    let (file, original, id) = fixture(&gui, "mycodex_resp_0123456789ab", false);
    gui.start();
    let generation = gui
        .root
        .path()
        .join("store/backups/codex-provider-migration-v1")
        .join(&id)
        .join("interrupted");
    std::fs::create_dir_all(&generation).unwrap();
    std::fs::write(generation.join("journal.json"), b"{").unwrap();
    for method in ["runtime/prepare", "gui/sessionHistory/providerRecovery"] {
        assert_eq!(
            gui.call(method, json!({}))["error"]["message"],
            "provider_recovery_required"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }
}
