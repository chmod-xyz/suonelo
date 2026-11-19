use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use super::{DeliveredQuality, MediaError, OutputOptions, ReleaseLayout, metadata};
use crate::qobuz::ResolvedTrack;

const TOKENS: &[&str] = &[
    "artist",
    "album artist",
    "track title",
    "track number",
    "year",
    "album name",
    "album id",
    "target quality",
    "bit depth",
    "sampling rate",
    "genre",
    "disc number",
];

#[derive(Debug)]
enum Part {
    Text(String),
    Token(String),
    Optional(Vec<Part>),
}

#[derive(Debug)]
pub struct OutputTemplate {
    folders: Vec<Vec<Part>>,
    filename: Vec<Part>,
}

fn bad(message: &'static str) -> MediaError {
    MediaError::new("output_template", message, false)
}

fn parts(input: &str, optional: bool) -> Result<Vec<Part>, MediaError> {
    let mut parts = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        // Brackets inside an optional group are literal output characters.
        let next = if optional {
            rest.find('%')
        } else {
            rest.find(['[', ']', '%'])
        }
        .unwrap_or(rest.len());
        if next > 0 {
            parts.push(Part::Text(rest[..next].to_owned()));
            rest = &rest[next..];
            continue;
        }
        let (end, is_group) = match rest.as_bytes()[0] {
            b'%' => ('%', false),
            b'[' if !optional => (']', true),
            _ => return Err(bad("An optional group has an unmatched closing bracket.")),
        };
        let tail = &rest[1..];
        let (value, remainder) = if is_group {
            let mut depth = 1;
            let closing = tail.char_indices().find_map(|(index, character)| {
                match character {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(index)
            });
            let closing =
                closing.ok_or_else(|| bad("Close every output token and optional group."))?;
            (&tail[..closing], &tail[closing + 1..])
        } else {
            tail.split_once(end)
                .ok_or_else(|| bad("Close every output token and optional group."))?
        };
        if is_group {
            let children = self::parts(value, true)?;
            if !children.iter().any(|part| matches!(part, Part::Token(_))) {
                return Err(bad("An optional group needs at least one metadata token."));
            }
            parts.push(Part::Optional(children));
        } else {
            let value = if value == "album title" {
                "album name"
            } else {
                value
            };
            if !TOKENS.contains(&value) {
                return Err(bad(
                    "The output template contains an unknown metadata token.",
                ));
            }
            parts.push(Part::Token(value.to_owned()));
        }
        rest = remainder;
    }
    Ok(parts)
}

pub fn parse_template(folder: &str, filename: &str) -> Result<OutputTemplate, MediaError> {
    if folder.len() + filename.len() > 4096 {
        return Err(bad("The output template is too long."));
    }
    if filename.trim().is_empty() || filename.contains(['/', '\\', ':']) {
        return Err(bad(
            "Enter a filename template without path separators or a drive letter.",
        ));
    }
    if folder.contains(':') {
        return Err(bad(
            "The folder template must be relative to the destination.",
        ));
    }
    let validate = |value: &str| {
        if matches!(value.trim(), "" | "." | "..") {
            return Err(bad(
                "Use nonempty relative path components without traversal.",
            ));
        }
        parts(value, false)
    };
    Ok(OutputTemplate {
        folders: if folder.is_empty() {
            Vec::new()
        } else {
            folder
                .split(['/', '\\'])
                .map(validate)
                .collect::<Result<_, _>>()?
        },
        filename: validate(filename)?,
    })
}

fn render(parts: &[Part], values: &BTreeMap<&str, String>) -> String {
    render_with_album_identity(parts, values, true)
}

fn render_with_album_identity(
    parts: &[Part],
    values: &BTreeMap<&str, String>,
    identity: bool,
) -> String {
    let mut output = String::new();
    for part in parts {
        match part {
            Part::Text(text) => output.push_str(text),
            Part::Token(key) => {
                if (identity || !matches!(key.as_str(), "album name" | "album id"))
                    && let Some(value) = values.get(key.as_str())
                {
                    output.push_str(value);
                }
            }
            Part::Optional(children) => {
                if children.iter().all(|part| match part {
                    Part::Token(key) => values.get(key.as_str()).is_some_and(|v| !v.is_empty()),
                    _ => true,
                }) {
                    output.push_str(&render_with_album_identity(children, values, identity));
                }
            }
        }
    }
    output
}

pub(super) fn sanitize(component: &str, safe_mode: bool) -> String {
    let original = component.trim().trim_end_matches(['.', ' ']);
    if original.is_empty() {
        return String::new();
    }
    let mut safe = sanitize_filename::sanitize_with_options(
        original,
        sanitize_filename::Options {
            windows: true,
            truncate: false,
            replacement: if safe_mode { "" } else { "_" },
        },
    );
    if safe.len() > 220 {
        let end = safe.floor_char_boundary(220);
        safe.truncate(end);
    }
    safe.trim_end_matches(['.', ' ']).to_owned()
}

fn has_token(parts: &[Part], name: &str) -> bool {
    parts.iter().any(|part| match part {
        Part::Token(token) => token == name,
        Part::Optional(children) => has_token(children, name),
        _ => false,
    })
}

fn render_folders(
    components: &[Vec<Part>],
    values: &BTreeMap<&str, String>,
    safe_mode: bool,
) -> PathBuf {
    let mut result = PathBuf::new();
    for component in components {
        let mut safe = sanitize(&render(component, values), safe_mode);
        if safe.is_empty()
            && component
                .iter()
                .any(|part| matches!(part, Part::Token(name) if name == "album name"))
            && let Some(id) = values.get("album id")
        {
            safe = sanitize(id, safe_mode);
        }
        if !safe.is_empty() {
            result.push(safe);
        }
    }
    result
}

fn cover_directory(
    album_root: &Path,
    disc_dirs: &BTreeMap<u32, PathBuf>,
    prefix: &[Vec<Part>],
    values: &BTreeMap<&str, String>,
    safe_mode: bool,
) -> Option<PathBuf> {
    let id = values.get("album id")?;
    let release_specific = prefix.iter().any(|component| {
        let rendered = sanitize(&render(component, values), safe_mode);
        let without_identity = sanitize(
            &render_with_album_identity(component, values, false),
            safe_mode,
        );
        rendered != without_identity
            || (rendered.is_empty()
                && !sanitize(id, safe_mode).is_empty()
                && component
                    .iter()
                    .any(|part| matches!(part, Part::Token(name) if name == "album name")))
    });
    if release_specific || disc_dirs.is_empty() {
        return Some(album_root.to_owned());
    }
    if disc_dirs.len() == 1 {
        return disc_dirs.values().next().cloned();
    }
    // Use the canonical identity as part of the directory name even if the title
    // needs truncating. An invalid ID cannot safely distinguish two editions.
    if id.is_empty() || sanitize(id, safe_mode) != *id || id.len() > 218 {
        return None;
    }
    let suffix = format!("[{id}]");
    let mut title = values
        .get("album name")
        .map_or_else(String::new, |title| sanitize(title, safe_mode));
    let room = 220usize.saturating_sub(suffix.len() + 1);
    title.truncate(title.floor_char_boundary(room.min(title.len())));
    let title = title.trim_end_matches(['.', ' ']);
    let name = if title.is_empty() {
        suffix
    } else {
        format!("{title} {suffix}")
    };
    Some(album_root.join(name))
}

impl OutputTemplate {
    fn filename(
        &self,
        values: &BTreeMap<&str, String>,
        quality: &DeliveredQuality,
        sequence: usize,
        safe_mode: bool,
    ) -> String {
        let mut name = sanitize(&render(&self.filename, values), safe_mode);
        let missing_track =
            !values.contains_key("track title") && !values.contains_key("track number");
        let uses_track =
            has_token(&self.filename, "track title") || has_token(&self.filename, "track number");
        if name.is_empty()
            || missing_track
                && uses_track
                && name.chars().all(|character| !character.is_alphanumeric())
        {
            name = format!("{:02}", sequence.max(1));
        }
        format!("{name}.{}", quality.format.extension())
    }
}

fn add_quality(values: &mut BTreeMap<&str, String>, tier: u8, quality: &DeliveredQuality) {
    values.insert("target quality", tier.to_string());
    if let Some(depth) = quality.bit_depth {
        values.insert("bit depth", depth.to_string());
    }
    if let Some(rate) = quality.sample_rate {
        values.insert("sampling rate", rate.to_string());
    }
}

pub fn preview_output(
    folder: &str,
    filename: &str,
    quality: u8,
    safe_mode: bool,
) -> Result<PathBuf, MediaError> {
    let template = parse_template(folder, filename)?;
    let quality = DeliveredQuality::requested(quality);
    let mut values = BTreeMap::from([
        ("artist", "Aster Vale".into()),
        ("album artist", "Aster Vale".into()),
        ("track title", "Quiet orbit".into()),
        ("track number", "03".into()),
        ("year", "2026".into()),
        ("album name", "Music for the Moon".into()),
        ("album id", "x1sample42".into()),
        ("genre", "Electronic".into()),
        ("disc number", "1".into()),
    ]);
    add_quality(&mut values, quality.tier, &quality);
    Ok(render_folders(&template.folders, &values, safe_mode)
        .join(template.filename(&values, &quality, 3, safe_mode)))
}

pub fn plan_output(
    root: &Path,
    options: &OutputOptions,
    track: &ResolvedTrack,
    quality: &DeliveredQuality,
    pinned: Option<&ReleaseLayout>,
    sequence: usize,
) -> Result<(PathBuf, ReleaseLayout), MediaError> {
    let template = parse_template(&options.folder_template, &options.filename_template)?;
    let album = &*track.album;
    let mut values = BTreeMap::from([
        ("artist", metadata::artists(track).join(", ")),
        ("album id", track.album_id.clone()),
        ("disc number", track.disc.to_string()),
    ]);
    if let Some(artist) = metadata::text_at(album, "/artist/name") {
        values.insert("album artist", artist.to_owned());
    }
    if let Some(title) = metadata::text_at(album, "/title") {
        values.insert("album name", title.to_owned());
    }
    if metadata::text_at(&track.track, "/title").is_some() {
        values.insert("track title", track.title.clone());
    }
    if track.number > 0 {
        values.insert("track number", format!("{:02}", track.number));
    }
    if let Some(year) = metadata::text_at(album, "/release_date_original")
        .or_else(|| metadata::text_at(album, "/release_date_download"))
        .and_then(|date| date.get(..4))
        .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
    {
        values.insert("year", year.to_owned());
    }
    if let Some(genre) = metadata::genres(album).into_iter().next() {
        values.insert("genre", genre);
    }
    add_quality(&mut values, options.quality, quality);
    let total_discs = album
        .get("media_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .max(u64::from(track.disc))
        .clamp(1, 999) as u32;
    let first_disc_component = template
        .folders
        .iter()
        .position(|part| has_token(part, "disc number"));
    let layout = if let Some(layout) = pinned {
        layout.clone()
    } else if let Some(index) = first_disc_component {
        let album_root = root.join(render_folders(
            &template.folders[..index],
            &values,
            options.safe_mode,
        ));
        // Validate the complete template for every disc before any transfers use it.
        let mut disc_dirs = BTreeMap::new();
        for disc in 1..=total_discs {
            values.insert("disc number", disc.to_string());
            let path = root.join(render_folders(
                &template.folders,
                &values,
                options.safe_mode,
            ));
            disc_dirs.insert(disc, path);
        }
        // An optional group may omit the disc token together with missing metadata.
        // Restore disc grouping for every affected output before any file is written.
        let duplicated = disc_dirs
            .iter()
            .filter_map(|(disc, path)| {
                disc_dirs
                    .iter()
                    .any(|(other_disc, other_path)| {
                        disc != other_disc
                            && path
                                .to_string_lossy()
                                .eq_ignore_ascii_case(&other_path.to_string_lossy())
                    })
                    .then_some(*disc)
            })
            .collect::<Vec<_>>();
        for disc in duplicated {
            disc_dirs
                .get_mut(&disc)
                .expect("planned disc directory")
                .push(format!("Disc {disc}"));
        }
        values.insert("disc number", track.disc.to_string());
        let cover_dir = cover_directory(
            &album_root,
            &disc_dirs,
            &template.folders[..index],
            &values,
            options.safe_mode,
        );
        ReleaseLayout {
            album_root,
            disc_dirs,
            cover_dir,
        }
    } else {
        let album_root = root.join(render_folders(
            &template.folders,
            &values,
            options.safe_mode,
        ));
        let disc_dirs = (total_discs > 1)
            .then(|| {
                (1..=total_discs)
                    .map(|disc| (disc, album_root.join(format!("Disc {disc}"))))
                    .collect()
            })
            .unwrap_or_default();
        let cover_dir = cover_directory(
            &album_root,
            &disc_dirs,
            &template.folders,
            &values,
            options.safe_mode,
        );
        ReleaseLayout {
            album_root,
            disc_dirs,
            cover_dir,
        }
    };
    let path = layout.disc_dir(track.disc).join(template.filename(
        &values,
        quality,
        sequence,
        options.safe_mode,
    ));
    Ok((path, layout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_layout_shares_identified_roots_and_uses_a_safe_disc_first_fallback() {
        use serde_json::json;
        use std::sync::Arc;
        let root = Path::new("C:/Music");
        let track = ResolvedTrack {
            id: "11".into(),
            title: "Song".into(),
            album_id: "001release".into(),
            disc: 2,
            number: 1,
            track: json!({"title":"Song"}),
            album: Arc::new(json!({"title":"Album","artist":{"name":"Artist"},"media_count":2})),
            unavailable: None,
        };
        for (template, audio, cover) in [
            (
                "%album artist%/%album name%",
                "Artist/Album/Disc 2",
                "Artist/Album",
            ),
            (
                "%album artist%/%album name%/Disc %disc number%",
                "Artist/Album/Disc 2",
                "Artist/Album",
            ),
            (
                "%album artist%/Disc %disc number%/%album name%",
                "Artist/Disc 2/Album",
                "Artist/Album [001release]",
            ),
            (
                "Disc %disc number%/%album name%",
                "Disc 2/Album",
                "Album [001release]",
            ),
            (
                "%album artist%",
                "Artist/Disc 2",
                "Artist/Album [001release]",
            ),
            (
                "[%album name% %year%]/Disc %disc number%/%album name%",
                "Disc 2/Album",
                "Album [001release]",
            ),
            (
                "%album id%/Disc %disc number%",
                "001release/Disc 2",
                "001release",
            ),
        ] {
            let options = OutputOptions {
                folder_template: template.into(),
                ..Default::default()
            };
            let (path, layout) = plan_output(
                root,
                &options,
                &track,
                &DeliveredQuality::requested(4),
                None,
                1,
            )
            .unwrap();
            assert_eq!(
                path.parent(),
                Some(root.join(audio).as_path()),
                "{template}"
            );
            assert_eq!(layout.cover_dir, Some(root.join(cover)), "{template}");
            let first = ResolvedTrack {
                disc: 1,
                ..track.clone()
            };
            let (_, pinned) = plan_output(
                Path::new("D:/Elsewhere"),
                &options,
                &first,
                &DeliveredQuality::requested(2),
                Some(&layout),
                1,
            )
            .unwrap();
            assert_eq!(pinned.cover_dir, layout.cover_dir);
        }

        let options = OutputOptions {
            folder_template: "Disc %disc number%/%album name%".into(),
            ..Default::default()
        };
        let fallback = |track: &ResolvedTrack| {
            plan_output(
                root,
                &options,
                track,
                &DeliveredQuality::requested(4),
                None,
                1,
            )
            .unwrap()
            .1
            .cover_dir
            .unwrap()
        };
        let other = ResolvedTrack {
            album_id: "002release".into(),
            ..track.clone()
        };
        assert_ne!(fallback(&track), fallback(&other));
        for title in [json!(null), json!("***")] {
            let missing = ResolvedTrack {
                album: Arc::new(json!({"title":title,"media_count":2})),
                ..track.clone()
            };
            assert_eq!(fallback(&missing), root.join("[001release]"));
        }
        let long = ResolvedTrack {
            album: Arc::new(json!({"title":"é".repeat(150),"media_count":2})),
            ..track.clone()
        };
        let long = fallback(&long);
        let name = long.file_name().unwrap().to_str().unwrap();
        assert!(name.len() <= 220 && name.ends_with(" [001release]"));
        let one_disc = ResolvedTrack {
            album: Arc::new(json!({"title":"Album","media_count":1})),
            disc: 1,
            ..track
        };
        assert_eq!(fallback(&one_disc), root.join("Disc 1/Album"));
    }

    #[test]
    fn authoritative_naming_and_safe_mode_fixtures_match_download_paths() {
        use serde_json::{Value, json};

        let fixtures: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/implementation-fixtures.json"
        ))
        .unwrap();
        for sample in fixtures["safeComponents"].as_array().unwrap() {
            let input = sample["input"].as_str().unwrap();
            assert_eq!(
                sanitize(input, true),
                sample["expectedOn"].as_str().unwrap(),
                "{}",
                sample["id"]
            );
            assert_eq!(
                sanitize(input, false),
                sample["expectedOff"].as_str().unwrap(),
                "{}",
                sample["id"]
            );
        }
        for control in '\0'..='\u{1f}' {
            let input = format!("A{control}B");
            assert_eq!(sanitize(&input, true), "AB");
            assert_eq!(sanitize(&input, false), "A_B");
        }
        let defaults = OutputOptions::default();
        assert_eq!(
            defaults.folder_template,
            fixtures["latestDefaults"]["folderTemplate"]
                .as_str()
                .unwrap()
        );
        assert_eq!(
            defaults.filename_template,
            fixtures["latestDefaults"]["filenameTemplate"]
                .as_str()
                .unwrap()
        );
        for case in fixtures["naming"].as_array().unwrap() {
            let mut values = fixtures["sampleMetadata"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str().unwrap().to_owned()))
                .collect::<BTreeMap<_, _>>();
            if let Some(overrides) = case["metadataOverrides"].as_object() {
                for (key, value) in overrides {
                    if value.is_null() {
                        values.remove(key.as_str());
                    } else {
                        values.insert(key, value.as_str().unwrap().into());
                    }
                }
            }
            if let Some(component) = case["componentTemplate"].as_str() {
                let template = parse_template(component, "%track title%").unwrap();
                assert_eq!(
                    render_folders(&template.folders, &values, true).to_string_lossy(),
                    case["expectedComponent"].as_str().unwrap(),
                    "{}",
                    case["id"]
                );
                continue;
            }
            let options = OutputOptions {
                quality: case["requestedTier"].as_u64().unwrap_or(4) as u8,
                folder_template: case["folderTemplate"]
                    .as_str()
                    .unwrap_or(&defaults.folder_template)
                    .into(),
                filename_template: case["filenameTemplate"]
                    .as_str()
                    .unwrap_or(&defaults.filename_template)
                    .into(),
                ..defaults.clone()
            };
            let mut delivered = DeliveredQuality::requested(options.quality);
            if case["delivered"].is_object() {
                delivered.bit_depth = case["delivered"]["bitDepth"]
                    .as_u64()
                    .map(|depth| depth as u32);
                delivered.sample_rate = case["delivered"]["samplingRateKHz"].as_f64();
            }
            let track = ResolvedTrack {
                id: "3".into(), title: values["track title"].clone(), album_id: values["album id"].clone(),
                disc: 1, number: 3, unavailable: None,
                track: json!({"title": values["track title"], "performer": {"name": values["artist"]}}),
                album: json!({"title": values["album name"], "artist": {"name": values["album artist"]}, "release_date_original": values.get("year").map(|year| format!("{year}-01-01")), "media_count": 1}).into(),
            };
            let (path, _) =
                plan_output(Path::new(""), &options, &track, &delivered, None, 3).unwrap();
            assert_eq!(
                path.to_string_lossy().replace('\\', "/"),
                case["expectedRelativePath"].as_str().unwrap(),
                "{}",
                case["id"]
            );
        }
    }

    #[test]
    fn template_rejects_traversal_and_unknown_tokens() {
        for folder in [
            "../foo",
            "C:/foo",
            "//server/share",
            "x//y",
            "%unknown%",
            "[(no token)]",
        ] {
            assert!(parse_template(folder, "name").is_err(), "{folder}");
        }
        assert!(parse_template("folder", "one/two").is_err());
    }

    #[test]
    fn preview_and_optional_groups() {
        let path = preview_output(
            "%album artist%/%album name%[ (%year%)]",
            "%track number% - %track title%",
            4,
            true,
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("Aster Vale")
                .join("Music for the Moon (2026)")
                .join("03 - Quiet orbit.flac")
        );
        let template = parse_template("Records/[Year %year%]/Album[ (%year%)]", "(Title)").unwrap();
        assert_eq!(
            render_folders(&template.folders, &BTreeMap::new(), true),
            PathBuf::from("Records").join("Album")
        );
        assert_eq!(
            template.filename(&BTreeMap::new(), &DeliveredQuality::requested(1), 3, true),
            "(Title).mp3"
        );
    }

    #[test]
    fn optional_year_spacing_stays_literal_in_both_forms() {
        for folder in ["%album name%[ (%year%)]", "%album name% [(%year%)]"] {
            assert_eq!(
                preview_output(folder, "%track title%", 4, true).unwrap(),
                PathBuf::from("Music for the Moon (2026)").join("Quiet orbit.flac")
            );
            let template = parse_template(folder, "%track title%").unwrap();
            let values = BTreeMap::from([("album name", "Music for the Moon".into())]);
            assert_eq!(
                render_folders(&template.folders, &values, true),
                PathBuf::from("Music for the Moon")
            );
        }

        let values = BTreeMap::from([("album name", "Music for the Moon".into())]);
        for (folder, expected) in [
            (
                "%album name%[ (%year%)] - Deluxe",
                "Music for the Moon - Deluxe",
            ),
            (
                "%album name% [(%year%)] - Deluxe",
                "Music for the Moon  - Deluxe",
            ),
        ] {
            let template = parse_template(folder, "%track title%").unwrap();
            assert_eq!(
                render_folders(&template.folders, &values, true),
                PathBuf::from(expected)
            );
        }
    }

    #[test]
    fn folder_and_filename_preserve_interior_spaces_and_trim_edges() {
        assert_eq!(
            preview_output(
                "  %artist%     %album name%  ",
                "  %track number%     %track title%  ",
                4,
                true,
            )
            .unwrap(),
            PathBuf::from("Aster Vale     Music for the Moon").join("03     Quiet orbit.flac")
        );
    }

    #[test]
    fn multiple_optional_groups_in_each_field_render_independently() {
        let path = preview_output(
            "%album artist%/%album name%[ (%year%)][ - %genre%]",
            "[%disc number% - ]%track number% - %track title%[ (%bit depth% bit)][ (%sampling rate% kHz)]",
            4,
            true,
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("Aster Vale")
                .join("Music for the Moon (2026) - Electronic")
                .join("1 - 03 - Quiet orbit (24 bit) (192 kHz).flac")
        );

        let template = parse_template(
            "Album[ (%year%)][ - %genre%]",
            "%track title%[ (%bit depth% bit)][ (%sampling rate% Hz)]",
        )
        .unwrap();
        let values = BTreeMap::from([
            ("genre", "Electronic".into()),
            ("track title", "Quiet orbit".into()),
            ("sampling rate", "44100".into()),
        ]);
        assert_eq!(
            render_folders(&template.folders, &values, true),
            PathBuf::from("Album - Electronic")
        );
        assert_eq!(
            template.filename(&values, &DeliveredQuality::requested(2), 1, true),
            "Quiet orbit (44100 Hz).flac"
        );
    }

    #[test]
    fn optional_groups_allow_literal_brackets_and_album_title_alias() {
        let folder = "[(%year%)] %album title% [[%bit depth%-Bits]]";
        assert_eq!(
            preview_output(folder, "%track title%", 4, true).unwrap(),
            PathBuf::from("(2026) Music for the Moon [24-Bits]").join("Quiet orbit.flac")
        );
        assert_eq!(
            preview_output(folder, "%track title% [[%bit depth%-Bits]]", 1, true).unwrap(),
            PathBuf::from("(2026) Music for the Moon").join("Quiet orbit.mp3")
        );
        for folder in ["[[%year%]", "[%year%]]", "[[text]]", "[%year%"] {
            assert!(parse_template(folder, "%track title%").is_err(), "{folder}");
        }
    }

    #[test]
    fn missing_metadata_keeps_stable_serials_album_identity_and_discs() {
        use serde_json::json;

        let root = Path::new("Music");
        let options = OutputOptions {
            folder_template: "%album artist%/%album name% [(%year%)]".into(),
            filename_template: "%track number% - %track title%".into(),
            ..Default::default()
        };
        let track = ResolvedTrack {
            id: "33".into(),
            title: "Unavailable track".into(),
            album_id: "album42".into(),
            disc: 2,
            number: 0,
            track: json!({}),
            album: json!({"media_count": 2}).into(),
            unavailable: None,
        };
        let quality = DeliveredQuality::requested(2);
        let (path, layout) = plan_output(root, &options, &track, &quality, None, 3).unwrap();
        assert_eq!(path, root.join("album42").join("Disc 2").join("03.flac"));
        let optional_disc = OutputOptions {
            folder_template: "%album name%/[%genre% Disc %disc number%]".into(),
            ..options
        };
        let (path, _) = plan_output(root, &optional_disc, &track, &quality, None, 3).unwrap();
        assert_eq!(path, root.join("album42").join("Disc 2").join("03.flac"));
        let changed = OutputOptions {
            folder_template: "Changed".into(),
            quality: 1,
            ..optional_disc
        };
        let (path, _) = plan_output(
            Path::new("Other"),
            &changed,
            &track,
            &DeliveredQuality::requested(1),
            Some(&layout),
            3,
        )
        .unwrap();
        assert_eq!(path, root.join("album42").join("Disc 2").join("03.mp3"));
    }
}
