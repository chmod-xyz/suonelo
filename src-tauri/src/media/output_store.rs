use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{MediaError, blocking};

pub const RECEIPT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS media_receipts (
    location TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    identity TEXT NOT NULL,
    variant TEXT NOT NULL,
    sha256 TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS media_stages (location TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS media_pending_updates (
    location TEXT PRIMARY KEY,
    old_sha256 TEXT NOT NULL,
    new_sha256 TEXT NOT NULL,
    path_location TEXT NOT NULL DEFAULT '',
    audio_identity TEXT,
    audio_variant TEXT
);
"#;

const STAGE_PREFIX: &str = ".suonelo-stage-";
const LEGACY_STAGE_PREFIX: &str = ".qbdlx-stage-"; // Recovery compatibility only.

#[derive(Clone)]
pub(super) struct OutputStore {
    database: PathBuf,
    portable_root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    pub kind: &'static str,
    pub identity: String,
    pub variant: String,
}

#[derive(Clone)]
pub struct AudioReplacement {
    pub token: String,
    path: PathBuf,
    canonical: PathBuf,
    track: String,
    variant: String,
    digest: String,
}

pub fn migrate_replacement_receipts(db: &Connection) -> rusqlite::Result<()> {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('media_pending_updates') WHERE name='audio_identity')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        db.execute_batch(
            "ALTER TABLE media_pending_updates ADD COLUMN audio_identity TEXT;
             ALTER TABLE media_pending_updates ADD COLUMN audio_variant TEXT;"
        )?;
    }
    Ok(())
}

pub(super) struct Stage {
    file: tempfile::TempPath,
    location: String,
}

impl Stage {
    pub fn path(&self) -> &Path {
        &self.file
    }
}

fn storage_error() -> MediaError {
    MediaError::new(
        "file_index",
        "The finalized-file index could not be accessed.",
        true,
    )
}

fn conflict(path: &Path) -> MediaError {
    MediaError::new(
        "output_conflict",
        "The path already contains a file that could not be verified for this output.",
        false,
    )
    .at_path(path)
}

impl OutputStore {
    pub fn new(database: PathBuf, portable_root: PathBuf) -> Self {
        let portable_root = fs::canonicalize(&portable_root).unwrap_or(portable_root);
        Self {
            database,
            portable_root,
        }
    }

    fn connect(&self) -> Result<Connection, MediaError> {
        let connection = Connection::open(&self.database).map_err(|_| storage_error())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| storage_error())?;
        Ok(connection)
    }

    fn location(&self, path: &Path) -> String {
        if let Ok(relative) = path.strip_prefix(&self.portable_root) {
            format!("portable:{}", relative.to_string_lossy())
        } else {
            format!("external:{}", path.to_string_lossy())
        }
    }

    fn receipt_location(&self, path: &Path) -> String {
        let key = self.location(path);
        if cfg!(windows) {
            key.to_lowercase()
        } else {
            key
        }
    }

    fn stage_path(&self, location: &str) -> Option<PathBuf> {
        let (scope, path) = location.split_once(':')?;
        let path = Path::new(path);
        if ![STAGE_PREFIX, LEGACY_STAGE_PREFIX].iter().any(|prefix| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        }) {
            return None;
        }
        match scope {
            "portable"
                if path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))) =>
            {
                Some(self.portable_root.join(path))
            }
            "external"
                if path.is_absolute()
                    && !path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir)) =>
            {
                Some(path.to_owned())
            }
            _ => None,
        }
    }

    pub async fn stage(&self, directory: &Path, suffix: &str) -> Result<Stage, MediaError> {
        let store = self.clone();
        let directory = directory.to_owned();
        let suffix = suffix.to_owned();
        blocking(move || {
            let file = tempfile::Builder::new()
                .prefix(STAGE_PREFIX)
                .suffix(&suffix)
                .tempfile_in(&directory)
                .map_err(|error| MediaError::disk(error).at_path(&directory))?
                .into_temp_path();
            let location = store.location(&file);
            store
                .connect()?
                .execute(
                    "INSERT INTO media_stages(location) VALUES (?1)",
                    [&location],
                )
                .map_err(|_| storage_error())?;
            Ok(Stage { file, location })
        })
        .await
    }

    pub async fn discard(&self, stage: Stage) {
        let store = self.clone();
        let _ = blocking(move || {
            if stage.file.close().is_ok() {
                store
                    .connect()?
                    .execute(
                        "DELETE FROM media_stages WHERE location = ?1",
                        [&stage.location],
                    )
                    .map_err(|_| storage_error())?;
            }
            Ok(())
        })
        .await;
    }

    pub async fn cleanup_stages(&self) -> Result<(), MediaError> {
        let store = self.clone();
        blocking(move || {
            let connection = store.connect()?;
            let mut statement = connection
                .prepare("SELECT location FROM media_stages")
                .map_err(|_| storage_error())?;
            let locations = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| storage_error())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| storage_error())?;
            drop(statement);
            for location in locations {
                if let Some(path) = store.stage_path(&location)
                    && let Err(error) = fs::remove_file(path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    continue;
                }
                connection
                    .execute("DELETE FROM media_stages WHERE location = ?1", [&location])
                    .map_err(|_| storage_error())?;
            }
            let mut statement = connection
                .prepare("SELECT location, path_location FROM media_pending_updates")
                .map_err(|_| storage_error())?;
            let updates = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|_| storage_error())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| storage_error())?;
            drop(statement);
            for (location, raw_location) in updates {
                if let Some(path) = store.saved_path(if raw_location.is_empty() {
                    &location
                } else {
                    &raw_location
                }) {
                    Self::reconcile_update(
                        &connection,
                        &location,
                        &path,
                        &CancellationToken::new(),
                    )?;
                }
            }
            Ok(())
        })
        .await
    }

    fn reuse(
        connection: &Connection,
        key: &str,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        Self::reconcile_update(connection, key, path, cancel)?;
        match fs::symlink_metadata(path) {
            Ok(info) if info.is_file() => {}
            Ok(_) => return Err(conflict(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(MediaError::disk(error)),
        }
        let stored: Option<(String, String)> = connection.query_row(
            "SELECT variant, sha256 FROM media_receipts WHERE location = ?1 AND kind = ?2 AND identity = ?3",
            params![key, receipt.kind, receipt.identity],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(|_| storage_error())?;
        let Some((variant, expected)) = stored else {
            return Err(conflict(path));
        };
        if !same_variant(&variant, &receipt.variant, receipt.kind) {
            return Err(conflict(path));
        }
        if expected != file_hash(path, Some(cancel))? {
            return Err(conflict(path));
        }
        Ok(true)
    }

    fn saved_path(&self, location: &str) -> Option<PathBuf> {
        let (scope, value) = location.split_once(':')?;
        let path = Path::new(value);
        match scope {
            "portable"
                if path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))) =>
            {
                Some(self.portable_root.join(path))
            }
            "external"
                if path.is_absolute()
                    && !path
                        .components()
                        .any(|component| matches!(component, Component::ParentDir)) =>
            {
                Some(path.to_owned())
            }
            _ => None,
        }
    }

    fn reconcile_update(
        connection: &Connection,
        key: &str,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), MediaError> {
        let pending: Option<(String, String, Option<String>, Option<String>)> = connection
            .query_row(
                "SELECT old_sha256, new_sha256, audio_identity, audio_variant FROM media_pending_updates WHERE location = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|_| storage_error())?;
        let Some((old, new, audio_identity, audio_variant)) = pending else {
            return Ok(());
        };
        let digest = match file_hash(path, Some(cancel)) {
            Ok(digest) => Some(digest),
            Err(error) if error.code == "disk" && !path.exists() => None,
            Err(error) => return Err(error),
        };
        let transaction = connection
            .unchecked_transaction()
            .map_err(|_| storage_error())?;
        if digest.as_deref() == Some(new.as_str()) {
            if let (Some(identity), Some(variant)) = (audio_identity, audio_variant) {
                transaction.execute(
                    "UPDATE media_receipts SET sha256=?1, variant=?2 WHERE location=?3 AND kind='audio' AND identity=?4 AND sha256=?5",
                    params![new, variant, key, identity, old],
                ).map_err(|_| storage_error())?;
            } else {
                transaction.execute("UPDATE media_receipts SET sha256 = ?1 WHERE location = ?2 AND kind = 'playlist' AND sha256 = ?3", params![new, key, old]).map_err(|_| storage_error())?;
            }
        }
        transaction
            .execute(
                "DELETE FROM media_pending_updates WHERE location = ?1",
                [key],
            )
            .map_err(|_| storage_error())?;
        transaction.commit().map_err(|_| storage_error())
    }

    pub async fn verify(
        &self,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        let receipt = receipt.clone();
        let cancel = cancel.clone();
        blocking(move || {
            Self::reuse(
                &store.connect()?,
                &store.receipt_location(&path),
                &path,
                &receipt,
                &cancel,
            )
        })
        .await
    }

    pub async fn audio_lyrics_note(
        &self,
        path: &Path,
    ) -> Result<Option<(String, bool)>, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        blocking(move || {
            let variant: Option<String> = store
                .connect()?
                .query_row(
                    "SELECT variant FROM media_receipts WHERE location = ?1 AND kind IN ('audio', 'audio_preview')",
                    [store.receipt_location(&path)],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| storage_error())?;
            Ok(variant
                .and_then(|variant| serde_json::from_str::<serde_json::Value>(&variant).ok())
                .and_then(|value| {
                    let note = value.get("_lyrics")?.as_array()?;
                    Some((note.first()?.as_str()?.to_owned(), note.get(1)?.as_bool()?))
                }))
        })
        .await
    }

    pub async fn publish(
        &self,
        stage: Stage,
        path: &Path,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        self.publish_file(stage, path, receipt, false, None, cancel).await
    }

    pub async fn publish_playlist(
        &self,
        stage: Stage,
        path: &Path,
        receipt: &Receipt,
        allow_refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        self.publish_file(stage, path, receipt, allow_refresh, None, cancel)
            .await
    }

    fn replacement(
        &self, db: &Connection, path: &Path, track: &str, cancel: &CancellationToken,
    ) -> Result<AudioReplacement, MediaError> {
        let key = self.receipt_location(path);
        Self::reconcile_update(db, &key, path, cancel)?;
        if !fs::symlink_metadata(path).is_ok_and(|file| file.is_file()) {
            return Err(conflict(path));
        }
        let canonical = dunce::canonicalize(path).map_err(|_| conflict(path))?;
        let (variant, digest): (String, String) = db.query_row(
            "SELECT variant, sha256 FROM media_receipts WHERE location=?1 AND kind='audio' AND identity=?2",
            params![key, track],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(|_| storage_error())?.ok_or_else(|| conflict(path))?;
        if file_hash(path, Some(cancel))? != digest {
            return Err(conflict(path));
        }
        Ok(AudioReplacement {
            token: format!("{:032x}", rand::random::<u128>()),
            path: path.to_owned(), canonical, track: track.to_owned(), variant, digest,
        })
    }

    fn check_replacement(
        &self, db: &Connection, path: &Path, track: &str, approval: &AudioReplacement, cancel: &CancellationToken,
    ) -> Result<(), MediaError> {
        if approval.path != path || approval.track != track {
            return Err(conflict(path));
        }
        let current = self.replacement(db, path, track, cancel)?;
        if current.canonical != approval.canonical || current.variant != approval.variant || current.digest != approval.digest {
            return Err(conflict(path));
        }
        Ok(())
    }

    pub async fn offer_replacement(&self, path: &Path, track: &str, cancel: &CancellationToken) -> Result<AudioReplacement, MediaError> {
        let (store, path, track, cancel) = (self.clone(), path.to_owned(), track.to_owned(), cancel.clone());
        blocking(move || store.replacement(&store.connect()?, &path, &track, &cancel)).await
    }

    pub async fn verify_replacement(&self, path: &Path, track: &str, approval: &AudioReplacement, cancel: &CancellationToken) -> Result<(), MediaError> {
        let (store, path, track, approval, cancel) = (self.clone(), path.to_owned(), track.to_owned(), approval.clone(), cancel.clone());
        blocking(move || store.check_replacement(&store.connect()?, &path, &track, &approval, &cancel)).await
    }

    pub async fn publish_replacement(
        &self, stage: Stage, path: &Path, receipt: &Receipt, approval: &AudioReplacement, cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        self.publish_file(stage, path, receipt, true, Some(approval.clone()), cancel).await
    }

    pub async fn verified_audio(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        let cancel = cancel.clone();
        blocking(move || {
            if !fs::symlink_metadata(&path).is_ok_and(|info| info.is_file()) {
                return Ok(false);
            }
            let connection = store.connect()?;
            let expected: Option<String> = connection
                .query_row(
                    "SELECT sha256 FROM media_receipts WHERE location = ?1 AND kind IN ('audio', 'audio_preview')",
                    [store.receipt_location(&path)],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| storage_error())?;
            let Some(expected) = expected else {
                return Ok(false);
            };
            match file_hash(&path, Some(&cancel)) {
                Ok(actual) => Ok(expected == actual),
                Err(error) if error.code == "disk" => Ok(false),
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn publish_file(
        &self,
        stage: Stage,
        path: &Path,
        receipt: &Receipt,
        allow_refresh: bool,
        replacement: Option<AudioReplacement>,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let store = self.clone();
        let path = path.to_owned();
        let receipt = receipt.clone();
        let cancel = cancel.clone();
        blocking(move || {
            let connection = store.connect()?;
            let mut file = Some(stage.file);
            let result = (|| {
                let key = store.receipt_location(&path);
                let existing = if let Some(approval) = &replacement {
                    store.check_replacement(&connection, &path, &receipt.identity, approval, &cancel)?;
                    true
                } else {
                    Self::reuse(&connection, &key, &path, &receipt, &cancel)?
                };
                if existing && !allow_refresh && receipt.kind != "playlist" { return Ok(true); }
                let stage = file.as_ref().expect("unpublished temporary path");
                let sha256 = file_hash(stage, Some(&cancel))?;
                cancel.check()?;
                fs::OpenOptions::new().write(true).open(stage).and_then(|file| file.sync_all()).map_err(MediaError::disk)?;
                let previous = if existing {
                    let sha = file_hash(&path, Some(&cancel))?;
                    if sha == sha256 && replacement.is_none() { return Ok(true); }
                    if !allow_refresh { return Err(conflict(&path)); }
                    Some(sha)
                } else { None };
                if let Some(previous) = &previous {
                    if let Some(approval) = &replacement
                        && &approval.digest != previous
                    {
                        return Err(conflict(&path));
                    }
                    let recorded: String = connection.query_row("SELECT sha256 FROM media_receipts WHERE location = ?1", [&key], |row| row.get(0)).map_err(|_| storage_error())?;
                    if &recorded != previous { return Err(conflict(&path)); }
                    let audio = replacement.as_ref().map(|_| (&receipt.identity, &receipt.variant));
                    connection.execute(
                        "INSERT OR REPLACE INTO media_pending_updates(location, old_sha256, new_sha256, path_location, audio_identity, audio_variant) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![key, previous, sha256, store.location(&path), audio.map(|(id, _)| id), audio.map(|(_, variant)| variant)],
                    ).map_err(|_| storage_error())?;
                } else {
                    connection.execute(
                        "INSERT OR REPLACE INTO media_receipts(location, kind, identity, variant, sha256) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![key, receipt.kind, receipt.identity, receipt.variant, sha256],
                    ).map_err(|_| storage_error())?;
                }
                let stage = file.take().expect("unpublished temporary path");
                let result = if existing { stage.persist(&path) } else { stage.persist_noclobber(&path) };
                if let Err(error) = result {
                    file = Some(error.path);
                    if previous.is_some() {
                        let _ = connection.execute("DELETE FROM media_pending_updates WHERE location = ?1", [&key]);
                    } else {
                        let _ = connection.execute("DELETE FROM media_receipts WHERE location = ?1 AND sha256 = ?2", params![key, sha256]);
                    }
                    return Err(if error.error.kind() == io::ErrorKind::AlreadyExists { conflict(&path) } else { MediaError::disk(error.error).at_path(&path) });
                }
                if previous.is_some() {
                    Self::reconcile_update(&connection, &key, &path, &cancel)?;
                }
                Ok(false)
            })();
            if file.is_none_or(|file| file.close().is_ok()) {
                let _ = connection.execute("DELETE FROM media_stages WHERE location = ?1", [&stage.location]);
            }
            result.map_err(|error: MediaError| error.at_path(&path))
        }).await
    }
}

fn same_variant(stored: &str, expected: &str, kind: &str) -> bool {
    if stored == expected {
        return true;
    }
    if !matches!(kind, "audio" | "audio_preview") {
        return false;
    }
    let parse = |text: &str| {
        let mut value = serde_json::from_str::<serde_json::Value>(text).ok()?;
        let fields = value.as_object_mut()?;
        fields.remove("_lyrics");
        fields.remove("reencode_flac");
        Some(value)
    };
    match (parse(stored), parse(expected)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn file_hash(path: &Path, cancel: Option<&CancellationToken>) -> Result<String, MediaError> {
    let mut source = fs::File::open(path).map_err(MediaError::disk)?;
    let mut hasher = Sha256::new();
    let mut bytes = vec![0_u8; 128 * 1024];
    loop {
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
        let read = source.read(&mut bytes).map_err(MediaError::disk)?;
        if read == 0 {
            break;
        }
        hasher.update(&bytes[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(super) trait CheckCancelled {
    fn check(&self) -> Result<(), MediaError>;
}

impl CheckCancelled for CancellationToken {
    fn check(&self) -> Result<(), MediaError> {
        if self.is_cancelled() {
            Err(MediaError::cancelled())
        } else {
            Ok(())
        }
    }
}

pub(super) async fn safe_directory(root: &Path, path: &Path) -> Result<PathBuf, MediaError> {
    let root = root.to_owned();
    let path = path.to_owned();
    blocking(move || {
        let outside = || {
            MediaError::new(
                "output_location",
                "The rendered path leaves the selected output folder.",
                false,
            )
        };
        let relative = path.strip_prefix(&root).map_err(|_| outside())?;
        let canonical_root = fs::canonicalize(&root).map_err(MediaError::disk)?;
        if !canonical_root.is_dir() {
            return Err(outside());
        }
        let mut current = canonical_root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(outside());
            };
            current.push(name);
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(MediaError::disk(error)),
            }
            let info = fs::symlink_metadata(&current).map_err(MediaError::disk)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if info.file_attributes() & 0x400 != 0 {
                    return Err(outside());
                } // FILE_ATTRIBUTE_REPARSE_POINT
            }
            if !info.is_dir() || info.file_type().is_symlink() {
                return Err(outside());
            }
            current = fs::canonicalize(&current).map_err(MediaError::disk)?;
            if !current.starts_with(&canonical_root) {
                return Err(outside());
            }
        }
        Ok(current)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mp3_preview_receipt_cannot_satisfy_or_replace_full_audio() {
        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        let database = root.join("receipts.sqlite3");
        #[cfg(not(target_os = "macos"))]
        crate::storage::Storage::initialize(database.clone()).unwrap();
        #[cfg(target_os = "macos")]
        Connection::open(&database).unwrap().execute_batch(RECEIPT_SCHEMA).unwrap();
        let store = OutputStore::new(database.clone(), root.clone());
        let path = root.join("track - Preview.mp3");
        let cancel = CancellationToken::new();
        let preview = Receipt { kind: "audio_preview", identity: "42".into(), variant: "mp3".into() };
        let full = Receipt { kind: "audio", ..preview.clone() };
        #[cfg(not(target_os = "macos"))]
        let full_path = {
            let full_path = root.join("track.mp3");
            let full_stage = store.stage(&root, ".mp3").await.unwrap();
            fs::write(full_stage.path(), "full audio").unwrap();
            store.publish(full_stage, &full_path, &full, &cancel).await.unwrap();
            full_path
        };
        let stage = store.stage(&root, ".mp3").await.unwrap();
        fs::write(stage.path(), "preview audio").unwrap();
        store.publish(stage, &path, &preview, &cancel).await.unwrap();
        assert!(store.verify(&path, &preview, &cancel).await.unwrap());
        assert_eq!(store.verify(&path, &full, &cancel).await.unwrap_err().code, "output_conflict");
        assert!(store.offer_replacement(&path, "42", &cancel).await.is_err());
        #[cfg(not(target_os = "macos"))]
        assert!(store.verify(&full_path, &full, &cancel).await.unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "preview audio");
        #[cfg(not(target_os = "macos"))]
        {
            let db = Connection::open(&database).unwrap();
            assert_eq!(db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), 4);
            assert_eq!(db.pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0)).unwrap(), "ok");
            fs::write(&path, "tampered preview").unwrap();
            assert!(store.verify(&path, &preview, &cancel).await.is_err());
        }
    }

    #[tokio::test]
    async fn confirmed_audio_replacement_revalidates_the_path_identity_and_old_digest() {
        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        let database = root.join("receipts.sqlite3");
        Connection::open(&database).unwrap().execute_batch(RECEIPT_SCHEMA).unwrap();
        let store = OutputStore::new(database, root.clone());
        let path = root.join("track.flac");
        let cancel = CancellationToken::new();
        let old = Receipt { kind: "audio", identity: "42".into(), variant: "old tags".into() };
        let new = Receipt { variant: "new tags".into(), ..old.clone() };
        let first = store.stage(&root, ".flac").await.unwrap();
        fs::write(first.path(), "old audio").unwrap();
        store.publish(first, &path, &old, &cancel).await.unwrap();
        assert!(store.offer_replacement(&path, "43", &cancel).await.is_err());
        let approval = store.offer_replacement(&path, "42", &cancel).await.unwrap();
        let unapproved = store.stage(&root, ".flac").await.unwrap();
        fs::write(unapproved.path(), "new audio").unwrap();
        assert_eq!(store.publish(unapproved, &path, &new, &cancel).await.unwrap_err().code, "output_conflict");
        fs::write(&path, "outside edit").unwrap();
        let stale = store.stage(&root, ".flac").await.unwrap();
        fs::write(stale.path(), "new audio").unwrap();
        assert_eq!(store.publish_replacement(stale, &path, &new, &approval, &cancel).await.unwrap_err().code, "output_conflict");
        assert_eq!(fs::read_to_string(&path).unwrap(), "outside edit");
        fs::write(&path, "old audio").unwrap();
        let approved = store.stage(&root, ".flac").await.unwrap();
        fs::write(approved.path(), "new audio").unwrap();
        assert!(!store.publish_replacement(approved, &path, &new, &approval, &cancel).await.unwrap());
        assert!(store.verify(&path, &new, &cancel).await.unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new audio");
        assert!(store.verify_replacement(&path, "42", &approval, &cancel).await.is_err());
    }

    #[tokio::test]
    async fn published_file_requires_identity_options_and_digest() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("receipts.sqlite3");
        Connection::open(&database)
            .unwrap()
            .execute_batch(RECEIPT_SCHEMA)
            .unwrap();
        let store = OutputStore::new(database, root.path().to_owned());
        let root = fs::canonicalize(root.path()).unwrap();
        let stage = store.stage(&root, ".flac").await.unwrap();
        let dest = root.join("track.flac");
        fs::write(stage.path(), "first").unwrap();
        let cancel = CancellationToken::new();
        let mut receipt = Receipt {
            kind: "audio",
            identity: "42".into(),
            variant: "default".into(),
        };
        assert!(
            !store
                .publish(stage, &dest, &receipt, &cancel)
                .await
                .unwrap()
        );
        assert!(store.verify(&dest, &receipt, &cancel).await.unwrap());
        receipt.variant = "different tags".into();
        assert_eq!(
            store
                .verify(&dest, &receipt, &cancel)
                .await
                .unwrap_err()
                .code,
            "output_conflict"
        );
        receipt.variant = "default".into();
        fs::write(&dest, "user edit").unwrap();
        assert_eq!(
            store
                .verify(&dest, &receipt, &cancel)
                .await
                .unwrap_err()
                .code,
            "output_conflict"
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "user edit");
    }

    #[tokio::test]
    async fn interrupted_playlist_replacement_recovers_only_a_known_digest() {
        let root = tempfile::tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let database = root_path.join("receipts.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch(RECEIPT_SCHEMA).unwrap();
        let store = OutputStore::new(database, root_path.clone());
        let path = root_path.join("saved.m3u");
        let stage = store.stage(&root_path, ".m3u").await.unwrap();
        fs::write(stage.path(), "old export\n").unwrap();
        let cancel = CancellationToken::new();
        let receipt = Receipt {
            kind: "playlist",
            identity: "42".into(),
            variant: "m3u".into(),
        };
        store
            .publish(stage, &path, &receipt, &cancel)
            .await
            .unwrap();
        let old = file_hash(&path, None).unwrap();
        let replacement = root_path.join("next.m3u");
        fs::write(&replacement, "new export\n").unwrap();
        let new = file_hash(&replacement, None).unwrap();
        let key = store.receipt_location(&path);
        let insert = || {
            connection.execute("INSERT INTO media_pending_updates(location, old_sha256, new_sha256) VALUES (?1, ?2, ?3)", params![key, old, new]).unwrap()
        };
        insert();
        assert!(store.verify(&path, &receipt, &cancel).await.unwrap());
        insert();
        fs::write(&path, "new export\n").unwrap();
        assert!(store.verify(&path, &receipt, &cancel).await.unwrap());
        let recorded: String = connection
            .query_row(
                "SELECT sha256 FROM media_receipts WHERE location = ?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, new);
        connection.execute("INSERT INTO media_pending_updates(location, old_sha256, new_sha256) VALUES (?1, ?2, ?3)", params![key, new, old]).unwrap();
        fs::write(&path, "a user changed the export\n").unwrap();
        assert_eq!(
            store
                .verify(&path, &receipt, &cancel)
                .await
                .unwrap_err()
                .code,
            "output_conflict"
        );
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "a user changed the export\n"
        );
    }
}
