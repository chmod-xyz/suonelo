use crate::{
    error::{AppError, Result},
    locations::{SavedLocation, music_folder},
    media::{self, ArtworkResolution, LyricsMode, OutputOptions},
    qobuz::{BookmarkSummary, CanonicalResource, ResourceKey, ResourceKind},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub revision: u64,
    #[serde(flatten)]
    pub output: OutputOptions,
    pub interval_seconds: f64,
    pub max_connections: MaxConnections,
    pub download_mode: DownloadMode,
    pub output_location: Option<SavedLocation>,
    pub appearance: Appearance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) light_theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dark_theme: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 0,
            output: OutputOptions::default(),
            interval_seconds: 2.0,
            max_connections: MaxConnections::default(),
            download_mode: DownloadMode::default(),
            output_location: None,
            appearance: Appearance::System,
            theme: Some("Suonelo".into()),
            light_theme: None,
            dark_theme: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum MaxConnections {
    Mode(ConnectionMode),
    Count(u8),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    Auto,
    Unlimited,
}

impl Default for MaxConnections {
    fn default() -> Self {
        Self::Count(1)
    }
}

impl MaxConnections {
    pub fn limit(self) -> usize {
        match self {
            Self::Mode(ConnectionMode::Auto) => 4,
            Self::Mode(ConnectionMode::Unlimited) => usize::MAX,
            Self::Count(count) if Self::valid_count(count) => usize::from(count),
            Self::Count(_) => 1,
        }
    }

    fn valid_count(count: u8) -> bool {
        matches!(count, 1 | 2 | 4 | 8 | 16 | 32)
    }

    fn normalize_count(count: &serde_json::Value) -> u8 {
        match count.as_u64() {
            Some(value @ (1 | 2 | 4 | 8 | 16 | 32)) => value as u8,
            Some(6) => 4,
            Some(10 | 12 | 14) => 8,
            Some(18 | 20 | 22 | 24 | 26 | 28 | 30) => 16,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadMode {
    #[default]
    Sequential,
    Concurrent,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    System,
    Light,
    Dark,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    revision: u64,
    #[serde(flatten)]
    output: OutputOptions,
    interval_seconds: f64,
    max_connections: MaxConnections,
    download_mode: DownloadMode,
    output_path: String,
    output_missing: bool,
    appearance: Appearance,
    theme: Option<String>,
    light_theme: Option<String>,
    dark_theme: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct SettingsPatch {
    quality: Option<u8>,
    interval_seconds: Option<f64>,
    pub(crate) max_connections: Option<MaxConnections>,
    download_mode: Option<DownloadMode>,
    folder_template: Option<String>,
    filename_template: Option<String>,
    lyrics_mode: Option<LyricsMode>,
    artwork: Option<ArtworkResolution>,
    tags: Option<BTreeMap<String, bool>>,
    output_path: Option<String>,
    appearance: Option<Appearance>,
    theme: Option<String>,
    limited_to_album_releases: Option<bool>,
    allow_fallback: Option<bool>,
    booklet_only: Option<bool>,
    create_playlist: Option<bool>,
    smart_discography: Option<bool>,
    download_cover_art: Option<bool>,
    embed_cover_art: Option<bool>,
    cover_filename: Option<String>,
    safe_mode: Option<bool>,
    // Accept retired settings from older clients without interpreting their values.
    #[serde(rename = "flacPath")]
    _legacy_flac_path: serde::de::IgnoredAny,
    #[serde(rename = "reencodeFlac")]
    _legacy_reencode_flac: serde::de::IgnoredAny,
}

impl Settings {
    pub fn dto(&self, root: &Path) -> SettingsDto {
        let output = self.destination(root);
        SettingsDto {
            revision: self.revision,
            output: self.output.clone(),
            interval_seconds: self.interval_seconds,
            max_connections: self.max_connections,
            download_mode: self.download_mode,
            output_path: output
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            output_missing: !output.as_ref().is_some_and(|p| p.is_dir()),
            appearance: self.appearance,
            theme: self.theme.clone(),
            light_theme: self.light_theme.clone(),
            dark_theme: self.dark_theme.clone(),
        }
    }

    fn select_theme(&mut self, theme: String) {
        self.theme = Some(theme);
        self.light_theme = None;
        self.dark_theme = None;
    }

    pub fn migrated_theme(
        &self,
        expected_revision: u64,
        legacy_dark: bool,
    ) -> Result<Option<Self>> {
        if self.theme.is_some() {
            return Ok(None);
        }
        if self.revision != expected_revision {
            return Err(AppError::new(
                "settings_conflict",
                "settings",
                "Settings changed. Reload and apply the edit again.",
                true,
            ));
        }
        let legacy = if legacy_dark {
            &self.dark_theme
        } else {
            &self.light_theme
        };
        let mut next = self.clone();
        next.select_theme(legacy.clone().unwrap_or_else(|| "Suonelo".into()));
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| AppError::invalid("settings", "Settings revision overflow."))?;
        Ok(Some(next))
    }

    pub fn destination(&self, root: &Path) -> Option<PathBuf> {
        self.output_location
            .as_ref()
            .map(|location| location.resolve(root))
            .or_else(music_folder)
    }

    pub fn apply(&self, root: &Path, patch: SettingsPatch) -> Result<Self> {
        let mut next = self.clone();
        if let Some(safe_mode) = patch.safe_mode {
            next.output.safe_mode = safe_mode;
        }
        if let Some(quality) = patch.quality {
            if !(1..=4).contains(&quality) {
                return Err(AppError::invalid(
                    "settings",
                    "Choose a quality tier from 1 through 4.",
                ));
            }
            next.output.quality = quality;
        }
        if let Some(interval) = patch.interval_seconds {
            if !interval.is_finite() || interval < 0.0 {
                return Err(AppError::invalid(
                    "settings",
                    "Enter a nonnegative start interval.",
                ));
            }
            next.interval_seconds = interval;
        }
        if let Some(connections) = patch.max_connections {
            if matches!(connections, MaxConnections::Count(n) if !MaxConnections::valid_count(n)) {
                return Err(AppError::invalid(
                    "settings",
                    "Choose Auto, 1, 2, 4, 8, 16, 32, or Unlimited.",
                ));
            }
            next.max_connections = connections;
        }
        if let Some(mode) = patch.download_mode {
            next.download_mode = mode;
        }
        if let Some(folder) = patch.folder_template {
            next.output.folder_template = folder;
        }
        if let Some(filename) = patch.filename_template {
            next.output.filename_template = filename;
        }
        media::preview_output(
            &next.output.folder_template,
            &next.output.filename_template,
            next.output.quality,
            next.output.safe_mode,
        )
        .map_err(|error| AppError::media(error, "settings"))?;
        if let Some(mode) = patch.lyrics_mode {
            next.output.lyrics_mode = mode;
        }
        if let Some(artwork) = patch.artwork {
            next.output.artwork = artwork;
        }
        if let Some(tags) = patch.tags {
            if tags.len() > 2048 || tags.keys().any(|id| id.len() > 256) {
                return Err(AppError::invalid(
                    "settings",
                    "The metadata selection is too large.",
                ));
            }
            next.output.tags.extend(tags);
        }
        if let Some(path) = patch.output_path {
            next.output_location = location(
                root,
                &path,
                Path::is_dir,
                "Choose an existing destination folder.",
            )?;
        }
        if let Some(appearance) = patch.appearance {
            next.appearance = appearance;
        }
        if let Some(theme) = patch.theme {
            next.select_theme(validate_theme(theme)?);
        }
        next.output.limited_to_album_releases = patch
            .limited_to_album_releases
            .unwrap_or(next.output.limited_to_album_releases);
        next.output.allow_fallback = patch.allow_fallback.unwrap_or(next.output.allow_fallback);
        next.output.booklet_only = patch.booklet_only.unwrap_or(next.output.booklet_only);
        next.output.create_playlist = patch.create_playlist.unwrap_or(next.output.create_playlist);
        next.output.smart_discography = patch
            .smart_discography
            .unwrap_or(next.output.smart_discography);
        next.output.download_cover_art = patch
            .download_cover_art
            .unwrap_or(next.output.download_cover_art);
        next.output.embed_cover_art = patch.embed_cover_art.unwrap_or(next.output.embed_cover_art);
        if let Some(filename) = patch.cover_filename {
            let filename = filename.trim();
            media::validate_cover_filename(filename)
                .map_err(|error| AppError::media(error, "settings"))?;
            next.output.cover_filename = if filename.is_empty() {
                "cover".into()
            } else {
                filename.into()
            };
        }
        next.revision += 1;
        Ok(next)
    }
}

fn validate_theme(theme: String) -> Result<String> {
    if [
        "Suonelo",
        "Vercel",
        "Spotify",
        "Material Design",
        "Claymorphism",
        "Violet",
    ]
    .contains(&theme.as_str())
    {
        Ok(theme)
    } else {
        Err(AppError::invalid(
            "settings",
            "Choose a theme from the appearance panel.",
        ))
    }
}

fn location(
    root: &Path,
    text: &str,
    check: impl Fn(&Path) -> bool,
    message: &'static str,
) -> Result<Option<SavedLocation>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let path = Path::new(text.trim());
    if !check(path) {
        return Err(AppError::invalid("settings", message));
    }
    SavedLocation::new(path, root).map(Some)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkDto {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub album_artist: Option<String>,
    pub album_name: Option<String>,
    pub bit_depth: Option<u32>,
    pub sampling_rate: Option<f64>,
    pub reason: Option<String>,
    pub thumbnail: Option<String>,
    pub resource_kind: Option<ResourceKind>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Bookmark {
    pub key: ResourceKey,
    pub dto: BookmarkDto,
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
}

impl Bookmark {
    pub fn new(id: u64, link: CanonicalResource, reason: String) -> Self {
        let resource_kind = Some(link.key.kind);
        Self {
            key: link.key,
            dto: BookmarkDto {
                id,
                title: link.original_url.clone(),
                url: link.original_url,
                album_artist: None,
                album_name: None,
                bit_depth: None,
                sampling_rate: None,
                reason: Some(reason),
                thumbnail: None,
                resource_kind,
            },
            fingerprint: None,
            release_id: None,
        }
    }

    pub fn update(&mut self, summary: BookmarkSummary, fingerprint: String) {
        if let Some(activity) = &summary.activity
            && self.key.kind == ResourceKind::Track
        {
            self.release_id = Some(activity.release.clone());
        }
        self.dto.title = summary.title;
        self.dto.album_artist = summary.album_artist;
        self.dto.album_name = summary.album_name;
        self.dto.bit_depth = summary.bit_depth;
        self.dto.sampling_rate = summary.sample_rate;
        self.dto.thumbnail = summary.thumbnail;
        self.dto.reason = None;
        self.fingerprint = Some(fingerprint);
    }
}

pub struct Storage {
    pub path: PathBuf,
}

pub const SCHEMA_VERSION: u32 = 4;

impl Storage {
    pub fn initialize(path: PathBuf) -> Result<(Self, Settings, BTreeMap<u64, Bookmark>, u64)> {
        let storage = Self { path };
        let mut db = storage.open()?;
        db.execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(AppError::database)?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(AppError::database)?;
        if version > SCHEMA_VERSION {
            return Err(AppError::invalid(
                "storage",
                "This data folder needs a newer version of suonelo.",
            ));
        }
        if version < SCHEMA_VERSION {
            let tx = db.transaction().map_err(AppError::database)?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS bookmarks (id INTEGER PRIMARY KEY, resource TEXT UNIQUE NOT NULL, value TEXT NOT NULL);"
            ).map_err(AppError::database)?;
            tx.execute_batch(media::RECEIPT_SCHEMA)
                .map_err(AppError::database)?;
            media::migrate_replacement_receipts(&tx).map_err(AppError::database)?;
            crate::search::activity::migrate(&tx, version)
                .map_err(AppError::database)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(AppError::database)?;
            tx.commit().map_err(AppError::database)?;
        }
        crate::search::activity::load_clock(&db).map_err(AppError::database)?;
        let settings: Option<String> = db
            .query_row("SELECT value FROM settings WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(AppError::database)?;
        let mut repaired = false;
        let normalized = settings
            .as_deref()
            .map(|text| -> Result<String> {
                let mut value: serde_json::Value = serde_json::from_str(text)
                    .map_err(|_| AppError::invalid("storage", "Saved settings are unreadable."))?;
                let object = value.as_object_mut().ok_or_else(|| {
                    AppError::invalid("storage", "Saved settings must be an object.")
                })?;
                if let Some(saved) = object.get_mut("maxConnections").filter(|v| v.is_number()) {
                    let normalized =
                        serde_json::Value::from(MaxConnections::normalize_count(saved));
                    if *saved != normalized {
                        *saved = normalized;
                        repaired = true;
                    }
                }
                if let Some(saved) = object.get("theme") {
                    if saved
                        .as_str()
                        .is_none_or(|theme| validate_theme(theme.to_owned()).is_err())
                    {
                        object.insert("theme".into(), serde_json::Value::String("Suonelo".into()));
                        repaired = true;
                    }
                    object.remove("lightTheme");
                    object.remove("darkTheme");
                } else {
                    for field in ["lightTheme", "darkTheme"] {
                        if object.get(field).is_some_and(|saved| {
                            saved
                                .as_str()
                                .is_none_or(|theme| validate_theme(theme.to_owned()).is_err())
                        }) {
                            object.remove(field);
                        }
                    }
                }
                serde_json::to_string(&value).map_err(|_| {
                    AppError::invalid("storage", "Saved settings could not be normalized.")
                })
            })
            .transpose()?;
        let mut settings: Settings = normalized
            .as_deref()
            .map(decode)
            .transpose()?
            .unwrap_or_default();
        if repaired && let Some(normalized) = normalized.as_deref() {
            settings.revision = settings
                .revision
                .checked_add(1)
                .ok_or_else(|| AppError::invalid("settings", "Settings revision overflow."))?;
            // Keep unrelated fields from the saved profile when repairing one value.
            let mut persisted: serde_json::Value = decode(normalized)?;
            persisted["revision"] = serde_json::Value::from(settings.revision);
            db.execute(
                "UPDATE settings SET value=?1 WHERE id=1",
                [encode(&persisted)?],
            )
            .map_err(AppError::database)?;
        }
        let mut stmt = db
            .prepare("SELECT value FROM bookmarks ORDER BY id")
            .map_err(AppError::database)?;
        let mut bookmarks = BTreeMap::new();
        for row in stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(AppError::database)?
        {
            let bookmark: Bookmark = decode(&row.map_err(AppError::database)?)?;
            bookmarks.insert(bookmark.dto.id, bookmark);
        }
        let next_id = bookmarks.keys().next_back().map_or(1, |id| id + 1);
        Ok((storage, settings, bookmarks, next_id))
    }

    pub(crate) fn open(&self) -> Result<Connection> {
        let db = Connection::open(&self.path).map_err(AppError::database)?;
        db.busy_timeout(Duration::from_secs(5))
            .map_err(AppError::database)?;
        Ok(db)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        self.open()?.execute(
            "INSERT INTO settings (id,value) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
            [encode(settings)?],
        ).map_err(AppError::database)?;
        Ok(())
    }

    pub fn save_bookmarks<'a>(
        &self,
        bookmarks: impl IntoIterator<Item = &'a Bookmark>,
    ) -> Result<()> {
        let mut db = self.open()?;
        let tx = db.transaction().map_err(AppError::database)?;
        Self::write_bookmarks(&tx, bookmarks)?;
        tx.commit().map_err(AppError::database)
    }

    fn write_bookmarks<'a>(tx: &Connection, bookmarks: impl IntoIterator<Item = &'a Bookmark>) -> Result<()> {
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bookmarks (id,resource,value) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET value=excluded.value"
            ).map_err(AppError::database)?;
            for bookmark in bookmarks {
                let id = i64::try_from(bookmark.dto.id).map_err(|_| {
                    AppError::invalid("bookmarks", "The bookmark ID is out of range.")
                })?;
                stmt.execute(params![id, encode(&bookmark.key)?, encode(bookmark)?])
                    .map_err(AppError::database)?;
            }
        }
        Ok(())
    }

    pub fn save_search_bookmarks(
        &self,
        bookmarks: &[Bookmark],
        removals: &[u64],
        credits: &[crate::search::activity::Credit],
    ) -> Result<()> {
        self.save_bookmarks_with_activity(bookmarks, removals, credits, crate::search::activity::event_time())
    }

    pub fn save_bookmarks_with_activity(
        &self,
        bookmarks: &[Bookmark],
        removals: &[u64],
        credits: &[crate::search::activity::Credit],
        at: i64,
    ) -> Result<()> {
        let mut db = self.open()?;
        let tx = db.transaction().map_err(AppError::database)?;
        Self::write_bookmarks(&tx, bookmarks)?;
        for id in removals.iter().filter_map(|id| i64::try_from(*id).ok()) {
            tx.execute("DELETE FROM bookmarks WHERE id=?1", [id]).map_err(AppError::database)?;
        }
        for credit in credits {
            crate::search::activity::record(&tx, credit, 1, at)?;
        }
        tx.commit().map_err(AppError::database)
    }

    pub fn remove_bookmarks(&self, ids: &[u64]) -> Result<()> {
        let mut db = self.open()?;
        let tx = db.transaction().map_err(AppError::database)?;
        {
            let mut stmt = tx
                .prepare("DELETE FROM bookmarks WHERE id=?1")
                .map_err(AppError::database)?;
            for id in ids.iter().filter_map(|id| i64::try_from(*id).ok()) {
                stmt.execute([id]).map_err(AppError::database)?;
            }
        }
        tx.commit().map_err(AppError::database)
    }
}

fn encode(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|_| AppError::invalid("storage", "The stored record could not be encoded."))
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|_| {
        AppError::invalid(
            "storage",
            "The portable data folder contains an unreadable record.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qobuz::normalize_link;

    #[test]
    fn upgrades_the_original_database_without_losing_receipts() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("qbdlx.sqlite3");
        let (storage, _, _, _) = Storage::initialize(path.clone()).unwrap();
        let db = storage.open().unwrap();
        db.execute("INSERT INTO media_receipts VALUES ('portable:track.flac','audio','42','options','digest')", []).unwrap();
        db.execute_batch("DROP TABLE media_pending_updates; PRAGMA user_version=1;")
            .unwrap();
        drop(db);
        let (storage, _, _, _) = Storage::initialize(path).unwrap();
        let db = storage.open().unwrap();
        let digest: String = db
            .query_row("SELECT sha256 FROM media_receipts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(digest, "digest");
        let count: i64 = db
            .query_row("SELECT count(*) FROM media_pending_updates", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn legacy_preferences_pick_up_new_defaults_and_keep_explicit_tags() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "revision": 7, "quality": 3, "tags": {"replaygain": true}, "intervalSeconds": 2.0
        }))
        .unwrap();
        assert_eq!(settings.revision, 7);
        assert_eq!(settings.output.quality, 3);
        assert_eq!(settings.output.tags.get("replaygain"), Some(&true));
        assert_eq!(settings.max_connections.limit(), 1);
        assert_eq!(settings.download_mode, DownloadMode::Sequential);
        assert_eq!(settings.output.lyrics_mode, LyricsMode::External);
        assert!(settings.output.download_cover_art);
        assert!(settings.output.allow_fallback);
        assert!(!settings.output.booklet_only);
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "maxConnections": "unlimited", "intervalSeconds": 10000.0, "coverFilename": "   "
        }))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let updated = settings.apply(directory.path(), patch).unwrap();
        assert_eq!(updated.max_connections.limit(), usize::MAX);
        assert_eq!(updated.interval_seconds, 10000.0);
        assert_eq!(updated.output.cover_filename, "cover");
    }

    #[test]
    fn legacy_flac_preferences_are_ignored_without_losing_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("qbdlx.sqlite3");
        let (storage, _, _, _) = Storage::initialize(path.clone()).unwrap();
        let mut expected = serde_json::to_value(Settings::default()).unwrap();
        expected["revision"] = serde_json::json!(12);
        expected["quality"] = serde_json::json!(4);
        expected["outputLocation"] = serde_json::json!({ "Portable": "Library" });
        expected["maxConnections"] = serde_json::json!("unlimited");
        expected["theme"] = serde_json::json!("Violet");
        expected["tags"] = serde_json::json!({ "replaygain": true });
        let mut legacy = expected.clone();
        legacy["flacLocation"] = serde_json::json!({ "Portable": "tools/flac.exe" });
        legacy["reencodeFlac"] = serde_json::json!(true);
        storage
            .open()
            .unwrap()
            .execute(
                "INSERT INTO settings (id, value) VALUES (1, ?1)",
                [legacy.to_string()],
            )
            .unwrap();

        let (storage, settings, _, _) = Storage::initialize(path.clone()).unwrap();
        assert_eq!(serde_json::to_value(&settings).unwrap(), expected);
        assert_eq!(
            settings.destination(directory.path()),
            Some(directory.path().join("Library"))
        );
        let dto = serde_json::to_value(settings.dto(directory.path())).unwrap();
        for removed in ["flacLocation", "flacPath", "reencodeFlac"] {
            assert!(dto.get(removed).is_none());
        }

        let patch = serde_json::from_value(serde_json::json!({
            "intervalSeconds": 9.0,
            "flacPath": "missing/flac.exe",
            "reencodeFlac": true,
        }))
        .unwrap();
        let updated = settings.apply(directory.path(), patch).unwrap();
        storage.save_settings(&updated).unwrap();
        expected["intervalSeconds"] = serde_json::json!(9.0);
        expected["revision"] = serde_json::json!(13);
        let (_, reloaded, _, _) = Storage::initialize(path).unwrap();
        assert_eq!(serde_json::to_value(reloaded).unwrap(), expected);
    }

    #[test]
    fn naming_defaults_and_saved_templates_keep_their_spacing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let database = root.join("qbdlx.sqlite3");
        let (storage, settings, _, _) = Storage::initialize(database.clone()).unwrap();
        assert_eq!(
            settings.output.folder_template,
            "%album artist%/[(%year%)] %album name% [[Q%target quality%]][[%bit depth%-Bit %sampling rate%kHz]]"
        );
        assert_eq!(
            settings.output.filename_template,
            "%track number%. %artist% - %track title%"
        );
        assert!(settings.output.safe_mode);

        // An existing template retains its separator placement on load and unrelated edits.
        let existing_folder = "%album artist%/%album name% [(%year%)]";
        let existing = Settings {
            output: OutputOptions {
                folder_template: existing_folder.into(),
                ..settings.output
            },
            ..settings
        };
        storage.save_settings(&existing).unwrap();
        let (_, loaded, _, _) = Storage::initialize(database.clone()).unwrap();
        let updated = loaded
            .apply(
                root,
                SettingsPatch {
                    quality: Some(2),
                    safe_mode: Some(false),
                    ..SettingsPatch::default()
                },
            )
            .unwrap();
        assert_eq!(updated.output.folder_template, existing_folder);

        let folder = "%artist%     %album name%";
        let filename = "%track number%     %track title%";
        let spaced = updated
            .apply(
                root,
                SettingsPatch {
                    folder_template: Some(folder.into()),
                    filename_template: Some(filename.into()),
                    ..SettingsPatch::default()
                },
            )
            .unwrap();
        storage.save_settings(&spaced).unwrap();
        let (_, restored, _, _) = Storage::initialize(database).unwrap();
        assert_eq!(restored.output.folder_template, folder);
        assert_eq!(restored.output.filename_template, filename);
        assert!(!restored.output.safe_mode);
        assert_eq!(
            media::preview_output(
                &restored.output.folder_template,
                &restored.output.filename_template,
                2,
                restored.output.safe_mode,
            )
            .unwrap(),
            PathBuf::from("Aster Vale     Music for the Moon").join("03     Quiet orbit.flac")
        );
    }

    #[test]
    fn restores_settings_and_cached_bookmarks_from_sqlite() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let database = root.join("qbdlx.sqlite3");
        let (storage, settings, _, _) = Storage::initialize(database.clone()).unwrap();
        let next = settings
            .apply(
                root,
                SettingsPatch {
                    quality: Some(1),
                    interval_seconds: Some(4.0),
                    output_path: Some(root.to_string_lossy().into_owned()),
                    ..SettingsPatch::default()
                },
            )
            .unwrap();
        storage.save_settings(&next).unwrap();
        let mut bookmark = Bookmark::new(
            1,
            normalize_link("https://open.qobuz.com/album/0886443927087").unwrap(),
            "Pending".into(),
        );
        bookmark.update(
            BookmarkSummary {
                activity: None,
                title: "Cached release".into(),
                album_artist: Some("Cached artist".into()),
                album_name: Some("Cached release".into()),
                bit_depth: Some(24),
                sample_rate: Some(96.0),
                thumbnail_url: None,
                thumbnail: None,
            },
            "account-fingerprint".into(),
        );
        storage.save_bookmarks([&bookmark]).unwrap();

        let (_, settings, bookmarks, next_id) = Storage::initialize(database).unwrap();
        assert_eq!(settings.output.quality, 1);
        assert_eq!(settings.interval_seconds, 4.0);
        assert!(
            matches!(settings.output_location, Some(SavedLocation::Portable(ref path)) if path.as_os_str().is_empty())
        );
        assert_eq!(bookmarks[&1].dto.title, "Cached release");
        assert_eq!(bookmarks[&1].dto.sampling_rate, Some(96.0));
        assert_eq!(
            bookmarks[&1].fingerprint.as_deref(),
            Some("account-fingerprint")
        );
        assert_eq!(next_id, 2);
    }
}

#[cfg(test)]
mod suonelo_theme_tests {
    use super::*;

    fn save_fixture(store: &Storage, fields: serde_json::Value) {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let settings = value.as_object_mut().unwrap();
        settings.remove("theme");
        settings.extend(fields.as_object().unwrap().clone());
        store.open().unwrap().execute(
            "INSERT INTO settings(id,value) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
            [value.to_string()],
        ).unwrap();
    }

    #[test]
    fn connection_repairs_persist_once_and_keep_explicit_preferences() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("suonelo.sqlite3");
        let (store, fresh, _, _) = Storage::initialize(path.clone()).unwrap();
        assert_eq!(fresh.max_connections, MaxConnections::Count(1));
        assert_eq!(fresh.download_mode, DownloadMode::Sequential);
        assert_eq!(fresh.output.lyrics_mode, LyricsMode::External);
        for (input, expected) in [(6, 4), (14, 8), (30, 16), (-3, 1), (1000, 1), (32, 32)] {
            save_fixture(
                &store,
                serde_json::json!({
                    "revision": 9, "maxConnections": input, "downloadMode": "concurrent", "lyricsMode": "off",
                    "tags": {"qobuzAlbumId": false, "qobuzTrackId": true}, "quality": 2,
                }),
            );
            for _ in 0..2 {
                let (_, saved, _, _) = Storage::initialize(path.clone()).unwrap();
                assert_eq!(
                    saved.revision,
                    if input == i32::from(expected) { 9 } else { 10 }
                );
                assert_eq!(saved.max_connections, MaxConnections::Count(expected));
                assert_eq!(saved.download_mode, DownloadMode::Concurrent);
                assert_eq!(saved.output.lyrics_mode, LyricsMode::Off);
                assert_eq!(saved.output.tags["qobuzAlbumId"], false);
                assert_eq!(saved.output.tags["qobuzTrackId"], true);
                assert_eq!(saved.output.quality, 2);
            }
        }
        let patch = serde_json::from_value(serde_json::json!({"maxConnections": 6})).unwrap();
        assert!(fresh.apply(folder.path(), patch).is_err());
    }
    #[test]
    fn shared_theme_wins_and_invalid_shared_values_repair_once() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("suonelo.sqlite3");
        let (store, fresh, _, _) = Storage::initialize(path.clone()).unwrap();
        assert_eq!(fresh.theme.as_deref(), Some("Suonelo"));
        assert!(matches!(fresh.appearance, Appearance::System));
        for (theme, expected, revision) in [
            (serde_json::json!("Vercel"), "Vercel", 9),
            (serde_json::json!("unknown"), "Suonelo", 10),
            (serde_json::Value::Null, "Suonelo", 10),
            (serde_json::json!(42), "Suonelo", 10),
        ] {
            save_fixture(
                &store,
                serde_json::json!({
                    "theme":theme,"lightTheme":"Spotify","darkTheme":"Violet","revision":9,"maxConnections":1,
                }),
            );
            let (_, loaded, _, _) = Storage::initialize(path.clone()).unwrap();
            assert_eq!(loaded.theme.as_deref(), Some(expected));
            assert_eq!(loaded.revision, revision);
            assert_eq!(loaded.max_connections.limit(), 1);
            assert!(loaded.migrated_theme(8, true).unwrap().is_none());
            let (_, again, _, _) = Storage::initialize(path.clone()).unwrap();
            assert_eq!(again.revision, revision);
        }
    }

    #[test]
    fn legacy_theme_conversion_is_revision_checked_and_preserves_other_settings() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("suonelo.sqlite3");
        let (store, ..) = Storage::initialize(path.clone()).unwrap();
        for (appearance, dark, light, legacy_dark, expected) in [
            (
                "light",
                false,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Spotify",
            ),
            (
                "dark",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Violet",
            ),
            (
                "system",
                false,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Spotify",
            ),
            (
                "system",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!("Violet"),
                "Violet",
            ),
            (
                "system",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!("Spotify"),
                "Spotify",
            ),
            (
                "system",
                true,
                serde_json::json!("Spotify"),
                serde_json::json!(42),
                "Suonelo",
            ),
        ] {
            save_fixture(
                &store,
                serde_json::json!({
                    "appearance":appearance,"lightTheme":light,"darkTheme":legacy_dark,"revision":9,
                    "folderTemplate":"%artist%     %album name%","maxConnections":1,
                }),
            );
            let (_, loaded, _, _) = Storage::initialize(path.clone()).unwrap();
            assert!(loaded.theme.is_none());
            assert_eq!(
                loaded.migrated_theme(8, dark).err().unwrap().code,
                "settings_conflict"
            );
            let next = loaded.migrated_theme(9, dark).unwrap().unwrap();
            assert_eq!(next.theme.as_deref(), Some(expected));
            assert_eq!(serde_json::to_value(next.appearance).unwrap(), appearance);
            assert_eq!(next.output.folder_template, "%artist%     %album name%");
            assert_eq!(next.max_connections.limit(), 1);
            store.save_settings(&next).unwrap();
            let (_, restarted, _, _) = Storage::initialize(path.clone()).unwrap();
            assert_eq!(restarted.theme.as_deref(), Some(expected));
            assert_eq!(restarted.revision, 10);
            assert!(restarted.migrated_theme(9, !dark).unwrap().is_none());
        }

        save_fixture(
            &store,
            serde_json::json!({"appearance":"system","revision":9,"lightTheme":"Violet"}),
        );
        let (_, loaded, _, _) = Storage::initialize(path.clone()).unwrap();
        let next = loaded.migrated_theme(9, true).unwrap().unwrap();
        assert_eq!(next.theme.as_deref(), Some("Suonelo"));
        store.open().unwrap().execute_batch(
            "CREATE TRIGGER reject_theme BEFORE UPDATE ON settings BEGIN SELECT raise(ABORT, 'test failure'); END;"
        ).unwrap();
        assert!(store.save_settings(&next).is_err());
        let (_, after_failure, _, _) = Storage::initialize(path.clone()).unwrap();
        assert!(after_failure.theme.is_none());
        store
            .open()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_theme")
            .unwrap();
        let selected = after_failure
            .apply(
                folder.path(),
                SettingsPatch {
                    theme: Some("Spotify".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store.save_settings(&selected).unwrap();
        assert!(selected.migrated_theme(9, true).unwrap().is_none());
        let (_, saved, _, _) = Storage::initialize(path).unwrap();
        assert_eq!(saved.theme.as_deref(), Some("Spotify"));
    }
}
