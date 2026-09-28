//! Allowlisted capture metadata only. Never pass auth, config, names or URLs here.
use super::{gui_provider, Outcome, Result};
use serde::Serialize;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Trace {
    source: &'static str,
    pub stage: &'static str,
    pub kind: &'static str,
    pub branch: &'static str,
    pub previous_ref: Option<String>,
    pub provider_ref: Option<String>,
    pub account_ref: Option<String>,
    pub provider_count: Option<usize>,
    pub candidate_count: Option<usize>,
    pub exact_count: Option<usize>,
    pub account_imported: bool,
    pub provider_saved: bool,
    pub current_set: bool,
}

impl Trace {
    pub fn new(source: &'static str) -> Self {
        Self {
            source,
            stage: "read_snapshot",
            kind: "unknown",
            branch: "undecided",
            previous_ref: None,
            provider_ref: None,
            account_ref: None,
            provider_count: None,
            candidate_count: None,
            exact_count: None,
            account_imported: false,
            provider_saved: false,
            current_set: false,
        }
    }

    pub fn finish(&self, result: &Result<Outcome>) {
        let Some(paths) = crate::mycodex_host::runtime_paths() else {
            return;
        };
        let record = json!({"schemaVersion":1,"timeUtc":chrono::Utc::now().to_rfc3339(),
            "processId":std::process::id(),"sourceRevision":env!("MYCODEX_CORE_SOURCE_REVISION"),
            "sourceDirty":env!("MYCODEX_CORE_SOURCE_DIRTY") != "false",
            "capture":self,"result":match result {
                Ok(outcome) => outcome.state,
                Err(_) => "failed",
            },"action":match result {
                Ok(outcome) if outcome.state == "current" => if self.branch == "new_identity" { "created" } else { "reused" },
                Ok(_) => "signed_out",
                Err(_) => "failed",
            },"error":result.as_ref().err().copied()});
        // The target/store and lifecycle locks already serialize capture. Logging
        // must not alter its success/failure even on full disks or read-only files.
        let _ = append(&paths.data_dir, &record.to_string(), MAX_BYTES);
    }
}

pub(super) fn reference(id: &str) -> Option<String> {
    if id.is_empty() {
        None
    } else {
        Some(gui_provider::hash(&json!(id)))
    }
}

fn regular_file_size(path: &Path) -> std::io::Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(std::io::ErrorKind::InvalidInput.into());
                }
            }
            if !metadata.file_type().is_file() {
                return Err(std::io::ErrorKind::InvalidInput.into());
            }
            Ok(Some(metadata.len()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn append(directory: &Path, line: &str, limit: u64) -> std::io::Result<()> {
    let path = directory.join("auto-capture.jsonl");
    if regular_file_size(&path)?.is_some_and(|size| size + line.len() as u64 + 1 > limit) {
        let previous = directory.join("auto-capture.previous.jsonl");
        if regular_file_size(&previous)?.is_some() {
            fs::remove_file(&previous)?;
        }
        fs::rename(&path, &previous)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    writeln!(options.open(path)?, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_one_archive_and_refuses_non_files() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "first", 12).unwrap();
        append(dir.path(), "second", 12).unwrap();
        append(dir.path(), "third", 12).unwrap();
        append(dir.path(), "fourth", 12).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("auto-capture.jsonl")).unwrap(),
            "fourth\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("auto-capture.previous.jsonl")).unwrap(),
            "third\n"
        );
        let blocked = tempfile::tempdir().unwrap();
        fs::create_dir(blocked.path().join("auto-capture.jsonl")).unwrap();
        assert!(append(blocked.path(), "record", 12).is_err());
        assert!(blocked.path().join("auto-capture.jsonl").is_dir());
    }
}
