//! Portable-profile migration. Caller owns the unchanged data/session.lock throughout.
use crate::error::{AppError, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const LEGACY: &str = "qbdlx.sqlite3"; // Required legacy profile compatibility.
const CURRENT: &str = "suonelo.sqlite3";
const TEMP: &str = "suonelo.sqlite3.migrating";
const JOURNAL: &str = "suonelo-migration.json";

#[derive(Serialize, Deserialize)]
struct Record {
    backup: String,
    fingerprint: String,
    complete: bool,
}

fn problem(message: impl Into<String>) -> AppError {
    AppError::new("profile_migration", "storage", message, true)
}
fn io(error: std::io::Error) -> AppError {
    problem(format!(
        "Profile migration could not complete: {error}. Originals remain recoverable."
    ))
}
fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(AppError::database)?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(AppError::database)?;
    Ok(db)
}
fn checked(db: &Connection) -> Result<()> {
    let integrity: String = db
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(AppError::database)?;
    let schema: u32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(AppError::database)?;
    if integrity != "ok" || schema > crate::storage::SCHEMA_VERSION {
        return Err(problem(
            "The profile failed integrity/schema validation. Keep its backup and use a compatible application.",
        ));
    }
    Ok(())
}
fn fingerprint(db: &Connection) -> Result<String> {
    checked(db)?;
    let mut hash = Sha256::new();
    let mut tables=db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(AppError::database)?;
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(AppError::database)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(AppError::database)?;
    for name in names {
        hash.update(name.as_bytes());
        let quoted = name.replace('"', "\"\"");
        let probe = db
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .map_err(AppError::database)?;
        let columns = probe.column_count();
        drop(probe);
        let order = (1..=columns)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = db
            .prepare(&format!("SELECT * FROM \"{quoted}\" ORDER BY {order}"))
            .map_err(AppError::database)?;
        let mut rows = stmt.query([]).map_err(AppError::database)?;
        while let Some(row) = rows.next().map_err(AppError::database)? {
            hash.update(b"row:");
            for column in 0..columns {
                let value = format!("{:?}", row.get_ref(column).map_err(AppError::database)?);
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            }
        }
    }
    Ok(hex::encode(hash.finalize()))
}
fn snapshot(db: &Connection, path: &Path) -> Result<()> {
    db.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])
        .map_err(AppError::database)?;
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io)?
        .sync_all()
        .map_err(io)
}
fn copy_profile(source: &Path, target: &Path, top: bool) -> Result<()> {
    fs::create_dir_all(target).map_err(io)?;
    for entry in fs::read_dir(source).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if top && (name_text.starts_with(LEGACY) || [JOURNAL, TEMP].contains(&name_text.as_ref())) {
            continue;
        }
        let kind = entry.file_type().map_err(io)?;
        if kind.is_symlink() {
            return Err(problem(
                "A profile contains a linked path; resolve it before migration.",
            ));
        }
        if kind.is_dir() {
            copy_profile(&entry.path(), &target.join(name), false)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target.join(name)).map_err(io)?;
        }
    }
    Ok(())
}
fn write_record(data: &Path, record: &Record) -> Result<()> {
    let temporary = data.join("suonelo-migration.json.new");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(record)
            .map_err(|_| problem("Could not encode migration state."))?,
    )
    .map_err(io)?;
    fs::OpenOptions::new()
        .write(true)
        .open(&temporary)
        .map_err(io)?
        .sync_all()
        .map_err(io)?;
    fs::rename(temporary, data.join(JOURNAL)).map_err(io)
}
pub fn prepare(data: &Path) -> Result<PathBuf> {
    prepare_steps(data, |_| Ok(()))
}

fn prepare_steps(data: &Path, checkpoint: impl Fn(&str) -> Result<()>) -> Result<PathBuf> {
    let legacy = data.join(LEGACY);
    let current = data.join(CURRENT);
    let temporary = data.join(TEMP);
    let journal = data.join(JOURNAL);
    if !legacy.exists() && !journal.exists() {
        if current.exists() {
            checked(&open(&current)?)?;
        }
        return Ok(current);
    }
    let mut record: Record = if journal.exists() {
        serde_json::from_slice(&fs::read(&journal).map_err(io)?).map_err(|_|problem("Migration state is unreadable. Preserve both databases and restore a consistent profile backup."))?
    } else {
        if current.exists() {
            return Err(problem(format!(
                "Both {} and {} exist. Close the app and choose a consistent profile backup; neither database was overwritten.",
                legacy.display(),
                current.display()
            )));
        }
        if temporary.exists() {
            return Err(problem(
                "An unrecognized migration temporary file exists. Preserve it and resolve the profile conflict.",
            ));
        }
        let source = open(&legacy)?;
        checked(&source)?;
        source
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(AppError::database)?;
        let signature = fingerprint(&source)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| problem("System time is invalid."))?
            .as_nanos();
        let backup = format!("pre-suonelo-{stamp}");
        let folder = data
            .parent()
            .ok_or_else(|| problem("Invalid portable root."))?
            .join("profile-backups")
            .join(&backup);
        copy_profile(data, &folder, true)?;
        snapshot(&source, &folder.join(LEGACY))?;
        if fingerprint(&open(&folder.join(LEGACY))?)? != signature {
            return Err(problem("Profile backup verification failed."));
        }
        let record = Record {
            backup,
            fingerprint: signature,
            complete: false,
        };
        write_record(data, &record)?;
        checkpoint("backup")?;
        record
    };
    if !record.backup.starts_with("pre-suonelo-")
        || !record
            .backup
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '-')
    {
        return Err(problem("Migration backup path is invalid."));
    }
    let backup = data
        .parent()
        .ok_or_else(|| problem("Invalid portable root."))?
        .join("profile-backups")
        .join(&record.backup);
    if record.complete {
        if legacy.exists() {
            return Err(problem(
                "A legacy database appeared after migration. Preserve both profiles and resolve the conflict.",
            ));
        }
        checked(&open(&current)?)?;
        return Ok(current);
    }
    if !current.exists() {
        if !legacy.exists() {
            return Err(problem(
                "Migration source is missing. Restore the complete consistent profile backup.",
            ));
        }
        let source = open(&legacy)?;
        if fingerprint(&source)? != record.fingerprint {
            return Err(problem(
                "The legacy profile changed after migration began. Preserve both profiles and resolve the conflict.",
            ));
        }
        if temporary.exists() {
            fs::remove_file(&temporary).map_err(io)?;
        }
        snapshot(&source, &temporary)?;
        if fingerprint(&open(&temporary)?)? != record.fingerprint {
            return Err(problem("New profile verification failed."));
        }
        checkpoint("temporary")?;
        // Same-directory hard-link publication is atomic and fails if the destination exists.
        fs::hard_link(&temporary, &current).map_err(io)?;
        fs::remove_file(&temporary).map_err(io)?;
        checkpoint("published")?;
    }
    if fingerprint(&open(&current)?)? != record.fingerprint {
        return Err(problem(
            "Published profile differs from its verified migration snapshot.",
        ));
    }
    let archive = backup.join("legacy-original");
    fs::create_dir_all(&archive).map_err(io)?;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let from = data.join(format!("{LEGACY}{suffix}"));
        let to = archive.join(format!("{LEGACY}{suffix}"));
        if from.exists() {
            if to.exists() {
                return Err(problem(
                    "Legacy archive conflict; originals were preserved.",
                ));
            }
            fs::rename(from, to).map_err(io)?;
        }
    }
    checkpoint("archived")?;
    record.complete = true;
    write_record(data, &record)?;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn legacy(data: &Path) -> Connection {
        fs::create_dir_all(data).unwrap();
        let db = Connection::open(data.join(LEGACY)).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA user_version=2;CREATE TABLE settings(id INTEGER PRIMARY KEY,value TEXT);INSERT INTO settings VALUES(1,'saved preferences');CREATE TABLE bookmarks(id INTEGER PRIMARY KEY,value TEXT);INSERT INTO bookmarks VALUES(1,'saved bookmark');").unwrap();
        fs::write(data.join("credentials.vault"), b"opaque encrypted fixture").unwrap();
        db
    }
    #[test]
    fn migrates_wal_preserves_records_and_vault_and_restarts() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let db = legacy(&data);
        // The migration owns the profile lock in production; fixture leaves a WAL connection open.
        drop(db);
        let current = prepare(&data).unwrap();
        assert_eq!(current, data.join(CURRENT));
        assert_eq!(
            open(&current)
                .unwrap()
                .query_row("SELECT count(*) FROM bookmarks", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            fs::read(data.join("credentials.vault")).unwrap(),
            b"opaque encrypted fixture"
        );
        assert!(!data.join(LEGACY).exists());
        assert!(data.join(JOURNAL).exists());
        assert_eq!(prepare(&data).unwrap(), current);
    }
    #[test]
    fn conflicting_databases_are_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        drop(legacy(&data));
        fs::copy(data.join(LEGACY), data.join(CURRENT)).unwrap();
        let before = fs::read(data.join(CURRENT)).unwrap();
        assert!(prepare(&data).is_err());
        assert_eq!(fs::read(data.join(CURRENT)).unwrap(), before);
    }
    #[test]
    fn interrupted_steps_resume_without_data_loss() {
        for stop in ["backup", "temporary", "published", "archived"] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            drop(legacy(&data));
            assert!(
                prepare_steps(&data, |step| if step == stop {
                    Err(problem("injected interruption"))
                } else {
                    Ok(())
                })
                .is_err()
            );
            let current = prepare(&data).unwrap();
            assert_eq!(
                open(&current)
                    .unwrap()
                    .query_row("SELECT value FROM settings", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "saved preferences"
            );
        }
    }
    #[test]
    fn disk_full_error_before_publication_keeps_profile_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        drop(legacy(&data));
        let before = fingerprint(&open(&data.join(LEGACY)).unwrap()).unwrap();
        let error = prepare_steps(&data, |step| {
            if step == "temporary" {
                Err(io(std::io::Error::from(std::io::ErrorKind::StorageFull)))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.message.contains("recoverable"));
        assert!(!data.join(CURRENT).exists());
        assert_eq!(
            fingerprint(&open(&data.join(LEGACY)).unwrap()).unwrap(),
            before
        );
        assert_eq!(
            fs::read(data.join("credentials.vault")).unwrap(),
            b"opaque encrypted fixture"
        );
        let current = prepare(&data).unwrap();
        assert_eq!(fingerprint(&open(&current).unwrap()).unwrap(), before);
    }

    #[test]
    fn source_change_after_interruption_is_a_recoverable_conflict() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        drop(legacy(&data));
        assert!(prepare_steps(&data, |_| Err(problem("stop"))).is_err());
        open(&data.join(LEGACY))
            .unwrap()
            .execute("INSERT INTO bookmarks VALUES(2,'newer')", [])
            .unwrap();
        assert!(prepare(&data).is_err());
        assert!(data.join(LEGACY).exists());
    }
    #[test]
    fn corrupt_and_newer_profiles_are_preserved() {
        for corrupt in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            drop(legacy(&data));
            if corrupt {
                fs::write(data.join(LEGACY), b"not sqlite").unwrap();
            } else {
                open(&data.join(LEGACY))
                    .unwrap()
                    .pragma_update(None, "user_version", 99)
                    .unwrap();
            }
            assert!(prepare(&data).is_err());
            assert!(data.join(LEGACY).exists());
        }
    }
    #[test]
    fn fresh_profile_does_not_invent_a_migration() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        assert_eq!(prepare(&data).unwrap(), data.join(CURRENT));
        assert!(!data.join(JOURNAL).exists());
    }
}

#[cfg(test)]
mod crash_wal_tests {
    use super::*;
    #[test]
    fn wal_fixture_child() {
        let Some(path) = std::env::var_os("SUONELO_TEST_WAL_PATH") else {
            return;
        };
        let db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;PRAGMA user_version=2;CREATE TABLE settings(id INTEGER PRIMARY KEY,value TEXT);INSERT INTO settings VALUES(1,'uncheckpointed');").unwrap();
        std::process::exit(0);
    }
    #[test]
    fn recovers_an_actual_uncheckpointed_wal_after_process_exit() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "profile_migration::crash_wal_tests::wal_fixture_child",
            ])
            .env("SUONELO_TEST_WAL_PATH", data.join(LEGACY));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        assert!(command.status().unwrap().success());
        assert!(data.join(format!("{LEGACY}-wal")).metadata().unwrap().len() > 0);
        let current = prepare(&data).unwrap();
        assert_eq!(
            open(&current)
                .unwrap()
                .query_row("SELECT value FROM settings", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "uncheckpointed"
        );
    }
}
