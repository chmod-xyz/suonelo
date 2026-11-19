use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::output_store::{CheckCancelled, Receipt, Stage, safe_directory};
use super::{
    Media, MediaError, TransferProgress, blocking, cancellable, copy_stage, existing_root,
    template::sanitize,
};
use crate::qobuz::ResolvedTrack;
use crate::recovery::{RetryBudget, TransferContext};

const ART_LIMIT: u64 = 40 * 1024 * 1024;
const BOOKLET_LIMIT: u64 = 128 * 1024 * 1024;
const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

pub fn validate_cover_filename(input: &str) -> Result<(), MediaError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(());
    }
    let lower = input.to_lowercase();
    let stem = lower.split('.').next().unwrap_or_default().trim_end();
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
    let image_extension = lower.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension,
            "avif"
                | "bmp"
                | "gif"
                | "heic"
                | "heif"
                | "ico"
                | "jpg"
                | "jpeg"
                | "png"
                | "svg"
                | "tif"
                | "tiff"
                | "webp"
        )
    });
    if reserved
        || image_extension
        || input != sanitize(input, false)
        || input.chars().any(char::is_control)
    {
        return Err(MediaError::new(
            "cover_filename",
            "Use a valid filename without an image extension.",
            false,
        ));
    }
    Ok(())
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd,
)]
pub enum ArtworkResolution {
    #[serde(rename = "300")]
    Px300,
    #[serde(rename = "600")]
    Px600,
    #[serde(rename = "1200")]
    Px1200,
    #[default]
    #[serde(rename = "original")]
    Original,
}

impl ArtworkResolution {
    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Px1200 => "1200 px",
            Self::Px600 => "600 px",
            Self::Px300 => "300 px",
        }
    }

    fn px(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::Px1200 => Some(1200),
            Self::Px600 => Some(600),
            Self::Px300 => Some(300),
        }
    }
}

pub struct ArtworkOutput {
    pub path: PathBuf,
    pub resolution: ArtworkResolution,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub reused: bool,
}

/// Staged provider bytes shared by the tracks and sidecar of one release attempt.
#[derive(Default)]
pub struct ArtworkCache {
    staged: OnceCell<Result<StagedArtwork, MediaError>>,
}

impl ArtworkCache {
    async fn prepare(
        &self,
        load: impl AsyncFnOnce() -> Result<StagedArtwork, MediaError>,
    ) -> Result<&StagedArtwork, MediaError> {
        self.staged
            .get_or_try_init(|| async {
                match load().await {
                    Err(error) if matches!(error.code, "disk_full" | "cancelled") => Err(error),
                    settled => Ok(settled),
                }
            })
            .await?
            .as_ref()
            .map_err(Clone::clone)
    }
}

struct StagedArtwork {
    file: Stage,
    info: ImageInfo,
    resolution: ArtworkResolution,
}

#[derive(Eq, Hash, PartialEq)]
pub(super) struct CoverKey {
    attempt: (u64, u64),
    album: String,
    path: PathBuf,
    resolution: ArtworkResolution,
}

#[derive(Clone)]
pub struct BookletSource {
    pub id: String,
    pub name: String,
    url: Option<Url>,
}

pub fn resolve_booklets(album: &Value, safe_mode: bool) -> Vec<BookletSource> {
    let mut seen = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut unnamed = 0;
    album["goodies"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, entry)| {
            if entry["file_format_id"].as_u64() != Some(21)
                && entry["file_format_id"].as_str() != Some("21")
            {
                return None;
            }
            let url = secure_url(&entry["url"]);
            let key = url
                .as_ref()
                .map(|url| format!("{}{}", url.host_str().unwrap_or_default(), url.path()))
                .unwrap_or_else(|| format!("unavailable:{index}"));
            if !seen.insert(key.clone()) {
                return None;
            }
            let id = hex::encode(Sha256::digest(key.as_bytes()));
            let title = ["title", "name", "file_name"]
                .into_iter()
                .find_map(|key| entry[key].as_str().filter(|title| !title.trim().is_empty()));
            let title = title
                .map(|title| {
                    let title = title.to_lowercase();
                    sanitize(title.strip_suffix(".pdf").unwrap_or(&title), safe_mode)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join("_")
                })
                .filter(|title| !title.is_empty());
            let name = loop {
                let candidate = if let Some(title) = &title {
                    let count = counts.entry(title.clone()).or_default();
                    *count += 1;
                    if *count == 1 {
                        format!("{title}.pdf")
                    } else {
                        format!("{title}_{count:02}.pdf")
                    }
                } else {
                    unnamed += 1;
                    format!("booklet_{unnamed:02}.pdf")
                };
                if names.insert(candidate.clone()) {
                    break candidate;
                }
            };
            Some(BookletSource { id, name, url })
        })
        .collect()
}

fn secure_url(value: &Value) -> Option<Url> {
    let url = Url::parse(value.as_str()?).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn artwork_sources(album: &Value, ceiling: ArtworkResolution) -> Vec<(ArtworkResolution, Url)> {
    use ArtworkResolution::*;
    let image = &album["image"];
    let mut sources = std::collections::BTreeMap::new();
    for (key, size) in [
        ("original", Original),
        ("org", Original),
        ("maximum", Original),
        ("extralarge", Px1200),
        ("1200", Px1200),
        ("large", Px600),
        ("600", Px600),
        ("medium", Px300),
        ("300", Px300),
        ("small", Px300),
        ("thumbnail", Px300),
    ] {
        let Some(url) = secure_url(&image[key]) else {
            continue;
        };
        let path = url.path().to_ascii_lowercase();
        let resolution = if path.contains("_org.") {
            Original
        } else if path.contains("_1200.") {
            Px1200
        } else if path.contains("_600.") {
            Px600
        } else if path.contains("_300.") || path.contains("_230.") {
            Px300
        } else {
            size
        };
        sources.entry(resolution).or_insert_with(Vec::new).push(url);
    }
    let base = sources
        .iter()
        .rev()
        .flat_map(|(_, urls)| urls)
        .find(|url| {
            url.host_str()
                .is_some_and(|host| host == "qobuz.com" || host.ends_with(".qobuz.com"))
                && url
                    .path()
                    .rsplit_once('.')
                    .and_then(|(prefix, _)| prefix.rsplit_once('_'))
                    .is_some_and(|(_, size)| {
                        ["org", "1200", "600", "300", "230", "50"].contains(&size)
                    })
        })
        .cloned();
    if let Some(url) = base {
        for (size, suffix) in [
            (Original, "org"),
            (Px1200, "1200"),
            (Px600, "600"),
            (Px300, "300"),
        ] {
            let (path, extension) = url
                .path()
                .rsplit_once('.')
                .expect("validated Qobuz artwork path");
            let (prefix, _) = path
                .rsplit_once('_')
                .expect("validated Qobuz artwork suffix");
            let mut derived = url.clone();
            derived.set_path(&format!("{prefix}_{suffix}.{extension}"));
            sources.entry(size).or_default().push(derived);
        }
    }
    let mut seen = BTreeSet::new();
    sources
        .into_iter()
        .rev()
        .filter(|(size, _)| *size <= ceiling)
        .flat_map(|(size, urls)| urls.into_iter().map(move |url| (size, url)))
        .filter(|(_, url)| seen.insert(url.as_str().to_owned()))
        .collect()
}

fn artwork_error() -> MediaError {
    MediaError::new(
        "artwork_unavailable",
        "No usable cover was available up to the selected resolution. Retry cover artwork.",
        true,
    )
}

fn artwork_conflict() -> MediaError {
    MediaError::new(
        "artwork_conflict",
        "This folder contains a cover that could not be verified for this release and resolution.",
        false,
    )
}

#[derive(Clone, Copy)]
pub(super) struct ImageInfo {
    pub(super) extension: &'static str,
    pub(super) dimensions: Option<(u32, u32)>,
}

impl ImageInfo {
    fn output(self, path: PathBuf, resolution: ArtworkResolution, reused: bool) -> ArtworkOutput {
        ArtworkOutput {
            path,
            resolution,
            width: self.dimensions.map(|size| size.0),
            height: self.dimensions.map(|size| size.1),
            reused,
        }
    }
}

fn image_info(path: &Path) -> Result<ImageInfo, MediaError> {
    let data = fs::read(path).map_err(MediaError::disk)?;
    image_data_info(&data)
}

pub(super) fn image_data_info(data: &[u8]) -> Result<ImageInfo, MediaError> {
    if data.len() > ART_LIMIT as usize {
        return Err(artwork_error());
    }
    let (extension, dimensions) =
        if data.starts_with(&[0xff, 0xd8, 0xff]) && data.ends_with(&[0xff, 0xd9]) {
            ("jpg", jpeg_dimensions(&data))
        } else if data.starts_with(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR")
            && data.len() > 33
            && data[data.len() - 8..data.len() - 4] == *b"IEND"
        {
            let width = u32::from_be_bytes(data[16..20].try_into().expect("PNG width"));
            let height = u32::from_be_bytes(data[20..24].try_into().expect("PNG height"));
            ("png", Some((width, height)))
        } else if (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"))
            && data.len() > 13
            && data.ends_with(&[0x3b])
        {
            let width = u16::from_le_bytes([data[6], data[7]]).into();
            let height = u16::from_le_bytes([data[8], data[9]]).into();
            ("gif", Some((width, height)))
        } else if data.starts_with(b"RIFF")
            && data.get(8..12) == Some(b"WEBP")
            && data.len() >= 25
            && usize::try_from(u32::from_le_bytes(
                data[4..8].try_into().expect("WebP RIFF size"),
            ))
            .ok()
            .and_then(|len| len.checked_add(8))
                == Some(data.len())
        {
            ("webp", webp_dimensions(&data))
        } else {
            return Err(artwork_error());
        };
    if dimensions.is_none_or(|(width, height)| width == 0 || height == 0) {
        return Err(artwork_error());
    }
    Ok(ImageInfo {
        extension,
        dimensions,
    })
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2;
    while index + 4 < data.len() {
        if data[index] != 0xff {
            return None;
        }
        index += 1;
        if data[index] == 0xff {
            continue;
        }
        let marker = data[index];
        index += 1;
        let length = usize::from(u16::from_be_bytes([data[index], data[index + 1]]));
        if length < 2 || index + length > data.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && length >= 7 {
            let height = u16::from_be_bytes([data[index + 3], data[index + 4]]).into();
            let width = u16::from_be_bytes([data[index + 5], data[index + 6]]).into();
            return Some((width, height));
        }
        if marker == 0xda {
            return None;
        }
        index += length;
    }
    None
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    match data.get(12..16)? {
        b"VP8X" if data.len() >= 30 => {
            let dimension =
                |index| u32::from_le_bytes([data[index], data[index + 1], data[index + 2], 0]) + 1;
            Some((dimension(24), dimension(27)))
        }
        b"VP8L" if data[20] == 0x2f => {
            let header = u32::from_le_bytes(data[21..25].try_into().ok()?);
            Some(((header & 0x3fff) + 1, ((header >> 14) & 0x3fff) + 1))
        }
        b"VP8 " if data.get(23..26) == Some(&[0x9d, 0x01, 0x2a]) && data.len() >= 30 => {
            let dimension =
                |index| u32::from(u16::from_le_bytes([data[index], data[index + 1]]) & 0x3fff);
            Some((dimension(26), dimension(28)))
        }
        _ => None,
    }
}

fn verify_pdf(path: &Path) -> Result<(), MediaError> {
    let invalid = || {
        MediaError::new(
            "booklet_format",
            "The booklet transfer did not contain a complete PDF.",
            true,
        )
    };
    let mut file = fs::File::open(path).map_err(MediaError::disk)?;
    let mut header = [0_u8; 8];
    file.read_exact(&mut header).map_err(|_| invalid())?;
    if !header.starts_with(b"%PDF-")
        || !header[5].is_ascii_digit()
        || header[6] != b'.'
        || !header[7].is_ascii_digit()
    {
        return Err(invalid());
    }
    let tail_length = file.metadata().map_err(MediaError::disk)?.len().min(2048) as usize;
    file.seek(SeekFrom::End(-(tail_length as i64)))
        .map_err(MediaError::disk)?;
    let mut tail = vec![0_u8; tail_length];
    file.read_exact(&mut tail).map_err(MediaError::disk)?;
    let end = tail
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    if !tail[..end].ends_with(b"%%EOF") {
        return Err(invalid());
    }
    Ok(())
}

async fn extra_directory(path: &Path) -> Result<PathBuf, MediaError> {
    let path = path.to_owned();
    let ancestor = blocking({
        let path = path.clone();
        move || {
            let invalid = || {
                MediaError::new(
                    "output_location",
                    "The asset location is not a safe output folder.",
                    false,
                )
            };
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(invalid());
            }
            let mut closest = None;
            for ancestor in path.ancestors() {
                // A UNC or drive prefix with no root is not a filesystem component.
                if !ancestor.is_absolute() {
                    break;
                }
                match fs::symlink_metadata(ancestor) {
                    Ok(info) => {
                        #[cfg(windows)]
                        {
                            use std::os::windows::fs::MetadataExt;
                            if info.file_attributes() & 0x400 != 0 {
                                return Err(invalid());
                            }
                        }
                        if info.file_type().is_symlink() || !info.is_dir() {
                            return Err(invalid());
                        }
                        if closest.is_none() {
                            closest = Some(ancestor.to_owned());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(MediaError::disk(error)),
                }
            }
            closest.ok_or_else(invalid)
        }
    })
    .await?;
    safe_directory(&ancestor, &path).await
}

impl Media {
    pub async fn prestage_artwork(
        &self,
        track: &ResolvedTrack,
        candidate: &Path,
        root: &Path,
        resolution: ArtworkResolution,
        basename: &str,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        cache: &ArtworkCache,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<(), MediaError> {
        cache
            .prepare(async || {
                let root = existing_root(root).await?;
                if tokio::fs::symlink_metadata(candidate).await.is_ok()
                    && let Ok(directory) = extra_directory(candidate).await
                {
                    let basename = cover_basename(basename);
                    let _lock = self.lock_path(&directory.join(basename), cancel).await?;
                    if let Ok(Some(cover)) = self
                        .verified_artwork(track, &directory, resolution, basename, cancel)
                        .await
                    {
                        self.wait_for_drive(&root, transfer, cancel).await?;
                        let file = self.store.stage(&root, ".image").await?;
                        let copied = async {
                            copy_stage(&cover.path, file.path(), cancel).await?;
                            let path = file.path().to_owned();
                            blocking(move || image_info(&path)).await
                        }
                        .await;
                        return match copied {
                            Ok(info) => Ok(StagedArtwork {
                                file,
                                info,
                                resolution: cover.resolution,
                            }),
                            Err(error) => {
                                self.store.discard(file).await;
                                Err(error)
                            }
                        };
                    }
                }
                self.stage_artwork(track, &root, resolution, transfer, cancel, &progress)
                    .await
            })
            .await
            .map(|_| ())
    }

    pub async fn artwork_for_embedding(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        staging_dir: &Path,
        resolution: ArtworkResolution,
        basename: &str,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        cache: &ArtworkCache,
    ) -> Result<PathBuf, MediaError> {
        cancel.check()?;
        validate_cover_filename(basename)?;
        let basename = cover_basename(basename);
        match tokio::fs::symlink_metadata(directory).await {
            Err(error)
                if directory.is_absolute() && error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                let directory = extra_directory(directory).await?;
                let _guard = self.lock_path(&directory.join(basename), cancel).await?;
                if let Some(cover) = self
                    .verified_artwork(track, &directory, resolution, basename, cancel)
                    .await?
                {
                    return Ok(cover.path);
                }
            }
        }
        let quiet = |_| {};
        let prepared = cache
            .prepare(async || {
                self.stage_artwork(track, staging_dir, resolution, transfer, cancel, &quiet)
                    .await
            })
            .await
            .map_err(|error| {
                if error.code == "disk_full" {
                    transfer.disk_full(error.path.as_deref().unwrap_or(staging_dir));
                }
                error
            })?;
        Ok(prepared.file.path().to_owned())
    }

    pub async fn discard_artwork(&self, cache: ArtworkCache) {
        if let Some(Ok(staged)) = cache.staged.into_inner() {
            self.store.discard(staged.file).await;
        }
    }

    pub async fn download_artwork(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        resolution: ArtworkResolution,
        cover_filename: &str,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        cache: Option<&ArtworkCache>,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<ArtworkOutput, MediaError> {
        cancel.check()?;
        validate_cover_filename(cover_filename)?;
        let cover_filename = cover_basename(cover_filename);
        let directory = extra_directory(directory).await?;
        let path = directory.join(cover_filename);
        let _path_lock = self.lock_path(&path, cancel).await?;
        let key = CoverKey {
            attempt: transfer.attempt_key(),
            album: track.album_id.clone(),
            path,
            resolution,
        };
        let cached = self.cover_failures.lock().unwrap().get(&key).cloned();
        if let Some(error) = cached {
            return Err(error);
        }
        let result = async {
            if let Some(cover) = self
                .verified_artwork(track, &directory, resolution, cover_filename, cancel)
                .await?
            {
                return Ok(cover);
            }
            let prepared = if let Some(cache) = cache {
                let cached = cache
                    .prepare(async || {
                        self.stage_artwork(
                            track, &directory, resolution, transfer, cancel, &progress,
                        )
                        .await
                    })
                    .await?;
                self.wait_for_drive(&directory, transfer, cancel).await?;
                let copy = self.store.stage(&directory, ".image").await?;
                if let Err(error) = copy_stage(cached.file.path(), copy.path(), cancel).await {
                    self.store.discard(copy).await;
                    return Err(error);
                }
                StagedArtwork {
                    file: copy,
                    info: cached.info,
                    resolution: cached.resolution,
                }
            } else {
                self.stage_artwork(track, &directory, resolution, transfer, cancel, &progress)
                    .await?
            };
            let path = directory.join(format!("{cover_filename}.{}", prepared.info.extension));
            if let Err(error) = self.wait_for_drive(&path, transfer, cancel).await {
                self.store.discard(prepared.file).await;
                return Err(error);
            }
            let reused = self
                .store
                .publish(
                    prepared.file,
                    &path,
                    &artwork_receipt(track, prepared.resolution),
                    cancel,
                )
                .await?;
            Ok(prepared.info.output(path, prepared.resolution, reused))
        }
        .await;
        if let Err(error) = &result {
            if error.code == "disk_full" {
                transfer.disk_full(error.path.as_deref().unwrap_or(&directory));
            }
            if error.code != "cancelled" && error.code != "disk_full" {
                let mut failures = self.cover_failures.lock().unwrap();
                if failures.len() >= 1024 {
                    failures.clear();
                }
                failures.insert(key, error.clone());
            }
        }
        result
    }

    async fn verified_artwork(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        resolution: ArtworkResolution,
        cover_filename: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<ArtworkOutput>, MediaError> {
        let variants = [
            ArtworkResolution::Original,
            ArtworkResolution::Px1200,
            ArtworkResolution::Px600,
            ArtworkResolution::Px300,
        ];
        for extension in COVER_EXTENSIONS {
            let path = directory.join(format!("{cover_filename}.{extension}"));
            match tokio::fs::symlink_metadata(&path).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(MediaError::disk(error)),
                Ok(_) => {}
            }
            for size in variants.into_iter().filter(|size| *size <= resolution) {
                match self
                    .store
                    .verify(&path, &artwork_receipt(track, size), cancel)
                    .await
                {
                    Ok(true) => {
                        let info = blocking({
                            let path = path.clone();
                            move || image_info(&path)
                        })
                        .await?;
                        return Ok(Some(info.output(path, size, true)));
                    }
                    Ok(false) => break,
                    Err(error) if error.code == "output_conflict" => {}
                    Err(error) => return Err(error),
                }
            }
            return Err(artwork_conflict().at_path(&path));
        }
        Ok(None)
    }

    async fn stage_artwork(
        &self,
        track: &ResolvedTrack,
        directory: &Path,
        resolution: ArtworkResolution,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<StagedArtwork, MediaError> {
        let budget = transfer.budget();
        let mut last_error = None;
        for (size, url) in artwork_sources(&track.album, resolution) {
            cancel.check()?;
            self.wait_for_drive(directory, transfer, cancel).await?;
            let stage = self.store.stage(directory, ".image").await?;
            let prepared = async {
                self.stream_asset(&url, &stage, ART_LIMIT, transfer, &budget, cancel, progress)
                    .await?;
                let info = blocking({
                    let path = stage.path().to_owned();
                    move || image_info(&path)
                })
                .await?;
                if let Some(ceiling) = resolution.px()
                    && info
                        .dimensions
                        .is_some_and(|(width, height)| width.max(height) > ceiling)
                {
                    return Err(artwork_error());
                }
                Ok(info)
            }
            .await;
            match prepared {
                Ok(info) => {
                    return Ok(StagedArtwork {
                        file: stage,
                        info,
                        resolution: size,
                    });
                }
                Err(error) => {
                    self.store.discard(stage).await;
                    if error.code != "artwork_unavailable"
                        && !(error.code.starts_with("media_") && error.retryable)
                    {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(artwork_error))
    }

    pub async fn download_booklet(
        &self,
        source: &BookletSource,
        album_id: &str,
        directory: &Path,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<PathBuf, MediaError> {
        let result = self
            .fetch_booklet(source, album_id, directory, transfer, cancel, &progress)
            .await;
        result.map_err(|error| {
            if error.code == "disk_full" {
                transfer.disk_full(error.path.as_deref().unwrap_or(directory));
            }
            error
        })
    }

    async fn fetch_booklet(
        &self,
        source: &BookletSource,
        album_id: &str,
        directory: &Path,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<PathBuf, MediaError> {
        cancel.check()?;
        let url = source.url.as_ref().ok_or_else(|| {
            MediaError::new(
                "booklet_unavailable",
                "Qobuz did not provide a usable URL for the booklet.",
                true,
            )
        })?;
        let directory = extra_directory(directory).await?;
        let target = directory.join(&source.name);
        let _path_lock = self.lock_path(&target, cancel).await?;
        let receipt = Receipt {
            kind: "booklet",
            identity: album_id.to_owned(),
            variant: source.id.clone(),
        };
        if self.store.verify(&target, &receipt, cancel).await? {
            return Ok(target);
        }
        self.wait_for_drive(&target, transfer, cancel).await?;
        let stage = self.store.stage(&directory, ".pdf").await?;
        let budget = transfer.budget();
        let prepared = async {
            self.stream_asset(
                url,
                &stage,
                BOOKLET_LIMIT,
                transfer,
                &budget,
                cancel,
                progress,
            )
            .await?;
            blocking({
                let path = stage.path().to_owned();
                move || verify_pdf(&path)
            })
            .await
        }
        .await;
        if let Err(error) = prepared {
            self.store.discard(stage).await;
            return Err(error.at_path(&target));
        }
        self.wait_for_drive(&target, transfer, cancel).await?;
        self.store.publish(stage, &target, &receipt, cancel).await?;
        Ok(target)
    }

    async fn stream_asset(
        &self,
        url: &Url,
        stage: &super::Stage,
        limit: u64,
        transfer: &TransferContext,
        budget: &RetryBudget,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<(), MediaError> {
        loop {
            match self
                .stream(url, stage, limit, transfer, cancel, progress)
                .await
            {
                Err(error) if matches!(error.code, "media_network" | "media_response") => {
                    if !cancellable(cancel, async { Ok(budget.wait(&error.message).await) }).await?
                    {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }
}

fn cover_basename(input: &str) -> &str {
    let name = input.trim();
    if name.is_empty() { "cover" } else { name }
}

fn artwork_receipt(track: &ResolvedTrack, size: ArtworkResolution) -> Receipt {
    Receipt {
        kind: "artwork",
        identity: track.album_id.clone(),
        variant: size.label().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ArtworkCache {
        pub(in crate::media) async fn seed_test_png(
            &self,
            media: &Media,
            directory: &Path,
        ) -> PathBuf {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD.decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5u0AAAAASUVORK5CYII="
            ).unwrap();
            let file = media.store.stage(directory, ".image").await.unwrap();
            fs::write(file.path(), &bytes).unwrap();
            let path = file.path().to_owned();
            assert!(
                self.staged
                    .set(Ok(StagedArtwork {
                        file,
                        info: image_data_info(&bytes).unwrap(),
                        resolution: ArtworkResolution::Original,
                    }))
                    .is_ok()
            );
            path
        }
    }

    #[tokio::test]
    async fn staged_cover_publishes_once_at_the_final_target_and_preserves_conflicts() {
        let fixture = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(fixture.path()).unwrap();
        let db = root.join("receipts.sqlite3");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(super::super::RECEIPT_SCHEMA)
            .unwrap();
        let media = Media::new(reqwest::Client::new(), db, root.clone());
        let cancel = CancellationToken::new();
        let recovery = std::sync::Arc::new(crate::recovery::Recovery::default());
        let transfer = recovery.transfer(1, 1, 1, |_| {});
        let track = ResolvedTrack {
            id: "12".into(),
            title: "Song".into(),
            album_id: "001release".into(),
            disc: 2,
            number: 1,
            track: serde_json::json!({}),
            album: serde_json::json!({}).into(),
            unavailable: None,
        };
        let artwork = ArtworkCache::default();
        let staged = artwork.seed_test_png(&media, &root).await;
        let previous_disc = root.join("Disc 2/Album");
        fs::create_dir_all(&previous_disc).unwrap();
        fs::write(previous_disc.join("Front.png"), b"old disc cover").unwrap();
        let provisional = root.join("Provisional/Album [001release]");
        let target = root.join("Delivered/Album [001release]");
        let missing = ArtworkCache::default();
        let error = media.prestage_artwork(
            &track, &provisional, &root, ArtworkResolution::Original, "Front", &transfer, &cancel, &missing, |_| {},
        ).await.unwrap_err();
        assert_eq!(error.code, "artwork_unavailable");
        assert!(!provisional.exists());
        let embed = media
            .artwork_for_embedding(
                &track,
                &provisional,
                &root,
                ArtworkResolution::Original,
                "Front",
                &transfer,
                &cancel,
                &artwork,
            )
            .await
            .unwrap();
        assert_eq!(embed, staged);
        assert!(!provisional.exists());

        let stopped = CancellationToken::new();
        stopped.cancel();
        let result = media
            .download_artwork(
                &track,
                &target,
                ArtworkResolution::Original,
                "Front",
                &transfer,
                &stopped,
                Some(&artwork),
                |_| {},
            )
            .await;
        assert_eq!(result.err().unwrap().code, "cancelled");
        assert!(!target.exists());
        let saved = media
            .download_artwork(
                &track,
                &target,
                ArtworkResolution::Original,
                "Front",
                &transfer,
                &cancel,
                Some(&artwork),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            saved.path,
            fs::canonicalize(target.join("Front.png")).unwrap()
        );
        assert!(!saved.reused);
        assert_eq!(fs::read(&saved.path).unwrap(), fs::read(&staged).unwrap());
        let verified = ArtworkCache::default();
        media.prestage_artwork(
            &track, &target, &root, ArtworkResolution::Original, "Front", &transfer, &cancel, &verified, |_| {},
        ).await.unwrap();
        assert!(matches!(verified.staged.get(), Some(Ok(_))));
        media.discard_artwork(verified).await;
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        assert_eq!(
            fs::read(previous_disc.join("Front.png")).unwrap(),
            b"old disc cover"
        );
        assert!(
            media
                .download_artwork(
                    &track,
                    &target,
                    ArtworkResolution::Original,
                    "Front",
                    &transfer,
                    &cancel,
                    None,
                    |_| {},
                )
                .await
                .unwrap()
                .reused
        );
        fs::write(&saved.path, b"foreign cover").unwrap();
        let retry = recovery.transfer(1, 2, 1, |_| {});
        assert_eq!(
            media
                .download_artwork(
                    &track,
                    &target,
                    ArtworkResolution::Original,
                    "Front",
                    &retry,
                    &cancel,
                    Some(&artwork),
                    |_| {},
                )
                .await
                .err()
                .unwrap()
                .code,
            "artwork_conflict"
        );
        assert_eq!(fs::read(&saved.path).unwrap(), b"foreign cover");
        let mut track = track;
        track.album = serde_json::json!({
            "title": "Album", "artist": {"name": "Artist"}, "media_count": 2,
        })
        .into();
        for (index, (template, expected)) in [
            ("%album artist%/%album name%", "Artist/Album"),
            (
                "%album artist%/%album name%/Disc %disc number%",
                "Artist/Album",
            ),
            (
                "%album artist%/Disc %disc number%/%album name%",
                "Artist/Album [001release]",
            ),
            ("Disc %disc number%/%album name%", "Album [001release]"),
        ]
        .into_iter()
        .enumerate()
        {
            let output = root.join(format!("Layout {index}"));
            fs::create_dir(&output).unwrap();
            let options = super::super::OutputOptions {
                folder_template: template.into(),
                safe_mode: true,
                embed_cover_art: false,
                ..Default::default()
            };
            let (_, layout) = super::super::plan_output(
                &output,
                &options,
                &track,
                &super::super::DeliveredQuality::requested(2),
                None,
                1,
            )
            .unwrap();
            let directory = layout.cover_dir.as_deref().unwrap();
            assert_eq!(directory, output.join(expected));
            let cover = media
                .download_artwork(
                    &track,
                    directory,
                    options.artwork,
                    "Front",
                    &retry,
                    &cancel,
                    Some(&artwork),
                    |_| {},
                )
                .await
                .unwrap();
            assert_eq!(
                cover.path,
                fs::canonicalize(output.join(expected).join("Front.png")).unwrap()
            );
            for disc in layout.disc_dirs.values() {
                assert!(!disc.join("Front.png").exists());
            }
        }
        media.discard_artwork(artwork).await;
        assert!(!staged.exists());
    }
    use serde_json::json;

    #[test]
    fn cover_name_and_planned_booklet_names_follow_windows_rules() {
        for invalid in [
            "../cover",
            "cover.png",
            "NUL",
            "com¹",
            "ends.",
            "has?wildcard",
        ] {
            assert!(validate_cover_filename(invalid).is_err(), "{invalid}");
        }
        for valid in ["", "cover", "front sleeve", "Album.front"] {
            assert!(validate_cover_filename(valid).is_ok(), "{valid}");
        }
        let album = json!({"goodies": [
            {"file_format_id": 21, "title": "Liner Notes", "url": "https://static.qobuz.com/a.pdf"},
            {"file_format_id": 21, "title": "Liner Notes", "url": "https://static.qobuz.com/b.pdf"},
            {"file_format_id": 21, "url": "https://static.qobuz.com/c.pdf"},
            {"file_format_id": 21, "title": "booklet_01", "url": "https://static.qobuz.com/d.pdf"},
            {"file_format_id": 21, "url": "https://static.qobuz.com/e.pdf"}
        ]});
        let names = resolve_booklets(&album, true)
            .into_iter()
            .map(|source| source.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "liner_notes.pdf",
                "liner_notes_02.pdf",
                "booklet_01.pdf",
                "booklet_01_02.pdf",
                "booklet_02.pdf"
            ]
        );
        let unsafe_title = json!({"goodies": [{"file_format_id": 21, "title": "Liner: Notes", "url": "https://static.qobuz.com/notes.pdf"}]});
        assert_eq!(
            resolve_booklets(&unsafe_title, true)[0].name,
            "liner_notes.pdf"
        );
        assert_eq!(
            resolve_booklets(&unsafe_title, false)[0].name,
            "liner__notes.pdf"
        );
        assert!(
            !super::super::metadata::metadata_catalog()
                .iter()
                .find(|field| field.id == "replaygain")
                .unwrap()
                .default_enabled
        );
    }
}
