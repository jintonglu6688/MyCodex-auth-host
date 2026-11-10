//! Target-local, recoverable migration of provider metadata in an idle runtime.
use super::{lifecycle, runtime_paths, session_provider_metadata as metadata, Result};
use crate::{codex_config, codex_history_migration, codex_rollout_file, codex_state_db, config};
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags, OptionalExtension,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const LIMIT: u64 = 512 * 1024 * 1024;
const BACKUPS: &str = "backups/codex-provider-migration-v1";

#[derive(Deserialize, Serialize)]
struct Journal {
    thread: String,
    source: String,
    rollout: PathBuf,
    before: String,
    after: String,
    indexes: Vec<PathBuf>,
    complete: bool,
}

pub(super) fn handle(params: &Value) -> Result<Value> {
    let id = params["threadId"]
        .as_str()
        .filter(|s| valid_id(s))
        .ok_or("invalid_params")?;
    let target = params["modelProvider"].as_str().ok_or("invalid_params")?;
    let apply = params["apply"].as_bool().ok_or("invalid_params")?;
    if target != "custom" {
        return Ok(outcome(false, false, "unsupported_provider"));
    }
    let _mutation = lifecycle::mutation()?;
    let _history = codex_history_migration::lock_codex_official_history_op();
    let paths = runtime_paths().ok_or("runtime_not_initialized")?;
    let config = codex_config::read_codex_config_text().map_err(|_| "invalid_live_config")?;
    let parsed: toml::Value = config.parse().map_err(|_| "invalid_live_config")?;
    let profile = parsed.get("profile").and_then(toml::Value::as_str);
    let selected = profile
        .and_then(|name| parsed.get("profiles")?.get(name)?.get("model_provider"))
        .or_else(|| parsed.get("model_provider"))
        .and_then(toml::Value::as_str);
    if selected != Some("custom")
        || parsed
            .get("model_providers")
            .and_then(|v| v.get("custom"))
            .is_none()
    {
        return Err("provider_configuration_changed");
    }
    let backup_root = paths.data_dir.join(BACKUPS).join(id);
    if let Some((dir, journal)) = pending(&backup_root, id)? {
        if !apply {
            return Ok(outcome(true, false, "recovery_pending"));
        }
        finish(&paths.codex_home, &dir, journal, &config)?;
        return Ok(outcome(false, true, "recovered"));
    }
    let Some(file) = find_rollout(&paths.codex_home, id)? else {
        return Ok(outcome(false, false, "not_found"));
    };
    let text = read_rollout(&file)?;
    let source = metadata::source(&text, id)?;
    let source = source.as_str();
    if source == "custom" {
        return Ok(outcome(false, false, "current_provider"));
    }
    if !metadata::legacy(source)
        && !(source == "openai" && crate::settings::unify_codex_session_history())
    {
        return Ok(outcome(false, false, "unknown_provider"));
    }
    let rewritten = metadata::rewrite(&text, id, source)?;
    if !apply {
        return Ok(outcome(true, false, "ready"));
    }
    let original = fs::read(&file).map_err(|_| "provider_history_changed")?;
    // Revalidate the bytes read before planning: another process may own this home.
    if read_rollout(&file)? != text {
        return Err("provider_history_changed");
    }
    let encoded = if codex_rollout_file::is_compressed(&file) {
        zstd::stream::encode_all(rewritten.as_bytes(), 3).map_err(|_| "provider_history_invalid")?
    } else {
        rewritten.into_bytes()
    };
    let original_hash = hash(&original);
    let dir = backup_root.join(&original_hash[..16]);
    ensure_safe(&backup_root)?;
    fs::create_dir_all(&dir).map_err(|_| "provider_backup_failed")?;
    ensure_safe(&dir)?;
    let previous = dir.join("original.rollout");
    if previous.exists()
        && hash(&fs::read(previous).map_err(|_| "provider_backup_failed")?) != original_hash
    {
        return Err("provider_backup_failed");
    }
    let indexes = indexes(&paths.codex_home, &config)?;
    let locked = lock_indexes(&indexes, id, source)?;
    for (index, path) in indexes.iter().enumerate() {
        snapshot(path, &dir.join(format!("index-{index}.sqlite")))?;
    }
    private_write(&dir.join("original.rollout"), &original)?;
    private_write(&dir.join("migrated.rollout"), &encoded)?;
    let journal = Journal {
        thread: id.to_string(),
        source: source.to_string(),
        rollout: file
            .strip_prefix(&paths.codex_home)
            .map_err(|_| "provider_history_invalid")?
            .to_path_buf(),
        before: hash(&original),
        after: hash(&encoded),
        indexes,
        complete: false,
    };
    if codex_config::read_codex_config_text().map_err(|_| "invalid_live_config")? != config {
        return Err("provider_configuration_changed");
    }
    save(&dir, &journal)?;
    if source == "openai" {
        super::session_ledger::remember(&paths.codex_home, &BTreeSet::from([id.to_string()]))
            .map_err(|_| "provider_backup_failed")?;
    }
    finish_locked(&paths.codex_home, &dir, journal, locked)?;
    Ok(outcome(false, true, "migrated"))
}

fn outcome(needs: bool, changed: bool, reason: &str) -> Value {
    json!({"needsMigration":needs,"changed":changed,"reason":reason})
}

fn valid_id(id: &str) -> bool {
    (id.len() == 36 || id.len() == 32) && uuid::Uuid::parse_str(id).is_ok()
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_safe(path)?;
    config::stage_write(path, bytes, Some(0o600), true)
        .and_then(|staged| staged.commit())
        .map_err(|_| "provider_backup_failed")
}

fn save(dir: &Path, journal: &Journal) -> Result<()> {
    private_write(
        &dir.join("journal.json"),
        &serde_json::to_vec(journal).map_err(|_| "provider_backup_failed")?,
    )
}

fn pending(root: &Path, id: &str) -> Result<Option<(PathBuf, Journal)>> {
    ensure_safe(root)?;
    if !root.exists() {
        return Ok(None);
    }
    let mut result = None;
    for entry in fs::read_dir(root).map_err(|_| "provider_recovery_failed")? {
        let dir = entry.map_err(|_| "provider_recovery_failed")?.path();
        ensure_safe(&dir)?;
        let file = dir.join("journal.json");
        if !file.exists() {
            continue;
        }
        ensure_safe(&file)?;
        let journal: Journal =
            serde_json::from_slice(&fs::read(file).map_err(|_| "provider_recovery_failed")?)
                .map_err(|_| "provider_recovery_failed")?;
        if journal.thread != id {
            return Err("provider_recovery_failed");
        }
        if !journal.complete {
            if result.is_some() {
                return Err("provider_recovery_failed");
            }
            result = Some((dir, journal));
        }
    }
    Ok(result)
}

fn finish(home: &Path, dir: &Path, journal: Journal, config: &str) -> Result<()> {
    if !metadata::legacy(&journal.source) && journal.source != "openai" {
        return Err("provider_recovery_failed");
    }
    if journal.indexes != indexes(home, config)? {
        return Err("provider_history_changed");
    }
    let locked = lock_indexes(&journal.indexes, &journal.thread, &journal.source)?;
    if journal.source == "openai" {
        if !crate::settings::unify_codex_session_history() {
            return Err("provider_configuration_changed");
        }
        super::session_ledger::remember(home, &BTreeSet::from([journal.thread.clone()]))
            .map_err(|_| "provider_backup_failed")?;
    }
    finish_locked(home, dir, journal, locked)
}

fn finish_locked(
    home: &Path,
    dir: &Path,
    mut journal: Journal,
    locked: Vec<Connection>,
) -> Result<()> {
    if journal
        .rollout
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("provider_recovery_failed");
    }
    let file = home.join(&journal.rollout);
    if !file.starts_with(home.join("sessions")) && !file.starts_with(home.join("archived_sessions"))
    {
        return Err("provider_recovery_failed");
    }
    ensure_safe(&file)?;
    let current = fs::read(&file).map_err(|_| "provider_history_changed")?;
    let current_hash = hash(&current);
    if current_hash != journal.before && current_hash != journal.after {
        return Err("provider_history_changed");
    }
    let staged = dir.join("migrated.rollout");
    ensure_safe(&staged)?;
    let migrated = fs::read(staged).map_err(|_| "provider_recovery_failed")?;
    if hash(&migrated) != journal.after {
        return Err("provider_recovery_failed");
    }
    if current_hash != journal.after {
        let modified = fs::metadata(&file)
            .and_then(|m| m.modified())
            .map_err(|_| "provider_history_changed")?;
        // The runtime is stopped and every known index is locked before replacing history.
        if hash(&fs::read(&file).map_err(|_| "provider_history_changed")?) != current_hash {
            return Err("provider_history_changed");
        }
        config::atomic_write_private(&file, &migrated).map_err(|_| "provider_recovery_required")?;
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_times(fs::FileTimes::new().set_modified(modified)))
            .map_err(|_| "provider_recovery_required")?;
    }
    for conn in &locked {
        conn.execute(
            "UPDATE threads SET model_provider='custom' WHERE id=?1 AND model_provider=?2",
            [&journal.thread, &journal.source],
        )
        .map_err(|_| "provider_recovery_required")?;
    }
    for conn in locked {
        conn.execute_batch("COMMIT")
            .map_err(|_| "provider_recovery_required")?;
    }
    journal.complete = true;
    save(dir, &journal)
}

fn indexes(home: &Path, config: &str) -> Result<Vec<PathBuf>> {
    let mut paths = codex_state_db::codex_state_db_paths(home, config);
    let nested = home
        .join("sqlite")
        .join(codex_state_db::CODEX_STATE_DB_FILENAME);
    if !paths.contains(&nested) {
        paths.push(nested);
    }
    paths.retain(|p| p.exists());
    for path in &paths {
        ensure_safe(path)?;
        if !codex_state_db::codex_state_db_is_lockable(path) {
            return Err("provider_index_unavailable");
        }
    }
    Ok(paths)
}

fn lock_indexes(paths: &[PathBuf], id: &str, source: &str) -> Result<Vec<Connection>> {
    let mut locked = Vec::new();
    for path in paths {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|_| "provider_index_unavailable")?;
        conn.busy_timeout(Duration::from_millis(500))
            .map_err(|_| "provider_index_unavailable")?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|_| "provider_index_busy")?;
        let saved: Option<String> = conn
            .query_row(
                "SELECT model_provider FROM threads WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| "provider_index_unavailable")?;
        if saved
            .as_deref()
            .is_some_and(|s| s != source && s != "custom")
        {
            return Err("provider_history_conflict");
        }
        locked.push(conn);
    }
    Ok(locked)
}

fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    ensure_safe(destination)?;
    // No journal exists yet: replace a partial snapshot left by an interrupted backup.
    let input = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "provider_backup_failed")?;
    let mut output = Connection::open(destination).map_err(|_| "provider_backup_failed")?;
    let backup = Backup::new(&input, &mut output).map_err(|_| "provider_backup_failed")?;
    match backup.step(-1).map_err(|_| "provider_backup_failed")? {
        StepResult::Done => Ok(()),
        _ => Err("provider_backup_failed"),
    }
}

fn read_rollout(file: &Path) -> Result<String> {
    let mut text = String::new();
    codex_rollout_file::open_reader(file)
        .map_err(|_| "provider_history_invalid")?
        .take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|_| "provider_history_invalid")?;
    if text.len() as u64 > LIMIT {
        return Err("provider_history_too_large");
    }
    Ok(text)
}

fn find_rollout(home: &Path, id: &str) -> Result<Option<PathBuf>> {
    let mut files = Vec::new();
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        collect(&root, &format!("-{id}.jsonl"), 0, &mut files)?;
    }
    if files.len() > 1 {
        return Err("provider_history_ambiguous");
    }
    Ok(files.pop())
}

fn collect(dir: &Path, suffix: &str, depth: u8, files: &mut Vec<PathBuf>) -> Result<()> {
    ensure_safe(dir)?;
    if !dir.exists() {
        return Ok(());
    }
    if depth > 8 {
        return Err("provider_history_invalid");
    }
    for entry in fs::read_dir(dir).map_err(|_| "provider_history_invalid")? {
        let entry = entry.map_err(|_| "provider_history_invalid")?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|_| "provider_history_invalid")?;
        if kind.is_dir() {
            collect(&path, suffix, depth + 1, files)?;
        } else if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.starts_with("rollout-")
                && (n.ends_with(suffix) || n.ends_with(&format!("{suffix}.zst")))
        }) {
            ensure_safe(&path)?;
            if !kind.is_file() {
                return Err("provider_history_invalid");
            }
            files.push(path);
        }
    }
    Ok(())
}

fn ensure_safe(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err("provider_history_unsafe_path");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err("provider_history_unsafe_path");
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("provider_history_unsafe_path"),
        }
    }
    Ok(())
}

// Called before Codex starts; acquire the mutation gate only when recovery writes are needed.
pub(super) fn recover_pending() -> Result<()> {
    recover_pending_inner().map_err(|_| "provider_recovery_required")
}

pub(super) fn recover_before_start() -> Result<Value> {
    recover_pending()?;
    Ok(json!({"recovered":true}))
}

fn recover_pending_inner() -> Result<()> {
    let paths = runtime_paths().ok_or("runtime_not_initialized")?;
    let root = paths.data_dir.join(BACKUPS);
    ensure_safe(&root)?;
    if !root.exists() {
        return Ok(());
    }
    let _history = codex_history_migration::lock_codex_official_history_op();
    for entry in fs::read_dir(&root).map_err(|_| "provider_recovery_required")? {
        let path = entry.map_err(|_| "provider_recovery_required")?.path();
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| valid_id(s))
            .ok_or("provider_recovery_required")?;
        if let Some((dir, journal)) = pending(&path, id)? {
            let _mutation = lifecycle::mutation()?;
            let config =
                codex_config::read_codex_config_text().map_err(|_| "provider_recovery_required")?;
            finish(&paths.codex_home, &dir, journal, &config)
                .map_err(|_| "provider_recovery_required")?;
        }
    }
    Ok(())
}
