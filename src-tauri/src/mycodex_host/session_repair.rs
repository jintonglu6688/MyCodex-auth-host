//! Repairs one Codex session file whose history mixes providers. With unified
//! session history a thread resumes with whichever provider is in use, but the
//! state other providers left in it (plaintext or foreign reasoning, foreign
//! item ids, CC Switch compaction summaries, encrypted fragments) is refused by
//! the official API, and official traffic stays direct instead of passing the
//! proxy that would scrub it. MyCodex calls this while the thread is unloaded:
//! - `resume`: before a thread loads with the official provider in use; drops
//!   only what the official API refuses for certain, so its own state stays;
//! - `rejected`: after a provider refused the history; the same scrub as the
//!   proxy's opaque state rectifier, which drops every reasoning item;
//! - `compaction`: after the user agreed, also replaces compaction items that
//!   another provider issued, which loses the earlier conversation they carry.
//!
//! A changed file is backed up in the private store first.
use super::{lifecycle, Host, Result};
use crate::codex_config::get_codex_config_dir;
use crate::codex_history_migration as history;
use crate::codex_rollout_file;
use crate::config::{copy_file, get_app_config_dir};
use crate::proxy::opaque_state_rectifier::{rectify_opaque_state, OpaqueStateRejection};
use crate::proxy::providers::codex_compaction::{
    decode_compaction_summary, is_compaction_item, strip_mismatched_item_id, user_message_item,
    SUMMARY_PREFIX,
};
use crate::settings;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const BACKUP_DIR: &str = "codex-session-repair-v1";
const BACKUP_GENERATIONS: usize = 50;
/// Every reasoning `encrypted_content` the official API issues is a Fernet token.
const OFFICIAL_ENCRYPTED_PREFIX: &str = "gAAAAA";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Resume,
    Rejected,
    Compaction,
}

#[derive(Default)]
struct Counts {
    reasoning: usize,
    ids: usize,
    converted_compactions: usize,
    encrypted_parts: usize,
    replaced_compactions: usize,
    remaining_compactions: usize,
}

impl Counts {
    fn changed(&self) -> bool {
        self.reasoning
            + self.ids
            + self.converted_compactions
            + self.encrypted_parts
            + self.replaced_compactions
            > 0
    }
}

pub(super) fn handle(host: &Host, params: &Value) -> Result<Value> {
    let thread_id = params["threadId"]
        .as_str()
        .filter(|id| valid_thread_id(id))
        .ok_or("invalid_params")?;
    let mode = match params["mode"].as_str() {
        Some("resume") => Mode::Resume,
        Some("rejected") => Mode::Rejected,
        Some("compaction") => Mode::Compaction,
        _ => return Err("invalid_params"),
    };
    if mode == Mode::Resume {
        if !settings::unify_codex_session_history() {
            return Ok(skipped("not_unified"));
        }
        if lifecycle::official_in_use(&host.state)?.is_none() {
            return Ok(skipped("not_official"));
        }
    }
    let Some(path) = find_rollout(&get_codex_config_dir(), thread_id) else {
        return Ok(skipped("not_found"));
    };
    let _guard = history::lock_codex_official_history_op();
    let backups = get_app_config_dir().join("backups").join(BACKUP_DIR);
    let counts = repair_file(&path, mode, &backups)?;
    Ok(json!({
        "repaired": counts.changed(),
        "skipped": if counts.changed() { Value::Null } else { json!("nothing_to_repair") },
        "reasoning": counts.reasoning,
        "ids": counts.ids,
        "convertedCompactions": counts.converted_compactions,
        "encryptedParts": counts.encrypted_parts,
        "replacedCompactions": counts.replaced_compactions,
        "remainingCompactions": counts.remaining_compactions,
    }))
}

fn skipped(reason: &str) -> Value {
    json!({"repaired": false, "skipped": reason})
}

fn valid_thread_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Codex names each session file `rollout-<timestamp>-<thread id>.jsonl[.zst]`.
fn find_rollout(codex_dir: &Path, thread_id: &str) -> Option<PathBuf> {
    let suffix = format!("-{}.jsonl", thread_id.to_ascii_lowercase());
    let mut found = Vec::new();
    collect(&codex_dir.join("sessions"), &suffix, &mut found, 0, 8);
    collect(
        &codex_dir.join("archived_sessions"),
        &suffix,
        &mut found,
        0,
        4,
    );
    codex_rollout_file::dedupe_siblings(found)
        .into_iter()
        .next()
}

fn collect(dir: &Path, suffix: &str, found: &mut Vec<PathBuf>, depth: u8, max_depth: u8) {
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
            collect(&path, suffix, found, depth + 1, max_depth);
        } else if kind.is_file()
            && codex_rollout_file::is_rollout_file(&path)
            && codex_rollout_file::logical_path(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("rollout-") && name.to_ascii_lowercase().ends_with(suffix)
                })
        {
            found.push(path);
        }
    }
}

fn repair_file(path: &Path, mode: Mode, backups: &Path) -> Result<Counts> {
    let before = stamp(path)?;
    let text = codex_rollout_file::read_to_string(path).map_err(|_| "session_repair_failed")?;
    let (rewritten, counts) = repair_text(&text, mode);
    if !counts.changed() {
        return Ok(counts);
    }
    // Codex appends to the file while the thread is loaded; never race it.
    let changed = || stamp(path).map(|now| now != before);
    if changed()? {
        return Err("session_changed");
    }
    backup(path, backups)?;
    if changed()? {
        return Err("session_changed");
    }
    codex_rollout_file::write_rollout(path, rewritten.as_bytes())
        .map_err(|_| "session_repair_failed")?;
    Ok(counts)
}

fn stamp(path: &Path) -> Result<(Option<SystemTime>, u64)> {
    let meta = fs::metadata(path).map_err(|_| "session_repair_failed")?;
    Ok((meta.modified().ok(), meta.len()))
}

fn backup(path: &Path, backups: &Path) -> Result<()> {
    let generation = backups.join(chrono::Local::now().format("%Y%m%d-%H%M%S%.3f").to_string());
    let name = path.file_name().ok_or("session_repair_failed")?;
    fs::create_dir_all(&generation).map_err(|_| "session_repair_failed")?;
    copy_file(path, &generation.join(name)).map_err(|_| "session_repair_failed")?;
    prune(backups);
    Ok(())
}

/// Keep the newest generations; the names sort by time.
fn prune(backups: &Path) {
    let Ok(entries) = fs::read_dir(backups) else {
        return;
    };
    let mut generations: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    if generations.len() <= BACKUP_GENERATIONS {
        return;
    }
    generations.sort();
    for old in &generations[..generations.len() - BACKUP_GENERATIONS] {
        let _ = fs::remove_dir_all(old);
    }
}

enum Edit {
    Keep,
    Replace(Value),
    Remove,
}

fn repair_text(text: &str, mode: Mode) -> (String, Counts) {
    let lines: Vec<(&str, &str)> = text
        .split_inclusive('\n')
        .map(|segment| match segment.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (segment, ""),
        })
        .collect();
    let parsed: Vec<Option<Value>> = lines
        .iter()
        .map(|(line, _)| {
            (line.contains("\"response_item\"") || line.contains("\"compacted\""))
                .then(|| serde_json::from_str::<Value>(line).ok())
                .flatten()
        })
        .collect();
    // The task a parent agent sent last is the sub-agent's current input; without
    // it the sub-agent has nothing to do (the rectifier keeps it the same way).
    let current = parsed
        .iter()
        .rposition(|value| value.as_ref().is_some_and(|v| v["type"] == "response_item"));
    let mut counts = Counts::default();
    let mut output = String::with_capacity(text.len());
    for (index, ((line, newline), value)) in lines.into_iter().zip(parsed).enumerate() {
        let edit = match value {
            Some(value) => repair_line(value, mode, Some(index) == current, &mut counts),
            None => Edit::Keep,
        };
        match edit {
            Edit::Keep => output.push_str(line),
            Edit::Replace(value) => output.push_str(&value.to_string()),
            Edit::Remove => continue,
        }
        output.push_str(newline);
    }
    (output, counts)
}

fn repair_line(mut line: Value, mode: Mode, current: bool, counts: &mut Counts) -> Edit {
    match line["type"].as_str() {
        Some("response_item") => {
            let item = line["payload"].take();
            match repair_item(item, mode, current, counts) {
                Some((item, false)) => {
                    count_compaction(&item, counts);
                    Edit::Keep
                }
                Some((item, true)) => {
                    count_compaction(&item, counts);
                    line["payload"] = item;
                    Edit::Replace(line)
                }
                None => Edit::Remove,
            }
        }
        Some("compacted") => {
            let Some(history) = line["payload"]
                .get_mut("replacement_history")
                .and_then(Value::as_array_mut)
            else {
                return Edit::Keep;
            };
            let mut changed = false;
            let mut repaired = Vec::with_capacity(history.len());
            for item in std::mem::take(history) {
                match repair_item(item, mode, false, counts) {
                    Some((item, edited)) => {
                        changed |= edited;
                        count_compaction(&item, counts);
                        repaired.push(item);
                    }
                    None => changed = true,
                }
            }
            *history = repaired;
            if changed {
                Edit::Replace(line)
            } else {
                Edit::Keep
            }
        }
        _ => Edit::Keep,
    }
}

fn count_compaction(item: &Value, counts: &mut Counts) {
    if is_compaction_item(item) {
        counts.remaining_compactions += 1;
    }
}

/// The repaired item and whether it changed, or `None` when it is dropped.
fn repair_item(
    mut item: Value,
    mode: Mode,
    current: bool,
    counts: &mut Counts,
) -> Option<(Value, bool)> {
    // CC Switch wraps the summary of a compaction it ran for another provider;
    // as plain text every provider reads it, so this loses nothing.
    if is_compaction_item(&item) {
        if let Some(summary) = item["encrypted_content"]
            .as_str()
            .and_then(decode_compaction_summary)
        {
            counts.converted_compactions += 1;
            return Some((
                user_message_item(&format!("{SUMMARY_PREFIX}\n{summary}")),
                true,
            ));
        }
    }
    if mode == Mode::Resume {
        let item_type = item["type"].as_str().unwrap_or_default().to_string();
        if item_type == "reasoning" && !official_reasoning(&item) {
            counts.reasoning += 1;
            return None;
        }
        let changed = strip_mismatched_item_id(&mut item, &item_type);
        counts.ids += usize::from(changed);
        return Some((item, changed));
    }
    let mut body = json!({ "input": [item] });
    if !current {
        // The rectifier keeps the last input as the turn's own; keep only this one.
        body["input"]
            .as_array_mut()
            .expect("input array")
            .push(user_message_item(""));
    }
    let result = rectify_opaque_state(
        &mut body,
        OpaqueStateRejection {
            reasoning: true,
            compaction: mode == Mode::Compaction,
        },
    );
    counts.reasoning += result.removed_reasoning_items;
    counts.ids += result.removed_foreign_ids;
    counts.encrypted_parts += result.replaced_encrypted_parts;
    counts.replaced_compactions += result.replaced_compaction_items;
    let mut input = match body["input"].take() {
        Value::Array(input) => input,
        _ => Vec::new(),
    };
    if !current {
        input.pop();
    }
    input.pop().map(|item| (item, result.applied))
}

/// Reasoning the official API issued: an encrypted token and no plaintext.
fn official_reasoning(item: &Value) -> bool {
    let plaintext = item["content"]
        .as_array()
        .is_some_and(|content| !content.is_empty());
    let issued = item["encrypted_content"]
        .as_str()
        .is_some_and(|value| value.starts_with(OFFICIAL_ENCRYPTED_PREFIX));
    issued && !plaintext
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::providers::codex_compaction::{
        encode_compaction_summary, OPAQUE_COMPACTION_NOTE,
    };

    fn item(payload: Value) -> String {
        json!({"timestamp":"t","type":"response_item","payload":payload}).to_string()
    }

    fn lines(text: &str) -> Vec<Value> {
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn deepseek_turn() -> Vec<String> {
        vec![
            json!({"type":"session_meta","payload":{"id":"t","model_provider":"custom"}})
                .to_string(),
            item(
                json!({"type":"message","id":"msg_01a1","role":"user","content":[{"type":"input_text","text":"hi"}]}),
            ),
            item(
                json!({"type":"reasoning","id":"601f9c7c-284a","summary":[],"content":[{"type":"reasoning_text","text":"plan"}],"encrypted_content":"9716bfa2-3904-44c6-839c-6c2f4fed88a6-0"}),
            ),
            item(
                json!({"type":"function_call","id":"ee2784d4-cff2","name":"shell","arguments":"{}","call_id":"call_1"}),
            ),
            item(
                json!({"type":"function_call_output","id":"fco_01a1","call_id":"call_1","output":"ok"}),
            ),
            item(
                json!({"type":"message","id":"09c974db-acf0","role":"assistant","content":[{"type":"output_text","text":"done"}]}),
            ),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}).to_string(),
        ]
    }

    #[test]
    fn resume_drops_what_the_official_api_refuses_from_another_providers_turn() {
        let text = deepseek_turn().join("\n") + "\n";
        let (repaired, counts) = repair_text(&text, Mode::Resume);
        assert_eq!(counts.reasoning, 1);
        assert_eq!(counts.ids, 2);
        let repaired = lines(&repaired);
        assert_eq!(repaired.len(), 6);
        assert!(repaired
            .iter()
            .all(|line| line["payload"]["type"] != "reasoning"));
        let call = &repaired[2]["payload"];
        assert_eq!(call["type"], "function_call");
        assert!(call.get("id").is_none());
        assert_eq!(call["call_id"], "call_1");
        assert!(repaired[4]["payload"].get("id").is_none());
        assert_eq!(repaired[1]["payload"]["id"], "msg_01a1");
        assert_eq!(repaired[3]["payload"]["id"], "fco_01a1");
        assert_eq!(repaired[5]["type"], "event_msg");
    }

    #[test]
    fn resume_leaves_an_official_session_byte_for_byte() {
        let text = [
            item(json!({"type":"message","id":"msg_1","role":"user","content":[]})),
            item(json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"s"}],"content":null,"encrypted_content":"gAAAAABqfake"})),
            item(json!({"type":"custom_tool_call","id":"ctc_1","name":"apply_patch","input":"x","call_id":"c"})),
            json!({"type":"compacted","payload":{"message":"","replacement_history":[{"type":"compaction","encrypted_content":"gAAAAABqcompact"}]}}).to_string(),
        ]
        .join("\n");
        let (repaired, counts) = repair_text(&text, Mode::Resume);
        assert!(!counts.changed());
        assert_eq!(counts.remaining_compactions, 1);
        assert_eq!(repaired, text);
    }

    #[test]
    fn compaction_summaries_cc_switch_wrapped_become_text() {
        let wrapped = encode_compaction_summary("earlier work");
        let text = json!({"type":"compacted","payload":{"message":"","replacement_history":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
            {"type":"compaction","encrypted_content":wrapped},
            {"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"x"}]}
        ]}})
        .to_string();
        let (repaired, counts) = repair_text(&text, Mode::Resume);
        assert_eq!(counts.converted_compactions, 1);
        assert_eq!(counts.reasoning, 1);
        assert_eq!(counts.remaining_compactions, 0);
        let history = &lines(&repaired)[0]["payload"]["replacement_history"];
        assert_eq!(history.as_array().unwrap().len(), 2);
        assert_eq!(history[1]["role"], "user");
        let summary = history[1]["content"][0]["text"].as_str().unwrap();
        assert!(summary.starts_with(SUMMARY_PREFIX) && summary.ends_with("earlier work"));
    }

    #[test]
    fn rejected_scrubs_every_reasoning_item_and_encrypted_fragment_but_keeps_compactions() {
        let text = [
            item(json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAAABqrelay"})),
            item(json!({"type":"agent_message","id":"amsg_1","content":[{"type":"encrypted_content","encrypted_content":"gAAAAABqtask"}]})),
            item(json!({"type":"function_call_output","call_id":"c","output":[{"type":"encrypted_content","encrypted_content":"gAAAAABqout"}]})),
            json!({"type":"compacted","payload":{"message":"","replacement_history":[{"type":"compaction","encrypted_content":"gAAAAABqcompact"}]}}).to_string(),
            item(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]})),
        ]
        .join("\n");
        let (repaired, counts) = repair_text(&text, Mode::Rejected);
        assert_eq!(counts.reasoning, 1);
        assert_eq!(counts.encrypted_parts, 2);
        assert_eq!(counts.replaced_compactions, 0);
        assert_eq!(counts.remaining_compactions, 1);
        let repaired = lines(&repaired);
        assert_eq!(repaired.len(), 4);
        assert_eq!(repaired[0]["payload"]["content"][0]["type"], "input_text");
        assert_eq!(repaired[1]["payload"]["output"][0]["type"], "input_text");
        assert_eq!(
            repaired[2]["payload"]["replacement_history"][0]["type"],
            "compaction"
        );
    }

    #[test]
    fn rejected_keeps_the_task_a_sub_agent_is_working_on() {
        let task = item(
            json!({"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"gAAAAABqtask"}]}),
        );
        let text = [
            item(json!({"type":"reasoning","summary":[],"encrypted_content":"gAAAAABq"})),
            task.clone(),
        ]
        .join("\n");
        let (repaired, counts) = repair_text(&text, Mode::Rejected);
        assert_eq!(counts.reasoning, 1);
        assert_eq!(counts.encrypted_parts, 0);
        assert_eq!(repaired, task);
    }

    #[test]
    fn compaction_mode_replaces_compactions_another_provider_issued() {
        let text = json!({"type":"compacted","payload":{"message":"","replacement_history":[
            {"type":"compaction","encrypted_content":"gAAAAABqcompact"},
            {"type":"context_compaction"}
        ]}})
        .to_string();
        let (repaired, counts) = repair_text(&text, Mode::Compaction);
        assert_eq!(counts.replaced_compactions, 2);
        assert_eq!(counts.remaining_compactions, 0);
        let history = &lines(&repaired)[0]["payload"]["replacement_history"];
        assert_eq!(history.as_array().unwrap().len(), 1);
        assert_eq!(history[0]["content"][0]["text"], OPAQUE_COMPACTION_NOTE);
    }

    #[test]
    fn rollouts_are_found_by_thread_id_only_under_the_session_folders() {
        let root = tempfile::tempdir().unwrap();
        let id = "01a1263c-44bd-7800-8000-000000000001";
        let day = root.path().join("sessions/2026/10/10");
        fs::create_dir_all(&day).unwrap();
        let plain = day.join(format!("rollout-2026-10-10T22-34-05-{id}.jsonl"));
        fs::write(&plain, "{}\n").unwrap();
        fs::write(
            day.join(format!("rollout-2026-10-10T22-34-05-{id}.jsonl.zst")),
            "",
        )
        .unwrap();
        fs::write(root.path().join(format!("rollout-x-{id}.jsonl")), "{}\n").unwrap();
        assert_eq!(find_rollout(root.path(), id), Some(plain));
        assert_eq!(find_rollout(root.path(), "01a1263c-0000"), None);
        assert!(!valid_thread_id("../../etc"));
        assert!(!valid_thread_id("short"));
    }
}
