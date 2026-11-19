use super::{
    Credentials, Params, QobuzClient, QobuzError, f64_value, nonempty, safe_reason, u32_value,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use url::Url;

const PAGE_SIZE: usize = 200;
const CATALOG_LIMIT: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Album,
    Track,
    Playlist,
    Artist,
    Label,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Track => "track",
            Self::Playlist => "playlist",
            Self::Artist => "artist",
            Self::Label => "label",
        }
    }

    fn endpoint(self) -> (&'static str, &'static str) {
        match self {
            Self::Album => ("album/get", "album_id"),
            Self::Track => ("track/get", "track_id"),
            Self::Playlist => ("playlist/get", "playlist_id"),
            Self::Artist => ("artist/get", "artist_id"),
            Self::Label => ("label/get", "label_id"),
        }
    }

    fn from_path(value: &str) -> Option<Self> {
        match value {
            "album" => Some(Self::Album),
            "track" => Some(Self::Track),
            "playlist" | "playlists" => Some(Self::Playlist),
            "artist" | "interpreter" => Some(Self::Artist),
            "label" => Some(Self::Label),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ResourceKey {
    pub kind: ResourceKind,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResource {
    pub key: ResourceKey,
    pub original_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BookmarkSummary {
    pub title: String,
    pub album_artist: Option<String>,
    pub album_name: Option<String>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f64>,
    #[serde(skip)]
    pub thumbnail_url: Option<String>,
    pub thumbnail: Option<String>,
    #[serde(skip)]
    pub activity: Option<crate::search::activity::Credit>,
}

#[derive(Clone)]
pub struct ResolvedTrack {
    pub id: String,
    pub title: String,
    pub album_id: String,
    pub disc: u32,
    pub number: u32,
    pub track: Value,
    pub album: Arc<Value>,
    pub unavailable: Option<QobuzError>,
}

#[derive(Clone)]
pub struct ResolvedResource {
    pub title: String,
    pub tracks: Vec<ResolvedTrack>,
    pub thumbnail_url: Option<String>,
    pub thumbnail: Option<String>,
}

pub fn normalize_link(raw: &str) -> Result<CanonicalResource, QobuzError> {
    let original = raw.trim();
    let invalid = || {
        QobuzError::new(
            "invalid_url",
            "Enter an HTTPS Qobuz album, track, playlist, artist, or label link.",
            false,
        )
    };
    if original.len() > 4096
        || original
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid());
    }
    let url = Url::parse(original).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.host_str(),
            Some("qobuz.com" | "www.qobuz.com" | "play.qobuz.com" | "open.qobuz.com")
        )
    {
        return Err(invalid());
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or_else(invalid)?
        .filter(|part| !part.is_empty())
        .collect();
    let (index, kind) = segments
        .iter()
        .take(2)
        .enumerate()
        .find_map(|(index, segment)| ResourceKind::from_path(segment).map(|kind| (index, kind)))
        .ok_or_else(invalid)?;
    if !(1..=2).contains(&(segments.len() - index - 1)) {
        return Err(invalid());
    }
    let id = segments
        .last()
        .copied()
        .filter(|id| valid_id(id))
        .ok_or_else(invalid)?;
    Ok(CanonicalResource {
        key: ResourceKey {
            kind,
            id: id.to_owned(),
        },
        original_url: original.to_owned(),
    })
}

impl QobuzClient {
    pub async fn resolve_bookmark_summary(
        &self,
        key: &ResourceKey,
        credentials: &Credentials,
    ) -> Result<BookmarkSummary, QobuzError> {
        let (endpoint, param) = key.kind.endpoint();
        let params = vec![(param, key.id.clone()), ("limit", "1".to_owned())];
        let value = self.metadata(endpoint, params, credentials).await?;
        if let Some(error) = unavailable_for_account(&value, credentials.access_mode.allows_preview_selection()) {
            return Err(error);
        }
        let album = match key.kind {
            ResourceKind::Album => Some(&value),
            ResourceKind::Track if value["album"].is_object() => Some(&value["album"]),
            _ => None,
        };
        let title = if let Some(album) = album {
            nonempty(&album["title"])
                .map(str::to_owned)
                .unwrap_or_else(|| resource_title(key, &value))
        } else {
            resource_title(key, &value)
        };
        Ok(BookmarkSummary {
            title,
            album_artist: album
                .and_then(|album| nonempty(&album["artist"]["name"]))
                .map(str::to_owned),
            album_name: album
                .and_then(|album| nonempty(&album["title"]))
                .map(str::to_owned),
            bit_depth: album
                .and_then(|album| u32_value(&album["maximum_bit_depth"]))
                .filter(|depth| *depth > 0),
            sample_rate: album.and_then(|album| f64_value(&album["maximum_sampling_rate"])),
            thumbnail_url: match key.kind {
                ResourceKind::Album | ResourceKind::Track => album.and_then(thumbnail_source),
                ResourceKind::Artist => thumbnail_source(&value),
                _ => None,
            },
            thumbnail: None,
            activity: match key.kind {
                ResourceKind::Album | ResourceKind::Track => {
                    let kind = if key.kind == ResourceKind::Album {
                        super::search::Kind::Album
                    } else {
                        super::search::Kind::Track
                    };
                    super::search::row(kind, &value, None)
                        .and_then(|row| crate::search::activity::Credit::from_row(&row))
                }
                _ => None,
            },
        })
    }

    pub async fn resolve_resource_for_job(
        &self,
        key: &ResourceKey,
        credentials: &Credentials,
        limited_to_album_releases: bool,
        smart_discography: bool,
    ) -> Result<ResolvedResource, QobuzError> {
        let mut portrait = None;
        let (title, tracks) = match key.kind {
            ResourceKind::Album => {
                let (album, entries) = self.fetch_album(&key.id, credentials).await?;
                if let Some(error) = unavailable_for_account(&album, credentials.access_mode.allows_preview_selection()) {
                    return Err(error);
                }
                if limited_to_album_releases && !is_album_release(&album) {
                    return Err(filtered_releases());
                }
                let title = resource_title(key, &album);
                let tracks = entries
                    .into_iter()
                    .map(|entry| resolved_track(entry, album.clone(), None, credentials.access_mode.allows_preview_selection()))
                    .collect();
                (title, tracks)
            }
            ResourceKind::Track => {
                let track = self
                    .metadata("track/get", vec![("track_id", key.id.clone())], credentials)
                    .await?;
                if let Some(error) = unavailable_for_account(&track, credentials.access_mode.allows_preview_selection()) {
                    return Err(error);
                }
                let album_id = identity(&track["album"]["id"]).ok_or_else(|| {
                    QobuzError::invalid_response("Qobuz did not identify the album for this track.")
                })?;
                let (album, _) = self.fetch_album(&album_id, credentials).await?;
                if let Some(error) = unavailable_for_account(&album, credentials.access_mode.allows_preview_selection()) {
                    return Err(error);
                }
                let title = resource_title(key, &track);
                (title, vec![resolved_track(track, album, None, credentials.access_mode.allows_preview_selection())])
            }
            ResourceKind::Playlist => {
                let (playlist, entries) = self.paginated(key, "tracks", true, credentials).await?;
                let mut albums = HashMap::new();
                let mut tracks = Vec::with_capacity(entries.len());
                for track in entries {
                    // Playlist rows already carry the track identity and album summary.
                    // Fetch each full album once for artwork, booklets and disc information.
                    let (album, failure) = match identity(&track["album"]["id"]) {
                        Some(id) => {
                            let album = match albums.entry(id) {
                                std::collections::hash_map::Entry::Occupied(entry) => {
                                    entry.into_mut()
                                }
                                std::collections::hash_map::Entry::Vacant(entry) => {
                                    let album =
                                        match self.fetch_album(entry.key(), credentials).await {
                                            Ok((album, _)) => Ok(album),
                                            Err(error) if error.code == "resource_unavailable" => {
                                                Err(error)
                                            }
                                            Err(error) => return Err(error),
                                        };
                                    entry.insert(album)
                                }
                            };
                            match album {
                                Ok(album) => (album.clone(), unavailable_for_account(album, credentials.access_mode.allows_preview_selection())),
                                Err(error) => {
                                    (Arc::new(track["album"].clone()), Some(error.clone()))
                                }
                            }
                        }
                        None => (
                            Arc::new(track["album"].clone()),
                            Some(QobuzError::new(
                                "resource_unavailable",
                                "This playlist entry has no available album.",
                                false,
                            )),
                        ),
                    };
                    tracks.push(resolved_track(track, album, failure, credentials.access_mode.allows_preview_selection()));
                }
                (resource_title(key, &playlist), tracks)
            }
            ResourceKind::Artist | ResourceKind::Label => {
                let (resource, mut entries) =
                    self.paginated(key, "albums", true, credentials).await?;
                if key.kind == ResourceKind::Artist {
                    portrait = thumbnail_source(&resource);
                }
                if limited_to_album_releases {
                    entries.retain(|entry| {
                        (entry["release_type"].is_null() || entry["release_type"] == "album")
                            && entry["artist"]["name"] != "Various Artists"
                    });
                }
                if key.kind == ResourceKind::Artist && smart_discography {
                    let artist = resource.clone();
                    entries =
                        tokio::task::spawn_blocking(move || best_artist_editions(&artist, entries))
                            .await
                            .map_err(|_| {
                                QobuzError::invalid_response(
                                    "Could not compare the artist's album editions.",
                                )
                            })?;
                }
                let mut tracks = Vec::new();
                let mut seen = HashSet::new();
                let mut unavailable_release = None;
                let mut filtered = limited_to_album_releases && entries.is_empty();
                for entry in entries {
                    let Some(album_id) = identity(&entry["id"]) else {
                        continue;
                    };
                    if !seen.insert(album_id.clone()) {
                        continue;
                    }
                    match self.fetch_album(&album_id, credentials).await {
                        Ok((album, entries)) => {
                            if limited_to_album_releases && !is_album_release(&album) {
                                filtered = true;
                                continue;
                            }
                            let failure = unavailable_for_account(&album, credentials.access_mode.allows_preview_selection());
                            tracks.extend(entries.into_iter().map(|entry| {
                                resolved_track(entry, album.clone(), failure.clone(), credentials.access_mode.allows_preview_selection())
                            }));
                        }
                        Err(error) if error.code == "resource_unavailable" => {
                            unavailable_release = Some(error)
                        }
                        Err(error) => return Err(error),
                    }
                }
                if tracks.is_empty()
                    && let Some(error) = unavailable_release
                {
                    return Err(error);
                }
                if tracks.is_empty() && filtered {
                    return Err(filtered_releases());
                }
                (resource_title(key, &resource), tracks)
            }
        };
        if tracks.is_empty() {
            return Err(QobuzError::new(
                "resource_unavailable",
                "Qobuz returned no audio tracks for this resource.",
                false,
            ));
        }
        let thumbnail_url = match key.kind {
            ResourceKind::Album | ResourceKind::Track => tracks
                .first()
                .and_then(|track| thumbnail_source(&track.album)),
            ResourceKind::Artist => portrait,
            _ => None,
        };
        Ok(ResolvedResource {
            title,
            tracks,
            thumbnail_url,
            thumbnail: None,
        })
    }

    async fn fetch_album(
        &self,
        album_id: &str,
        credentials: &Credentials,
    ) -> Result<(Arc<Value>, Vec<Value>), QobuzError> {
        let key = ResourceKey {
            kind: ResourceKind::Album,
            id: album_id.to_owned(),
        };
        let (mut album, tracks) = self.paginated(&key, "tracks", false, credentials).await?;
        if let Some(metadata) = album.as_object_mut() {
            let max_disc = tracks
                .iter()
                .filter_map(|track| u32_value(&track["media_number"]))
                .max()
                .unwrap_or(1);
            let media_count = metadata.get("media_count").and_then(u32_value).unwrap_or(1);
            metadata.insert(
                "media_count".to_owned(),
                Value::from(media_count.max(max_disc)),
            );
            metadata
                .entry("id")
                .or_insert_with(|| Value::from(album_id));
        }
        Ok((Arc::new(album), tracks))
    }

    async fn paginated(
        &self,
        key: &ResourceKey,
        collection: &'static str,
        request_extra: bool,
        credentials: &Credentials,
    ) -> Result<(Value, Vec<Value>), QobuzError> {
        let (endpoint, param) = key.kind.endpoint();
        let mut resource = Value::Null;
        let mut entries = Vec::new();
        let mut expected = None;
        let mut last_album_page = Vec::new();
        loop {
            let offset = entries.len();
            let mut params: Params = vec![
                (param, key.id.clone()),
                ("offset", offset.to_string()),
                ("limit", PAGE_SIZE.to_string()),
            ];
            if request_extra {
                params.push(("extra", collection.to_owned()));
            }
            let mut page = self.metadata(endpoint, params, credentials).await?;
            if offset == 0
                && request_extra
                && let Some(error) = unavailable_for_account(&page, credentials.access_mode.allows_preview_selection())
            {
                return Err(error);
            }
            expected = page[collection]["total"]
                .as_u64()
                .or_else(|| page[format!("{collection}_count")].as_u64())
                .and_then(|count| usize::try_from(count).ok())
                .filter(|count| *count > 0)
                .or(expected);
            let items = std::mem::take(&mut page[collection])
                .as_object_mut()
                .and_then(|container| container.remove("items"))
                .and_then(|items| match items {
                    Value::Array(items) => Some(items),
                    _ => None,
                })
                .unwrap_or_default();
            let count = items.len();
            if offset == 0 {
                page.as_object_mut()
                    .ok_or_else(|| {
                        QobuzError::invalid_response("Qobuz returned invalid catalog metadata.")
                    })?
                    .remove(collection);
                resource = page;
            }
            if count == 0 {
                break;
            }
            if key.kind == ResourceKind::Album {
                let ids: Vec<_> = items.iter().map(|item| identity(&item["id"])).collect();
                if ids == last_album_page {
                    return Err(QobuzError::invalid_response(
                        "Qobuz returned the same album track page more than once.",
                    ));
                }
                last_album_page = ids;
            }
            entries.extend(items);
            if entries.len() > CATALOG_LIMIT || expected.is_some_and(|total| total > CATALOG_LIMIT)
            {
                return Err(QobuzError::new(
                    "catalog_limit",
                    "This catalog contains more than 100,000 entries. Submit individual albums instead.",
                    false,
                ));
            }
            if expected.is_some_and(|total| entries.len() >= total)
                || (expected.is_none() && count < PAGE_SIZE)
            {
                break;
            }
        }
        if expected.is_some_and(|total| entries.len() < total) && unavailable(&resource).is_none() {
            return Err(QobuzError::invalid_response(
                "Qobuz returned fewer catalog entries than it advertised.",
            ));
        }
        Ok((resource, entries))
    }
}

fn is_album_release(album: &Value) -> bool {
    album["release_type"] == "album" && album["artist"]["name"] != "Various Artists"
}

fn filtered_releases() -> QobuzError {
    QobuzError::new(
        "release_filtered",
        "No matching album releases. Check Limited to album releases and use Download again to rebuild the selection.",
        false,
    )
}

fn best_artist_editions(resource: &Value, entries: Vec<Value>) -> Vec<Value> {
    static EXTRAS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(re-?master(ed)?|anniversary|deluxe|live|collector|demo|expanded|remix|acoustic|instrumental|edition)\b").unwrap()
    });
    static BRACKETS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[\(\[][^()\[\]]*[\)\]]").unwrap());
    let primary_artist = nonempty(&resource["name"])
        .or_else(|| nonempty(&resource["artist"]["name"]))
        .or_else(|| {
            entries
                .first()
                .and_then(|entry| nonempty(&entry["artist"]["name"]))
        })
        .map(str::to_owned);
    let mut groups: Vec<(Vec<char>, Value)> = Vec::new();
    let mut exact = HashMap::new();
    for entry in entries {
        if primary_artist
            .as_deref()
            .zip(nonempty(&entry["artist"]["name"]))
            .is_some_and(|(primary, actual)| primary != actual)
        {
            continue;
        }
        let title = nonempty(&entry["title"]).unwrap_or_default();
        let title: Vec<char> = BRACKETS
            .replace_all(title, "")
            .trim()
            .to_lowercase()
            .chars()
            .collect();
        let group = if title.is_empty() {
            None
        } else {
            exact.get(&title).copied().or_else(|| {
                groups
                    .iter()
                    .position(|(base, _)| similar_titles(base, &title))
            })
        };
        if let Some(group) = group {
            exact.insert(title, group);
            let current = &mut groups[group].1;
            let ranking = |album: &Value| {
                (
                    u32_value(&album["maximum_bit_depth"]).unwrap_or_default(),
                    f64_value(&album["maximum_sampling_rate"]).unwrap_or_default(),
                    !EXTRAS.is_match(&format!(
                        "{} {}",
                        album["title"].as_str().unwrap_or(""),
                        album["version"].as_str().unwrap_or("")
                    )),
                    album["release_date_original"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                )
            };
            if ranking(&entry) > ranking(current) {
                *current = entry;
            }
        } else {
            if !title.is_empty() {
                exact.insert(title.clone(), groups.len());
            }
            groups.push((title, entry));
        }
    }
    groups.into_iter().map(|(_, album)| album).collect()
}

fn similar_titles(left: &[char], right: &[char]) -> bool {
    let total = left.len() + right.len();
    let required = (total * 9).div_ceil(20);
    if left.len().min(right.len()) < required || total > 1024 {
        return false;
    }

    // SequenceMatcher counts the longest contiguous match, then matches on either side.
    let mut regions = vec![(left, right)];
    let mut matches = 0;
    while let Some((a, b)) = regions.pop() {
        let mut row = vec![0; b.len() + 1];
        let (mut start_a, mut start_b, mut longest) = (0, 0, 0);
        for (i, character) in a.iter().enumerate() {
            let mut previous = 0;
            for (j, candidate) in b.iter().enumerate() {
                let count = if character == candidate {
                    previous + 1
                } else {
                    0
                };
                previous = std::mem::replace(&mut row[j + 1], count);
                if count > longest {
                    (start_a, start_b, longest) = (i + 1 - count, j + 1 - count, count);
                }
            }
        }
        if longest == 0 {
            continue;
        }
        matches += longest;
        if matches >= required {
            return true;
        }
        if start_a > 0 && start_b > 0 {
            regions.push((&a[..start_a], &b[..start_b]));
        }
        if start_a + longest < a.len() && start_b + longest < b.len() {
            regions.push((&a[start_a + longest..], &b[start_b + longest..]));
        }
    }
    false
}

#[cfg(test)]
#[test]
fn artist_selection_prioritizes_quality_then_standard_edition() {
    use serde_json::json;

    let edition = |id, title, depth, rate, date| {
        json!({
            "id": id, "title": title, "maximum_bit_depth": depth, "maximum_sampling_rate": rate,
            "release_date_original": date, "artist": {"name": "Test Artist"},
        })
    };
    let entries = vec![
        edition("01", "A Long Album Title", 16, 44.1, "2023-01-01"),
        edition("02", "A Long Album Title (Deluxe)", 24, 96.0, "2024-01-01"),
        edition("03", "A Long Album Titles", 24, 96.0, "2022-01-01"),
        edition(
            "04",
            "A Long Album Title (Remastered)",
            24,
            48.0,
            "2025-01-01",
        ),
        edition("05", "Other recording", 16, 44.1, "2025-01-01"),
    ];
    let selected = best_artist_editions(&json!({"name": "Test Artist"}), entries);
    assert_eq!(
        selected
            .iter()
            .map(|album| album["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["03", "05"]
    );
    assert!(is_album_release(&json!({"release_type": "album"})));
    assert!(!is_album_release(
        &json!({"release_type": "album", "artist": {"name": "Various Artists"}})
    ));
}

pub(super) fn thumbnail_source(value: &Value) -> Option<String> {
    let image = value.get("image").or_else(|| value.get("picture"))?;
    ["small", "thumbnail", "medium", "large"]
        .into_iter()
        .filter_map(|key| image.get(key))
        .chain(std::iter::once(image))
        .filter_map(Value::as_str)
        .find_map(|text| {
            let url = Url::parse(text).ok()?;
            (url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none())
            .then(|| url.to_string())
        })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) fn identity(value: &Value) -> Option<String> {
    if let Some(id) = value.as_u64() {
        return (id > 0).then(|| id.to_string());
    }
    nonempty(value)
        .filter(|id| *id != "0" && valid_id(id))
        .map(str::to_owned)
}

fn resource_title(key: &ResourceKey, value: &Value) -> String {
    nonempty(&value["title"])
        .or_else(|| nonempty(&value["name"]))
        .or_else(|| nonempty(&value[key.kind.as_str()]["name"]))
        .or_else(|| nonempty(&value["artist"]["name"]))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Qobuz {} {}", key.kind.as_str(), key.id))
}

fn unavailable(value: &Value) -> Option<QobuzError> {
    (value["streamable"].as_bool() == Some(false)).then(|| {
        let reason = ["streamable_reason", "availability_reason", "reason", "message"]
            .iter().find_map(|field| nonempty(&value[field]).and_then(|reason| safe_reason(reason, None)));
        let default = "Qobuz reports that this resource is unavailable for streaming under the current account.";
        let message = reason.map(|reason| format!("{default} Qobuz reported: {reason}"));
        QobuzError::new("resource_unavailable", message.as_deref().unwrap_or(default), false)
    })
}

fn unavailable_for_account(value: &Value, preview_only: bool) -> Option<QobuzError> {
    // Catalog availability describes full streams. A preview still requires a
    // signed MP3 offer for this track at transfer time.
    if preview_only { None } else { unavailable(value) }
}

fn resolved_track(
    mut track: Value,
    album: Arc<Value>,
    failure: Option<QobuzError>,
    preview_only: bool,
) -> ResolvedTrack {
    let id = identity(&track["id"]);
    let unavailable = failure.or_else(|| unavailable_for_account(&track, preview_only)).or_else(|| {
        id.is_none().then(|| {
            QobuzError::new(
                "resource_unavailable",
                "Qobuz did not identify this track.",
                false,
            )
        })
    });
    let mut title = nonempty(&track["title"])
        .unwrap_or("Unavailable track")
        .to_owned();
    if let Some(version) = nonempty(&track["version"]).filter(|version| !title.contains(version)) {
        title.push_str(&format!(" ({version})"));
    }
    let album_id = identity(&album["id"]).unwrap_or_default();
    let disc = u32_value(&track["media_number"])
        .filter(|disc| *disc > 0)
        .unwrap_or(1);
    let number = u32_value(&track["track_number"]).unwrap_or_default();
    if let Some(track) = track.as_object_mut() {
        track.remove("album");
    }
    ResolvedTrack {
        id: id.unwrap_or_default(),
        title,
        album_id,
        disc,
        number,
        track,
        album,
        unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deduplication_keys_ignore_storefront_and_tracking() {
        let storefront =
            normalize_link("https://www.qobuz.com/us-en/album/title/0093624949091?foo=bar#b")
                .unwrap();
        let direct = normalize_link("https://open.qobuz.com/album/0093624949091").unwrap();
        assert_eq!(storefront.key, direct.key);
        assert_ne!(storefront.original_url, direct.original_url);
        assert_eq!(
            normalize_link("https://qobuz.com/fr-fr/interpreter/name/12")
                .unwrap()
                .key
                .kind,
            ResourceKind::Artist
        );
        assert_ne!(
            normalize_link("https://qobuz.com/track/0093624949091")
                .unwrap()
                .key,
            direct.key
        );
        assert_eq!(
            normalize_link("https://www.qobuz.com/gb-en/playlists/top-uk/5624881")
                .unwrap()
                .key,
            normalize_link("https://play.qobuz.com/playlist/5624881")
                .unwrap()
                .key,
        );
    }

    #[test]
    fn rejects_invalid_or_unsupported_urls() {
        for url in [
            "http://qobuz.com/track/5",
            "https://qobuz.com.attacker.test/track/5",
            "https://qobuz.com/search/5",
            "https://qobuz.com/track/",
            "https://qobuz.com:444/track/5",
            "https://person@qobuz.com/track/5",
            "https://qobuz.com/track/folder/5/6",
        ] {
            assert!(normalize_link(url).is_err(), "{url}");
        }
    }

    #[test]
    fn playlist_occurrences_share_the_album_without_changing_track_identity() {
        let album = Arc::new(json!({"id": "album-id", "media_count": 2}));
        let track = json!({"id": 55, "title": "Track", "version": "Live", "media_number": 2, "track_number": 3});
        let first = resolved_track(track.clone(), album.clone(), None, false);
        let second = resolved_track(track, album, None, false);
        assert_eq!(
            (&first.id, first.disc, first.number),
            (&second.id, second.disc, second.number)
        );
        assert_eq!(first.title, "Track (Live)");
        assert!(Arc::ptr_eq(&first.album, &second.album));
    }
}
