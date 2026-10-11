//! Moves official sessions into the shared `custom` bucket of unified session
//! history and back. Codex keeps a session's bucket in the `model_provider` of
//! its first line (`session_meta`) and in the state DB, so only that field
//! changes; the conversation stays in place, including turns added while the
//! history was unified. Instead of copying whole sessions, the ids that were
//! official are kept in a small ledger in the private store, which is all a
//! restore needs. Moving is idempotent and never runs while a turn writes.
use crate::codex_config::{get_codex_config_dir, read_codex_config_text};
use crate::codex_history_migration as history;
use crate::codex_rollout_file;
use crate::codex_state_db::{codex_state_db_is_lockable, codex_state_db_paths};
use crate::config::{atomic_write, get_app_config_dir};
use rusqlite::{params_from_iter, Connection};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LEDGER_FILE: &str = "codex-session-history-ledger.json";
const OFFICIAL: &str = "openai";
const SHARED: &str = "custom";
const ID_CHUNK: usize = 500;

#[derive(Default)]
pub(super) struct Moved {
    pub sessions: usize,
    pub rows: usize,
    pub failed: bool,
}

impl Moved {
    pub fn failed() -> Self {
        Self {
            failed: true,
            ..Self::default()
        }
    }
}

/// Whether new sessions land in the shared bucket, which moved ones must join.
pub(super) fn live_routes_shared() -> bool {
    read_codex_config_text()
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .is_some_and(|config| config["model_provider"].as_str() == Some(SHARED))
}

/// Sessions a move would change now: official ones for enabling, ledgered
/// ones still in the shared bucket for disabling.
pub(super) fn counts() -> (usize, usize) {
    let codex_dir = get_codex_config_dir();
    let ledger = ledger_ids(&codex_dir);
    let mut migrate = 0;
    let mut restore = 0;
    for path in rollouts(&codex_dir) {
        match first_meta(&path) {
            Some((provider, _)) if provider == OFFICIAL => migrate += 1,
            Some((provider, id)) if provider == SHARED && ledger.contains(&id) => restore += 1,
            _ => {}
        }
    }
    (migrate, restore)
}

/// Whether the move for the current setting is unfinished.
pub(super) fn pending(unified: bool) -> bool {
    let (migrate, restore) = counts();
    if unified {
        migrate > 0
    } else {
        restore > 0
    }
}

pub(super) fn migrate() -> Moved {
    let _guard = history::lock_codex_official_history_op();
    let codex_dir = get_codex_config_dir();
    let files: Vec<(PathBuf, String)> = rollouts(&codex_dir)
        .into_iter()
        .filter_map(|path| match first_meta(&path) {
            Some((provider, id)) if provider == OFFICIAL => Some((path, id)),
            _ => None,
        })
        .collect();
    let dbs = state_dbs(&codex_dir);
    let mut ids: BTreeSet<String> = files.iter().map(|(_, id)| id.clone()).collect();
    let mut moved = Moved::default();
    for db in &dbs {
        match thread_ids(db, OFFICIAL) {
            Ok(rows) => ids.extend(rows),
            Err(()) => moved.failed = true,
        }
    }
    // Record ownership before anything moves, so a restore finds every session.
    if remember(&codex_dir, &ids).is_err() {
        moved.failed = true;
        return moved;
    }
    move_sessions(&files, &dbs, OFFICIAL, SHARED, &ids, &mut moved);
    moved
}

pub(super) fn restore() -> Moved {
    let _guard = history::lock_codex_official_history_op();
    let codex_dir = get_codex_config_dir();
    let ids = ledger_ids(&codex_dir);
    let mut moved = Moved::default();
    if ids.is_empty() {
        return moved;
    }
    let files: Vec<(PathBuf, String)> = rollouts(&codex_dir)
        .into_iter()
        .filter_map(|path| match first_meta(&path) {
            Some((provider, id)) if provider == SHARED && ids.contains(&id) => Some((path, id)),
            _ => None,
        })
        .collect();
    let dbs = state_dbs(&codex_dir);
    move_sessions(&files, &dbs, SHARED, OFFICIAL, &ids, &mut moved);
    moved
}

fn move_sessions(
    files: &[(PathBuf, String)],
    dbs: &[PathBuf],
    from: &str,
    to: &str,
    ids: &BTreeSet<String>,
    moved: &mut Moved,
) {
    for (path, _) in files {
        match rewrite_first_meta(path, from, to) {
            Ok(true) => moved.sessions += 1,
            Ok(false) => {}
            Err(()) => moved.failed = true,
        }
    }
    for db in dbs {
        match update_threads(db, from, to, ids) {
            Ok(rows) => moved.rows += rows,
            Err(()) => moved.failed = true,
        }
    }
}

fn rollouts(codex_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect(&codex_dir.join("sessions"), &mut found, 0, 8);
    collect(&codex_dir.join("archived_sessions"), &mut found, 0, 4);
    codex_rollout_file::dedupe_siblings(found)
}

fn collect(dir: &Path, found: &mut Vec<PathBuf>, depth: u8, max_depth: u8) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect(&path, found, depth + 1, max_depth);
        } else if kind.is_file() && codex_rollout_file::is_rollout_file(&path) {
            found.push(path);
        }
    }
}

/// The bucket and id Codex reads from a session's first line.
fn first_meta(path: &Path) -> Option<(String, String)> {
    let mut reader = codex_rollout_file::open_reader(path).ok()?;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(line.trim_end()).ok()?;
    if value["type"] != "session_meta" {
        return None;
    }
    let payload = &value["payload"];
    Some((
        payload["model_provider"].as_str()?.to_string(),
        payload["id"].as_str()?.to_string(),
    ))
}

fn rewrite_first_meta(path: &Path, from: &str, to: &str) -> Result<bool, ()> {
    let stamp = |path: &Path| {
        fs::metadata(path)
            .map(|meta| (meta.modified().ok(), meta.len()))
            .map_err(|_| ())
    };
    let before = stamp(path)?;
    let text = codex_rollout_file::read_to_string(path).map_err(|_| ())?;
    let (first, rest) = match text.find('\n') {
        Some(end) => text.split_at(end),
        None => (text.as_str(), ""),
    };
    let mut value: Value = serde_json::from_str(first).map_err(|_| ())?;
    if value["type"] != "session_meta" || value["payload"]["model_provider"] != from {
        return Ok(false);
    }
    value["payload"]["model_provider"] = json!(to);
    let rewritten = format!("{value}{rest}");
    // Codex appends while the thread is loaded; never race it.
    if stamp(path)? != before {
        return Err(());
    }
    codex_rollout_file::write_rollout(path, rewritten.as_bytes()).map_err(|_| ())?;
    Ok(true)
}

fn state_dbs(codex_dir: &Path) -> Vec<PathBuf> {
    let config = read_codex_config_text().unwrap_or_default();
    codex_state_db_paths(codex_dir, &config)
        .into_iter()
        .filter(|path| path.exists() && codex_state_db_is_lockable(path))
        .collect()
}

fn open_threads(db: &Path) -> Result<Option<Connection>, ()> {
    let conn = Connection::open(db).map_err(|_| ())?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(|_| ())?;
    let has_column = conn
        .prepare("SELECT model_provider FROM threads LIMIT 0")
        .is_ok();
    Ok(has_column.then_some(conn))
}

fn thread_ids(db: &Path, provider: &str) -> Result<Vec<String>, ()> {
    let Some(conn) = open_threads(db)? else {
        return Ok(Vec::new());
    };
    let mut statement = conn
        .prepare("SELECT id FROM threads WHERE model_provider = ?1")
        .map_err(|_| ())?;
    let rows = statement
        .query_map([provider], |row| row.get::<_, String>(0))
        .map_err(|_| ())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|_| ())
}

fn update_threads(db: &Path, from: &str, to: &str, ids: &BTreeSet<String>) -> Result<usize, ()> {
    if ids.is_empty() {
        return Ok(0);
    }
    let Some(mut conn) = open_threads(db)? else {
        return Ok(0);
    };
    let tx = conn.transaction().map_err(|_| ())?;
    let ids: Vec<&String> = ids.iter().collect();
    let mut changed = 0;
    for chunk in ids.chunks(ID_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "UPDATE threads SET model_provider = ? WHERE model_provider = ? AND id IN ({placeholders})"
        );
        let values = [to, from]
            .into_iter()
            .chain(chunk.iter().map(|id| id.as_str()));
        changed += tx.execute(&sql, params_from_iter(values)).map_err(|_| ())?;
    }
    tx.commit().map_err(|_| ())?;
    Ok(changed)
}

fn ledger_path() -> PathBuf {
    get_app_config_dir().join(LEDGER_FILE)
}

/// The ledger is kept per Codex directory, which can be moved in the settings.
fn dir_key(codex_dir: &Path) -> String {
    fs::canonicalize(codex_dir)
        .unwrap_or_else(|_| codex_dir.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn read_ledger() -> Value {
    fs::read_to_string(ledger_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"version": 1, "dirs": {}}))
}

fn ledger_ids(codex_dir: &Path) -> BTreeSet<String> {
    read_ledger()["dirs"][dir_key(codex_dir)]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn remember(codex_dir: &Path, ids: &BTreeSet<String>) -> Result<(), ()> {
    let mut known = ledger_ids(codex_dir);
    if ids.is_subset(&known) {
        return Ok(());
    }
    known.extend(ids.iter().cloned());
    let mut ledger = read_ledger();
    if !ledger["dirs"].is_object() {
        ledger["dirs"] = json!({});
    }
    ledger["dirs"][dir_key(codex_dir)] = json!(known);
    let bytes = serde_json::to_vec_pretty(&ledger).map_err(|_| ())?;
    atomic_write(&ledger_path(), &bytes).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_line_changes_in_plain_and_compressed_sessions() {
        let root = tempfile::tempdir().unwrap();
        let meta = json!({"type":"session_meta","payload":{"id":"a","model_provider":"openai"}});
        let text =
            format!("{meta}\n{{\"type\":\"response_item\",\"payload\":{{\"text\":\"kept\"}}}}\n");
        let plain = root.path().join("rollout-x-a.jsonl");
        fs::write(&plain, &text).unwrap();
        let compressed = root.path().join("rollout-x-b.jsonl.zst");
        fs::write(
            &compressed,
            zstd::stream::encode_all(text.as_bytes(), 3).unwrap(),
        )
        .unwrap();
        for path in [&plain, &compressed] {
            assert_eq!(first_meta(path), Some(("openai".into(), "a".into())));
            assert_eq!(rewrite_first_meta(path, OFFICIAL, SHARED), Ok(true));
            assert_eq!(first_meta(path), Some(("custom".into(), "a".into())));
            assert_eq!(rewrite_first_meta(path, OFFICIAL, SHARED), Ok(false));
            let rewritten = codex_rollout_file::read_to_string(path).unwrap();
            assert_eq!(rewritten.lines().nth(1), text.lines().nth(1));
        }
    }
}
