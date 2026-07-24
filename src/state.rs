//! journal (~/.local/state/undertake/)
//!
//! Writes/overwrites `journal.json` in the state dir with the latest cycle entry.
//! The `undertake status` command reads `last_cycle` from this file.

#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Top-level journal shape — `last_cycle` is the most recent entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub(crate) last_cycle: JournalEntry,
}

/// One cycle's journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
    pub(crate) id: String,
    pub(crate) completed_at: String,
    pub(crate) dry_run: bool,
    pub(crate) summary: JournalSummary,
}

/// Numeric summary of one cycle's outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JournalSummary {
    pub(crate) scanned: u64,
    pub(crate) ready: u64,
    pub(crate) dispatched: u64,
    pub(crate) proposed: u64,
    pub(crate) verified: u64,
    pub(crate) flagged: u64,
    pub(crate) skipped: u64,
}

/// Writes (overwrites) `journal.json` with the given entry as `last_cycle`.
pub(crate) fn write_journal(state_dir: &Path, entry: &JournalEntry) -> io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let journal = Journal {
        last_cycle: entry.clone(),
    };
    let json = serde_json::to_vec_pretty(&journal).map_err(io::Error::other)?;
    let path = state_dir.join("journal.json");
    std::fs::write(path, json)?;
    Ok(())
}

/// Result of one copy-based legacy state migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MigrationSummary {
    pub(crate) files_copied: u64,
}

/// Copies quiescent live Conductor state into a new Undertake-owned root.
///
/// The source remains untouched. The destination must not exist. Exact legacy
/// archive roots (`runs`, `logs`, `worker-commit-hooks`, `leases`, and `arena`)
/// are intentionally left behind; any other unknown live state fails closed
/// before the staging directory is published.
pub(crate) fn migrate_live_state(
    source: &Path,
    destination: &Path,
    policy: &crate::role_routing::RoutingPolicy,
) -> io::Result<MigrationSummary> {
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "state migration destination already exists: {}",
                destination.display()
            ),
        ));
    }
    let source = source.canonicalize()?;
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state migration source must be a directory",
        ));
    }
    let destination = absolute_path(destination)?;
    if destination == source || destination.starts_with(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state migration refuses in-place or nested destination",
        ));
    }
    validate_top_level(&source)?;
    validate_quiescent_runs(&source)?;
    let source_digest = tree_digest(&source)?;

    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "state migration destination has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let staging = parent.join(format!(".{name}.migration-{}", std::process::id()));
    fs::create_dir(&staging)?;

    let result = (|| {
        let mut files_copied = 0_u64;
        files_copied += copy_typed_file::<Journal>(&source, &staging, "journal.json")?;
        files_copied +=
            copy_typed_file::<crate::ratchet::RatchetStore>(&source, &staging, "ratchet.json")?;
        files_copied += migrate_plans(&source, &staging)?;
        files_copied += migrate_runs(&source, &staging)?;
        files_copied += crate::role_routing::migrate_legacy_state(&source, &staging, policy)
            .map_err(io::Error::other)?;
        if tree_digest(&source)? != source_digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state migration source changed during copy",
            ));
        }
        fs::rename(&staging, &destination)?;
        Ok(MigrationSummary { files_copied })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn validate_top_level(source: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 live-state entry")
        })?;
        if !matches!(
            name,
            "journal.json"
                | "ratchet.json"
                | "plans"
                | "runs-v2"
                | "role-routing"
                | "runs"
                | "logs"
                | "worker-commit-hooks"
                | "leases"
                | "arena"
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown live-state entry: {name}"),
            ));
        }
        if entry.file_type()?.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("live-state symlink is not migratable: {name}"),
            ));
        }
    }
    Ok(())
}

fn validate_quiescent_runs(source: &Path) -> io::Result<()> {
    let runs = source.join("runs-v2");
    if !runs.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(runs)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown runs-v2 entry",
            ));
        }
        let path = entry.path().join("manifest.json");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
        if value.get("schema").and_then(serde_json::Value::as_str) != Some("conductor/run@2") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown legacy run schema: {}", path.display()),
            ));
        }
        if value.get("lifecycle").and_then(serde_json::Value::as_str) != Some("finished") {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "state migration requires zero in-flight runs",
            ));
        }
    }
    Ok(())
}

fn copy_typed_file<T: serde::de::DeserializeOwned>(
    source: &Path,
    destination: &Path,
    name: &str,
) -> io::Result<u64> {
    let path = source.join(name);
    if !path.exists() {
        return Ok(0);
    }
    let bytes = fs::read(&path)?;
    serde_json::from_slice::<T>(&bytes).map_err(io::Error::other)?;
    fs::write(destination.join(name), bytes)?;
    Ok(1)
}

fn migrate_plans(source: &Path, destination: &Path) -> io::Result<u64> {
    let plans = source.join("plans");
    if !plans.exists() {
        return Ok(0);
    }
    let target = destination.join("plans");
    fs::create_dir(&target)?;
    let mut copied = 0_u64;
    for entry in fs::read_dir(plans)? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown legacy plan entry",
            ));
        }
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.path())?).map_err(io::Error::other)?;
        rename_object_key(
            &mut value,
            "bursar_roster_source_artifact",
            "musterroll_roster_source_artifact",
        )?;
        rewrite_owned_strings(&mut value);
        serde_json::from_value::<crate::plan::CyclePlan>(value.clone())
            .map_err(io::Error::other)?;
        let mut bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
        bytes.push(b'\n');
        fs::write(target.join(entry.file_name()), bytes)?;
        copied = copied.saturating_add(1);
    }
    Ok(copied)
}

fn migrate_runs(source: &Path, destination: &Path) -> io::Result<u64> {
    let runs = source.join("runs-v2");
    if !runs.exists() {
        return Ok(0);
    }
    let target = destination.join("runs-v2");
    fs::create_dir(&target)?;
    let mut copied = 0_u64;
    for entry in fs::read_dir(runs)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown runs-v2 entry",
            ));
        }
        let run_target = target.join(entry.file_name());
        fs::create_dir(&run_target)?;
        let source_run = entry.path();
        let roster_hash = migrate_roster_snapshot(&source_run, &run_target)?;
        if roster_hash.is_some() {
            copied = copied.saturating_add(1);
        }
        copied += migrate_run_manifest(&source_run, &run_target, roster_hash.as_deref())?;
        copied += migrate_run_events(&source_run, &run_target)?;
        copied += copy_run_payloads(&source_run, &run_target)?;
        crate::run::read_manifest(&run_target.join("manifest.json")).map_err(io::Error::other)?;
        crate::run::read_events(&run_target.join("events.jsonl")).map_err(io::Error::other)?;
    }
    Ok(copied)
}

fn migrate_roster_snapshot(source: &Path, destination: &Path) -> io::Result<Option<String>> {
    let path = source.join("roster.json");
    if !path.exists() {
        return Ok(None);
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some("bursar/roster@2") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown legacy roster snapshot schema",
        ));
    }
    value["schema"] = serde_json::Value::String("musterroll/roster@2".to_string());
    let mut bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    crate::musterroll::parse_roster_snapshot(&bytes).map_err(io::Error::other)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    fs::write(destination.join("roster.json"), bytes)?;
    Ok(Some(hash))
}

fn migrate_run_manifest(
    source: &Path,
    destination: &Path,
    roster_hash: Option<&str>,
) -> io::Result<u64> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json"))?)
            .map_err(io::Error::other)?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some("conductor/run@2") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown legacy run schema",
        ));
    }
    rename_optional_object_key(
        &mut value,
        "bursar_roster_artifact",
        "musterroll_roster_artifact",
    )?;
    value["schema"] = serde_json::Value::String(crate::run::RUN_SCHEMA.to_string());
    rewrite_owned_strings(&mut value);
    if let Some(hash) = roster_hash {
        rewrite_artifact_hash(&mut value, "roster.json", hash);
    }
    serde_json::from_value::<crate::run::RunManifest>(value.clone()).map_err(io::Error::other)?;
    let mut bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    fs::write(destination.join("manifest.json"), bytes)?;
    Ok(1)
}

fn migrate_run_events(source: &Path, destination: &Path) -> io::Result<u64> {
    let path = source.join("events.jsonl");
    let contents = fs::read_to_string(&path)?;
    let mut output = Vec::with_capacity(contents.len());
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: serde_json::Value = serde_json::from_str(line).map_err(io::Error::other)?;
        if value.get("schema").and_then(serde_json::Value::as_str) != Some("conductor/event@2") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown legacy event schema",
            ));
        }
        value["schema"] = serde_json::Value::String(crate::run::EVENT_SCHEMA.to_string());
        rewrite_owned_strings(&mut value);
        serde_json::from_value::<crate::run::RunEvent>(value.clone()).map_err(io::Error::other)?;
        serde_json::to_writer(&mut output, &value).map_err(io::Error::other)?;
        output.push(b'\n');
    }
    fs::write(destination.join("events.jsonl"), output)?;
    Ok(1)
}

fn copy_run_payloads(source: &Path, destination: &Path) -> io::Result<u64> {
    let mut copied = 0_u64;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("manifest.json" | "events.jsonl" | "roster.json")
        ) {
            continue;
        }
        let target = destination.join(&name);
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "run payload symlinks are not migratable",
            ));
        }
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copied += copy_run_payloads(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
            copied = copied.saturating_add(1);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown run payload entry",
            ));
        }
    }
    Ok(copied)
}

fn rename_object_key(value: &mut serde_json::Value, old: &str, new: &str) -> io::Result<()> {
    let object = value.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "legacy state envelope is not an object",
        )
    })?;
    let field = object.remove(old).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("legacy state envelope is missing {old}"),
        )
    })?;
    if object.insert(new.to_string(), field).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("legacy state envelope already contains {new}"),
        ));
    }
    Ok(())
}

fn rename_optional_object_key(
    value: &mut serde_json::Value,
    old: &str,
    new: &str,
) -> io::Result<()> {
    let object = value.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "legacy state envelope is not an object",
        )
    })?;
    if let Some(field) = object.remove(old) {
        if object.insert(new.to_string(), field).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("legacy state envelope already contains {new}"),
            ));
        }
    }
    Ok(())
}

fn rewrite_owned_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                rewrite_owned_strings(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                rewrite_owned_strings(value);
            }
        }
        serde_json::Value::String(text) => {
            if let Some(suffix) = text.strip_prefix("conductor/") {
                *text = format!("undertake/{suffix}");
            } else if let Some(suffix) = text.strip_prefix("bursar/") {
                *text = format!("musterroll/{suffix}");
            } else if text == "conductor-runtime" {
                *text = "undertake-runtime".to_string();
            } else if text == "bursar-api" {
                *text = "musterroll-api".to_string();
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn rewrite_artifact_hash(value: &mut serde_json::Value, path: &str, hash: &str) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                rewrite_artifact_hash(value, path, hash);
            }
        }
        serde_json::Value::Object(values) => {
            if values.get("path").and_then(serde_json::Value::as_str) == Some(path) {
                values.insert(
                    "sha256".to_string(),
                    serde_json::Value::String(hash.to_string()),
                );
            }
            for value in values.values_mut() {
                rewrite_artifact_hash(value, path, hash);
            }
        }
        _ => {}
    }
}

fn is_archive_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let Some(top_level) = relative.components().next() else {
        return false;
    };
    matches!(
        top_level.as_os_str().to_str(),
        Some("runs" | "logs" | "worker-commit-hooks" | "leases" | "arena")
    )
}

fn tree_digest(root: &Path) -> io::Result<String> {
    fn collect(root: &Path, path: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "state migration source contains a symlink",
                ));
            }
            entries.push(entry.path());
            if kind.is_dir() {
                collect(root, &entry.path(), entries)?;
            } else if !(kind.is_file() || kind.is_fifo() && is_archive_path(root, &entry.path())) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "state migration source contains an unknown entry",
                ));
            }
        }
        let _ = root;
        Ok(())
    }

    let mut entries = Vec::new();
    collect(root, root, &mut entries)?;
    entries.sort();
    let mut hash = Sha256::new();
    for path in entries {
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        hash.update(relative.as_os_str().as_encoded_bytes());
        let kind = fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            hash.update(b"d");
        } else if kind.is_file() {
            hash.update(b"f");
            hash.update(fs::read(&path)?);
        } else if kind.is_fifo() && is_archive_path(root, &path) {
            hash.update(b"p");
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state migration source contains an unknown entry",
            ));
        }
        hash.update(b"\0");
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn write_journal_creates_valid_json() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("undertake-state-test-{nanos}"));
        let _ = std::fs::remove_dir_all(&tmp);

        let entry = JournalEntry {
            id: "cycle-20260702-120000".to_string(),
            completed_at: "2026-07-02T12:00:00Z".to_string(),
            dry_run: true,
            summary: JournalSummary {
                scanned: 5,
                ready: 10,
                dispatched: 0,
                proposed: 3,
                verified: 0,
                flagged: 2,
                skipped: 1,
            },
        };

        write_journal(&tmp, &entry).unwrap();

        let path = tmp.join("journal.json");
        assert!(path.is_file());

        let content = std::fs::read_to_string(&path).unwrap();
        let journal: Journal = serde_json::from_str(&content).unwrap();
        assert_eq!(journal.last_cycle.id, "cycle-20260702-120000");
        assert!(journal.last_cycle.dry_run);
        assert_eq!(journal.last_cycle.summary.scanned, 5);
        assert_eq!(journal.last_cycle.summary.ready, 10);
        assert_eq!(journal.last_cycle.summary.proposed, 3);

        let _ = std::fs::remove_dir_all(&tmp);
    }
    fn migration_policy() -> crate::role_routing::RoutingPolicy {
        use std::num::NonZeroU32;

        crate::role_routing::RoutingPolicy::new(
            "a".repeat(64),
            [("planner", "alpha", 1), ("planner", "beta", 1)]
                .into_iter()
                .map(|(role, profile, weight)| {
                    crate::role_routing::RoleBinding::new(
                        crate::role_routing::RoleId::new(role).unwrap(),
                        crate::role_routing::ProfileId::new(profile).unwrap(),
                        NonZeroU32::new(weight).unwrap(),
                    )
                })
                .collect(),
        )
        .unwrap()
    }

    fn migration_root(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("undertake-migration-{label}-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn migration_refuses_in_place_before_writing() {
        let root = migration_root("in-place");
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();

        let error = migrate_live_state(&source, &source, &migration_policy())
            .expect_err("in-place migration must fail closed");

        assert!(error.to_string().contains("in-place"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migration_refuses_existing_destination_and_unknown_source_state() {
        let root = migration_root("refusals");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        let existing = migrate_live_state(&source, &destination, &migration_policy())
            .expect_err("existing destination must fail closed");
        assert!(existing.to_string().contains("already exists"));

        std::fs::remove_dir_all(&destination).unwrap();
        std::fs::write(source.join("unknown-state.bin"), b"unknown").unwrap();
        let unknown = migrate_live_state(&source, &destination, &migration_policy())
            .expect_err("unknown source state must fail closed");
        assert!(unknown.to_string().contains("unknown live-state entry"));
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migration_rejects_special_nodes_in_runtime_roots() {
        let root = migration_root("runtime-special");
        let source = root.join("source");
        let destination = root.join("destination");
        let plans = source.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let fifo = plans.join("live.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());

        let error = migrate_live_state(&source, &destination, &migration_policy())
            .expect_err("special runtime state must fail closed");
        assert!(error.to_string().contains("unknown entry"));
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migration_recognizes_exact_archive_roots_without_copying_them() {
        let root = migration_root("archives");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(&source).unwrap();
        for archive in ["runs", "logs", "worker-commit-hooks", "leases", "arena"] {
            let archive_root = source.join(archive);
            std::fs::create_dir_all(&archive_root).unwrap();
            std::fs::write(archive_root.join("retained-in-snapshot"), archive).unwrap();
        }

        let fifo = source.join("runs").join("worker-lineage.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());

        let source_digest = tree_digest(&source).unwrap();
        migrate_live_state(&source, &destination, &migration_policy()).unwrap();

        assert_eq!(tree_digest(&source).unwrap(), source_digest);
        for archive in ["runs", "logs", "worker-commit-hooks", "leases", "arena"] {
            assert!(
                !destination.join(archive).exists(),
                "archive-only root was copied: {archive}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migration_requires_zero_in_flight_runs() {
        let root = migration_root("in-flight");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(&source).unwrap();
        crate::run::RunHandle::create(
            &source,
            crate::run::RunJob::Review,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: "/repo/example".to_string(),
                    bead: None,
                },
                ..crate::run::NewRun::default()
            },
        )
        .unwrap();
        let run_dir = std::fs::read_dir(crate::run::runs_dir(&source))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest_path = run_dir.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["schema"] = serde_json::json!("conductor/run@2");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = migrate_live_state(&source, &destination, &migration_policy())
            .expect_err("nonterminal run must block migration");

        assert!(error.to_string().contains("zero in-flight"));
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the migration contract fixture covers every required state class in one end-to-end copy"
    )]
    #[test]
    fn migration_copies_current_state_without_rewinding_scores_or_mutating_source() {
        let root = migration_root("complete");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(&source).unwrap();

        write_journal(
            &source,
            &JournalEntry {
                id: "cycle-migrated".to_string(),
                completed_at: "2026-07-24T12:00:00Z".to_string(),
                dry_run: true,
                summary: JournalSummary {
                    scanned: 3,
                    ready: 2,
                    dispatched: 0,
                    proposed: 1,
                    verified: 0,
                    flagged: 0,
                    skipped: 0,
                },
            },
        )
        .unwrap();
        let ratchet = crate::ratchet::RatchetStore {
            repos: [(
                "example".to_string(),
                crate::ratchet::RatchetEntry {
                    clean_cycles: 3,
                    unlocked: true,
                },
            )]
            .into_iter()
            .collect(),
        };
        crate::ratchet::RatchetFileStore::open(&source)
            .save(&ratchet)
            .unwrap();
        crate::plan::CyclePlan::from_triage(
            "cycle-migrated",
            "2026-07-24T12:00:00Z",
            &crate::triage::Plan::default(),
        )
        .save(&source)
        .unwrap();
        let plan_path = source.join("plans/cycle-migrated.json");
        let mut plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&plan_path).unwrap()).unwrap();
        let roster_artifact = plan
            .as_object_mut()
            .unwrap()
            .remove("musterroll_roster_source_artifact")
            .unwrap();
        plan["bursar_roster_source_artifact"] = roster_artifact;
        std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();

        let policy = migration_policy();
        let router = crate::role_routing::RoleRouter::new(&source, policy.clone()).unwrap();
        let first = router
            .reserve(
                crate::role_routing::RunId::new("run-one").unwrap(),
                crate::role_routing::RoleId::new("planner").unwrap(),
                crate::run::PlanStage::Planner,
                &[],
            )
            .unwrap();
        let first_profile = first.selected_profile_id().as_str().to_string();
        router.cancel(&first).unwrap();
        let lane_path = std::fs::read_dir(source.join("role-routing/lanes"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("state.json");
        let mut lane: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lane_path).unwrap()).unwrap();
        lane["schema"] = serde_json::json!("conductor/role-lane@1");
        std::fs::write(&lane_path, serde_json::to_vec_pretty(&lane).unwrap()).unwrap();

        let mut run = crate::run::RunHandle::create(
            &source,
            crate::run::RunJob::Review,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: "/repo/example".to_string(),
                    bead: None,
                },
                ..crate::run::NewRun::default()
            },
        )
        .unwrap();
        run.finish("accepted").unwrap();
        let run_dir = std::fs::read_dir(crate::run::runs_dir(&source))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest_path = run_dir.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["schema"] = serde_json::json!("conductor/run@2");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let events_path = run_dir.join("events.jsonl");
        let legacy_events = std::fs::read_to_string(&events_path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
                event["schema"] = serde_json::json!("conductor/event@2");
                serde_json::to_string(&event).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&events_path, legacy_events).unwrap();
        let source_digest = tree_digest(&source).unwrap();

        let summary = migrate_live_state(&source, &destination, &policy).unwrap();

        assert_eq!(tree_digest(&source).unwrap(), source_digest);
        assert!(summary.files_copied >= 5);
        assert_eq!(
            crate::ratchet::RatchetFileStore::open(&destination)
                .load()
                .unwrap(),
            ratchet
        );
        crate::plan::CyclePlan::load(&destination, "cycle-migrated").unwrap();
        let migrated_run_dir = std::fs::read_dir(crate::run::runs_dir(&destination))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        crate::run::read_manifest(&migrated_run_dir.join("manifest.json")).unwrap();
        crate::run::read_events(&migrated_run_dir.join("events.jsonl")).unwrap();

        let restarted = crate::role_routing::RoleRouter::new(&destination, policy.clone()).unwrap();
        let next = restarted
            .reserve(
                crate::role_routing::RunId::new("run-two").unwrap(),
                crate::role_routing::RoleId::new("planner").unwrap(),
                crate::run::PlanStage::Planner,
                &[],
            )
            .unwrap();
        assert_ne!(next.selected_profile_id().as_str(), first_profile);
        let source_lane: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lane_path).unwrap()).unwrap();
        assert_eq!(source_lane["schema"], "conductor/role-lane@1");
        let _ = std::fs::remove_dir_all(root);
    }
}
