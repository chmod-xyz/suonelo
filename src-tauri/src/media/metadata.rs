use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
};

use lofty::{
    TextEncoding,
    config::{ParseOptions, WriteOptions},
    file::AudioFile,
    flac::FlacFile,
    id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame, UnsynchronizedTextFrame},
    mpeg::MpegFile,
    ogg::OggPictureStorage,
    picture::{Picture, PictureType},
};
use serde::Serialize;
use serde_json::Value;

use super::{AudioFormat, MediaError, OutputOptions};
use crate::qobuz::{Lyrics, ResolvedTrack};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataField {
    pub id: String,
    pub label: String,
    pub default_enabled: bool,
    pub eligible: bool,
    pub description: String,
    pub value_type: &'static str,
    pub source: &'static str,
    pub available: Option<bool>,
    pub group: Option<&'static str>,
    pub raw_path: Option<String>,
    pub export_tag: Option<String>,
}

struct Field {
    id: &'static str,
    label: &'static str,
    vorbis: &'static str,
    id3: Option<&'static str>,
    enabled: bool,
}

impl Field {
    const fn new(
        id: &'static str,
        label: &'static str,
        vorbis: &'static str,
        id3: Option<&'static str>,
        enabled: bool,
    ) -> Self {
        Self {
            id,
            label,
            vorbis,
            id3,
            enabled,
        }
    }
}

const STANDARD: &[Field] = &[
    Field::new("title", "Title", "TITLE", Some("TIT2"), true),
    Field::new("artist", "Artist", "ARTIST", Some("TPE1"), true),
    Field::new(
        "albumArtist",
        "Album artist",
        "ALBUMARTIST",
        Some("TPE2"),
        true,
    ),
    Field::new("album", "Album", "ALBUM", Some("TALB"), true),
    Field::new("description", "Description", "DESCRIPTION", None, true),
    Field::new(
        "trackNumber",
        "Track number",
        "TRACKNUMBER",
        Some("TRCK"),
        true,
    ),
    Field::new(
        "trackTotal",
        "Track total",
        "TRACKTOTAL",
        Some("TRCK"),
        true,
    ),
    Field::new(
        "discNumber",
        "Disc number",
        "DISCNUMBER",
        Some("TPOS"),
        true,
    ),
    Field::new("discTotal", "Disc total", "DISCTOTAL", Some("TPOS"), true),
    Field::new("releaseDate", "Release date", "DATE", Some("TDRC"), true),
    Field::new("genre", "Genre", "GENRE", Some("TCON"), true),
    Field::new("label", "Label", "LABEL", Some("TPUB"), true),
    Field::new("isrc", "ISRC", "ISRC", Some("TSRC"), true),
    Field::new("barcode", "Barcode", "BARCODE", None, true),
    Field::new("copyright", "Copyright", "COPYRIGHT", Some("TCOP"), true),
    Field::new("composer", "Composer", "COMPOSER", Some("TCOM"), true),
    Field::new("producer", "Producer", "PRODUCER", None, true),
    Field::new("credits", "Credits", "CREDITS", None, true),
    Field::new(
        "replaygain",
        "ReplayGain",
        "REPLAYGAIN_TRACK_GAIN / REPLAYGAIN_TRACK_PEAK",
        None,
        false,
    ),
    Field::new(
        "qobuzAlbumId",
        "Qobuz album ID",
        "QOBUZ_ALBUM_ID",
        None,
        true,
    ),
    Field::new(
        "qobuzTrackId",
        "Qobuz track ID",
        "QOBUZ_TRACK_ID",
        None,
        true,
    ),
    Field::new("releaseType", "Release type", "RELEASETYPE", None, false),
];

pub fn metadata_catalog() -> Vec<MetadataField> {
    STANDARD
        .iter()
        .map(|field| MetadataField {
            id: field.id.into(),
            label: field.label.into(),
            default_enabled: field.enabled,
            eligible: true,
            description: format!(
                "FLAC {} · ID3 {}",
                field.vorbis,
                field
                    .id3
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("TXXX {}", field.vorbis))
            ),
            value_type: match field.id {
                "artist" | "genre" | "composer" | "producer" => "list",
                "trackNumber" | "trackTotal" | "discNumber" | "discTotal" => "number",
                _ => "text",
            },
            source: "Qobuz metadata",
            available: None,
            group: None,
            raw_path: None,
            export_tag: None,
        })
        .collect()
}

pub(super) fn text_at<'a>(value: &'a Value, path: &str) -> Option<&'a str> {
    value
        .pointer(path)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn value_at(value: &Value, path: &str) -> Option<String> {
    match value.pointer(path)? {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn first_value(sources: &[(&Value, &str)]) -> Vec<String> {
    sources
        .iter()
        .find_map(|(value, path)| value_at(value, path))
        .into_iter()
        .collect()
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn roles(track: &Value, wanted: &[&str]) -> Vec<String> {
    let names = text_at(track, "/performers")
        .unwrap_or_default()
        .split(" - ")
        .filter_map(|credit| {
            let (name, roles) = credit.split_once(',')?;
            roles
                .split(',')
                .any(|role| {
                    let role = role.trim().to_ascii_lowercase().replace(' ', "");
                    wanted.contains(&role.as_str())
                })
                .then(|| name.trim().to_owned())
                .filter(|name| !name.is_empty())
        })
        .collect();
    dedupe(names)
}

pub(super) fn artists(track: &ResolvedTrack) -> Vec<String> {
    let mut main = roles(&track.track, &["mainartist", "performer"]);
    if main.is_empty() {
        main = first_value(&[
            (&track.track, "/performer/name"),
            (&track.track, "/artist/name"),
            (&track.album, "/artist/name"),
        ]);
    }
    main.extend(roles(&track.track, &["featuredartist"]));
    dedupe(main)
}

pub(super) fn genres(album: &Value) -> Vec<String> {
    let values = album
        .get("genres_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .flat_map(|genre| genre.split(['→', '/']))
        .map(str::trim)
        .filter(|genre| !genre.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if values.is_empty() {
        first_value(&[(album, "/genre/name")])
    } else {
        dedupe(values)
    }
}

fn standard_values(track: &ResolvedTrack) -> BTreeMap<&'static str, Vec<String>> {
    let raw = &track.track;
    let album = &*track.album;
    let composers = roles(
        raw,
        &[
            "composer",
            "composerlyricist",
            "lyricist",
            "writer",
            "author",
        ],
    );
    let copyright = first_value(&[(raw, "/copyright"), (album, "/copyright")])
        .into_iter()
        .map(|value| value.replace("(P)", "℗").replace("(C)", "©"))
        .collect();
    BTreeMap::from([
        ("title", vec![track.title.clone()]),
        ("artist", artists(track)),
        ("albumArtist", first_value(&[(album, "/artist/name")])),
        ("album", first_value(&[(album, "/title")])),
        (
            "description",
            first_value(&[(raw, "/description"), (album, "/description")]),
        ),
        (
            "trackNumber",
            (track.number > 0)
                .then(|| track.number.to_string())
                .into_iter()
                .collect(),
        ),
        ("trackTotal", first_value(&[(album, "/tracks_count")])),
        ("discNumber", vec![track.disc.to_string()]),
        ("discTotal", first_value(&[(album, "/media_count")])),
        (
            "releaseDate",
            first_value(&[
                (album, "/release_date_original"),
                (album, "/release_date_download"),
            ]),
        ),
        ("genre", genres(album)),
        ("label", first_value(&[(album, "/label/name")])),
        ("isrc", first_value(&[(raw, "/isrc")])),
        ("barcode", first_value(&[(album, "/upc")])),
        ("copyright", copyright),
        (
            "composer",
            if composers.is_empty() {
                first_value(&[(raw, "/composer/name")])
            } else {
                composers
            },
        ),
        ("producer", roles(raw, &["producer", "co-producer"])),
        ("credits", first_value(&[(raw, "/performers")])),
        ("qobuzAlbumId", vec![track.album_id.clone()]),
        ("qobuzTrackId", vec![track.id.clone()]),
        ("releaseType", first_value(&[(album, "/release_type")])),
    ])
}

fn replaygain(track: &Value) -> impl Iterator<Item = (&'static str, String)> {
    [
        (
            "REPLAYGAIN_TRACK_GAIN",
            "/audio_info/replaygain_track_gain",
            2,
            " dB",
        ),
        (
            "REPLAYGAIN_TRACK_PEAK",
            "/audio_info/replaygain_track_peak",
            6,
            "",
        ),
    ]
    .into_iter()
    .filter_map(|(key, path, precision, suffix)| {
        let value = value_at(track, path)?.parse::<f64>().ok()?;
        value
            .is_finite()
            .then(|| (key, format!("{value:.precision$}{suffix}")))
    })
}

fn internal_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "credential",
        "signature",
        "url",
        "authorization",
        "session",
        "stream",
        "user_id",
        "app_id",
    ]
    .iter()
    .any(|part| key.contains(part))
        || [
            "image",
            "images",
            "goodies",
            "tracks",
            "albums",
            "restrictions",
            "purchasable",
            "sampleable",
            "streamable",
            "downloadable",
            "album",
            "user",
            "account",
        ]
        .contains(&key.as_str())
}

fn safe_leaf(value: &Value) -> bool {
    match value {
        Value::String(text) => {
            !text.trim().is_empty() && text.len() <= 16_384 && !text.contains("://")
        }
        Value::Number(_) | Value::Bool(_) => true,
        Value::Array(items) => {
            !items.is_empty()
                && items.len() <= 64
                && items.iter().all(|item| !item.is_array() && safe_leaf(item))
        }
        _ => false,
    }
}

fn discover<'a>(
    value: &'a Value,
    path: &str,
    depth: usize,
    output: &mut BTreeMap<String, &'a Value>,
) {
    if depth >= 6 || output.len() >= 256 {
        return;
    }
    if let Some(object) = value.as_object() {
        for (key, value) in object.iter().filter(|(key, _)| !internal_key(key)) {
            if output.len() >= 256 {
                break;
            }
            let key = key.replace('~', "~0").replace('/', "~1");
            if key.len() > 80 || !key.is_ascii() || key.contains(['\0', '=']) {
                continue;
            }
            let path = format!("{path}/{key}");
            if safe_leaf(value) {
                output.insert(path, value);
            } else if value.is_object() {
                discover(value, &path, depth + 1, output);
            } else if let Some(items) = value.as_array().filter(|items| items.len() <= 64) {
                // Array members are collected by index so embedded metadata still has a canonical JSON pointer.
                for (index, item) in items.iter().enumerate() {
                    if item.is_object() {
                        discover(item, &format!("{path}/{index}"), depth + 1, output);
                    }
                }
            }
        }
    }
}

fn dynamic(track: &ResolvedTrack) -> BTreeMap<String, &Value> {
    let mut fields = BTreeMap::new();
    discover(&track.track, "track", 0, &mut fields);
    discover(&track.album, "album", 0, &mut fields);
    fields
}

pub(super) fn catalog_track(track: &ResolvedTrack) -> Vec<MetadataField> {
    let mut known = metadata_catalog();
    let values = standard_values(track);
    for field in &mut known {
        field.available = Some(if field.id == "replaygain" {
            replaygain(&track.track).next().is_some()
        } else {
            values
                .get(field.id.as_str())
                .is_some_and(|value| !value.is_empty())
        });
    }
    known.extend(dynamic(track).into_iter().map(|(id, value)| MetadataField {
        description: format!("Qobuz JSON pointer {id} · FLAC QOBUZ:{id} · ID3 TXXX QOBUZ:{id}"),
        label: friendly_label(&id),
        default_enabled: false,
        eligible: true,
        value_type: match value {
            Value::Array(_) => "list",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            _ => "text",
        },
        source: if id.starts_with("album/") {
            "Qobuz album"
        } else {
            "Qobuz track"
        },
        available: Some(true),
        group: Some(if id.starts_with("album/") {
            "album"
        } else {
            "track"
        }),
        raw_path: Some(id.clone()),
        export_tag: Some(format!("QOBUZ:{id}")),
        id,
    }));
    known
}

fn friendly_label(path: &str) -> String {
    path.split('/')
        .skip(1)
        .map(|segment| {
            let decoded = segment.replace("~1", "/").replace("~0", "~");
            decoded
                .split('_')
                .filter(|word| !word.is_empty())
                .map(|word| match word.to_ascii_lowercase().as_str() {
                    "id" | "ids" | "isrc" | "upc" | "ean" | "bpm" | "url" => {
                        word.to_ascii_uppercase()
                    }
                    "replaygain" => "ReplayGain".into(),
                    _ => {
                        let mut characters = word.chars();
                        match characters.next() {
                            Some(first) => first.to_uppercase().chain(characters).collect(),
                            None => String::new(),
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn advanced_catalog_keeps_raw_identity_exports_and_sensitive_filters() {
        let track = ResolvedTrack {
            id: "3".into(), title: "Song".into(), album_id: "album42".into(), disc: 1, number: 3, unavailable: None,
            track: json!({"title": "Song", "user_token": "secret", "signed_url": "https://example.invalid/signed", "audio_info": {"replaygain_track_gain": -5.0}}),
            album: json!({"title": "Album", "genre": {"name": "Electronic"}, "release_date_original": "2026-01-01"}).into(),
        };
        let catalog = catalog_track(&track);
        let raw = catalog
            .iter()
            .find(|field| field.id == "album/genre/name")
            .unwrap();
        assert_eq!(raw.label, "Genre / Name");
        assert_eq!(raw.group, Some("album"));
        assert_eq!(raw.raw_path.as_deref(), Some("album/genre/name"));
        assert_eq!(raw.export_tag.as_deref(), Some("QOBUZ:album/genre/name"));
        assert!(!raw.default_enabled);
        assert!(
            catalog
                .iter()
                .filter(|field| field.id.contains('/'))
                .all(|field| !field.id.contains("token") && !field.id.contains("url"))
        );
        let mut options = OutputOptions::default();
        for id in ["qobuzAlbumId", "qobuzTrackId"] {
            let field = STANDARD.iter().find(|field| field.id == id).unwrap();
            assert!(
                catalog
                    .iter()
                    .find(|field| field.id == id)
                    .unwrap()
                    .default_enabled
            );
            assert!(enabled(&options, field));
            options.tags.insert(id.into(), false);
            assert!(!enabled(&options, field));
        }
        assert!(!custom_tags(&track, &options).contains_key("QOBUZ:album/genre/name"));
        options.tags.insert("album/genre/name".into(), true);
        assert_eq!(
            custom_tags(&track, &options)["QOBUZ:album/genre/name"],
            ["Electronic"]
        );
        options.tags.insert("album/genre/name".into(), false);
        assert!(!custom_tags(&track, &options).contains_key("QOBUZ:album/genre/name"));
        assert!(
            catalog
                .iter()
                .find(|field| field.id == "genre")
                .unwrap()
                .raw_path
                .is_none()
        );
    }
}

fn enabled(options: &OutputOptions, field: &Field) -> bool {
    options.tags.get(field.id).copied().unwrap_or(field.enabled)
}

fn custom_tags(track: &ResolvedTrack, options: &OutputOptions) -> BTreeMap<String, Vec<String>> {
    let mut tags = BTreeMap::new();
    for (id, value) in dynamic(track)
        .into_iter()
        .filter(|(id, _)| options.tags.get(id) == Some(&true))
    {
        let encode = |value: &Value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        };
        let values = value
            .as_array()
            .map(|items| items.iter().map(encode).collect())
            .unwrap_or_else(|| vec![encode(value)]);
        tags.insert(format!("QOBUZ:{id}"), values);
    }
    if options.tags.get("replaygain").copied().unwrap_or(false) {
        tags.extend(replaygain(&track.track).map(|(key, value)| (key.to_owned(), vec![value])));
    }
    tags
}

fn set_id3_text(tag: &mut Id3v2Tag, id: &'static str, value: String) {
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed(id)),
        TextEncoding::UTF8,
        value,
    )));
}

fn set_id3_position(
    tag: &mut Id3v2Tag,
    id: &'static str,
    number: Option<&str>,
    total: Option<&str>,
) {
    if number.is_none() && total.is_none() {
        return;
    }
    let source = tag
        .get_text(&FrameId::Valid(Cow::Borrowed(id)))
        .unwrap_or_default();
    let (source_number, source_total) = source.split_once('/').unwrap_or((source, ""));
    let (number, total) = (
        number.unwrap_or(source_number),
        total.unwrap_or(source_total),
    );
    let value = if total.is_empty() {
        number.to_owned()
    } else {
        format!("{number}/{total}")
    };
    set_id3_text(tag, id, value);
}

pub(super) fn write_tags(
    path: &Path,
    format: AudioFormat,
    track: &ResolvedTrack,
    options: &OutputOptions,
    lyrics: Option<&Lyrics>,
    cover: Option<&Path>,
) -> Result<(), MediaError> {
    let error = || {
        MediaError::new(
            "tags",
            "Could not write or preserve the audio metadata.",
            true,
        )
    };
    let mut source = File::open(path).map_err(|_| error())?;
    let mut values = standard_values(track);
    values.retain(|id, values| {
        !values.is_empty()
            && STANDARD
                .iter()
                .any(|field| field.id == *id && enabled(options, field))
    });
    let custom = custom_tags(track, options);
    if values.is_empty() && custom.is_empty() && lyrics.is_none() && cover.is_none() {
        return Ok(());
    }
    let picture = cover
        .map(|cover| {
            let failure = || {
                MediaError::new(
                    "artwork_embed",
                    "The cover was saved beside the audio but could not be embedded.",
                    true,
                )
                .at_path(cover)
            };
            let mut source = File::open(cover).map_err(|_| failure())?;
            if format == AudioFormat::Flac
                && source.metadata().map_err(|_| failure())?.len() >= 16_777_000
            {
                return Err(failure());
            }
            let mut picture = Picture::from_reader(&mut source).map_err(|_| failure())?;
            picture.set_pic_type(PictureType::CoverFront);
            Ok(picture)
        })
        .transpose()?;
    match format {
        AudioFormat::Flac => {
            let mut audio =
                FlacFile::read_from(&mut source, ParseOptions::new()).map_err(|_| error())?;
            drop(source);
            if audio.vorbis_comments().is_none() {
                audio.set_vorbis_comments(Default::default());
            }
            let tag = audio.vorbis_comments_mut().ok_or_else(error)?;
            let mut set = |key: String, values: Vec<String>| {
                let mut values = values.into_iter();
                if let Some(first) = values.next() {
                    tag.insert(key.clone(), first);
                    for value in values {
                        tag.push(key.clone(), value);
                    }
                }
            };
            for field in STANDARD {
                if let Some(values) = values.remove(field.id) {
                    set(field.vorbis.to_owned(), values);
                }
            }
            for (key, values) in custom {
                set(key, values);
            }
            if let Some(lyrics) = lyrics {
                set("LYRICS".into(), vec![lyrics.text.clone()]);
            }
            if let Some(picture) = picture {
                audio.remove_picture_type(PictureType::CoverFront);
                audio.insert_picture(picture, None).map_err(|_| error())?;
            }
            audio
                .save_to_path(path, WriteOptions::default())
                .map_err(|_| error())?;
        }
        AudioFormat::Mp3 => {
            let mut audio =
                MpegFile::read_from(&mut source, ParseOptions::new()).map_err(|_| error())?;
            drop(source);
            if audio.id3v2().is_none() {
                audio.set_id3v2(Id3v2Tag::new());
            }
            let tag = audio.id3v2_mut().ok_or_else(error)?;
            let text = |key| {
                values
                    .get(key)
                    .and_then(|values| values.first())
                    .map(String::as_str)
            };
            set_id3_position(tag, "TRCK", text("trackNumber"), text("trackTotal"));
            set_id3_position(tag, "TPOS", text("discNumber"), text("discTotal"));
            for field in STANDARD {
                let Some(values) = values.remove(field.id) else {
                    continue;
                };
                match field.id3 {
                    Some("TRCK" | "TPOS") => {}
                    Some(id) => set_id3_text(tag, id, values.join("\0")),
                    None => {
                        tag.insert_user_text(field.vorbis.to_owned(), values.join("\0"));
                    }
                }
            }
            for (key, values) in custom {
                tag.insert_user_text(key, values.join("\0"));
            }
            if let Some(lyrics) = lyrics {
                // The LRC timestamps remain in the text for players which recognize embedded LRC.
                tag.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
                    TextEncoding::UTF8,
                    *b"und",
                    "suonelo lyrics",
                    lyrics.text.clone(),
                )));
            }
            if let Some(picture) = picture {
                tag.remove_picture_type(PictureType::CoverFront);
                tag.insert_picture(picture);
            }
            audio
                .save_to_path(path, WriteOptions::default())
                .map_err(|_| error())?;
        }
    }
    Ok(())
}
