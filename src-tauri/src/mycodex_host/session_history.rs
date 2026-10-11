//! Unified Codex session history. Official providers run under the shared
//! `custom` id, so a session resumes with whichever provider is in use instead
//! of the one that created it. Turning it on moves the existing official
//! sessions into the shared bucket; turning it off moves them back
//! (`session_ledger`). Setting the same value again retries an unfinished move.
use super::{auto_capture, lifecycle, service_error, session_ledger, Host, Result};
use crate::settings;
use serde_json::{json, Value};

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    match method {
        "gui/sessionHistory/get" => status(host),
        "gui/sessionHistory/preview" => {
            let (migrate, restore) = session_ledger::counts();
            Ok(json!({"toMigrate": migrate, "toRestore": restore}))
        }
        "gui/sessionHistory/set" => set(host, params),
        "gui/sessionHistory/repair" => super::session_repair::handle(host, params),
        _ => Err("method_not_supported"),
    }
}

fn status(host: &Host) -> Result<Value> {
    let unified = settings::unify_codex_session_history();
    Ok(json!({
        "unified": unified,
        "pending": session_ledger::pending(unified),
        // Sessions other providers wrote need repair before they load.
        "repairOnResume": unified && lifecycle::official_in_use(&host.state)?.is_some(),
    }))
}

fn set(host: &Host, params: &Value) -> Result<Value> {
    let state = &host.state;
    let unified = params["unified"].as_bool().ok_or("invalid_params")?;
    let _guard = lifecycle::mutation()?;
    auto_capture::before_write(state)?;
    let existing = settings::get_settings();
    let changed = existing.unify_codex_session_history != unified;
    let mut applied = false;
    if changed {
        let mut next = existing.clone();
        next.unify_codex_session_history = unified;
        settings::update_settings(next).map_err(service_error)?;
        // The live route must follow the setting, otherwise sessions would split
        // again once moved; a failed rewrite leaves the setting as it was.
        match lifecycle::reapply_official(state) {
            Ok(rewritten) => applied = rewritten,
            Err(code) => {
                let _ = settings::update_settings(existing);
                // Reapply the previous route; the official row may have been half switched.
                let _ = lifecycle::reapply_official(state);
                return Err(code);
            }
        }
    }
    // The setting is already in effect, so a failed move is reported, not raised:
    // moving is idempotent and setting the same value again retries it.
    let moved = if unified {
        if session_ledger::live_routes_shared() {
            session_ledger::migrate()
        } else {
            // Moved sessions would sit in a bucket the live config cannot see.
            session_ledger::Moved::failed()
        }
    } else {
        session_ledger::restore()
    };
    let mut result = status(host)?;
    result["globalApplied"] = json!(applied);
    result["moved"] = json!({"sessions": moved.sessions, "indexRows": moved.rows});
    if moved.failed {
        result["moved"]["error"] = json!("history_move_failed");
    }
    Ok(result)
}
