use crate::{
    error::{AppError, Result},
    qobuz::{CanonicalResource, ResourceKey, normalize_link},
};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Read,
    path::Path,
};

pub const MAX_LINK_BYTES: usize = 2_000_000;
const MAX_ISSUES: usize = 20;

#[derive(Clone, Serialize)]
pub struct InvalidLink {
    pub line: usize,
    pub url: String,
    pub reason: String,
}

pub struct LinkPreflight {
    pub links: Vec<CanonicalResource>,
    pub invalid: Vec<InvalidLink>,
    pub invalid_count: usize,
    pub duplicates: usize,
}

impl LinkPreflight {
    pub fn valid(&self) -> bool {
        !self.links.is_empty() && self.invalid_count == 0
    }
}

/// Pure syntax/identity inspection. Catalog access and list mutation happen at submission.
pub fn preflight_links(text: &str) -> LinkPreflight {
    let mut result = LinkPreflight {
        links: Vec::new(),
        invalid: Vec::new(),
        invalid_count: 0,
        duplicates: 0,
    };
    let mut identities = HashSet::<ResourceKey>::new();
    let mut line = 1;
    let mut last_was_cr = false;
    let mut token_start = None;
    let mut token_line = 1;
    let mut inspect = |token: &str, line| match normalize_link(token) {
        Ok(resource) => {
            if !identities.insert(resource.key.clone()) {
                result.duplicates += 1;
            }
            result.links.push(resource);
        }
        Err(error) => {
            result.invalid_count += 1;
            if result.invalid.len() < MAX_ISSUES {
                result.invalid.push(InvalidLink {
                    line,
                    url: token.chars().take(240).collect(),
                    reason: error.message,
                });
            }
        }
    };
    for (offset, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = token_start.take() {
                inspect(&text[start..offset], token_line);
            }
        } else if token_start.is_none() {
            token_start = Some(offset);
            token_line = line;
        }
        if character == '\r' || character == '\n' && !last_was_cr {
            line += 1;
        }
        last_was_cr = character == '\r';
    }
    if let Some(start) = token_start {
        inspect(&text[start..], token_line);
    }
    result
}

pub fn validate_import_snapshot(text: &str) -> Result<LinkPreflight> {
    if text.len() > MAX_LINK_BYTES || text.chars().any(|c| c.is_control() && !c.is_whitespace()) {
        return Err(import_error(
            "The file must contain only UTF-8 Qobuz URLs and whitespace, within 2,000,000 bytes.",
        ));
    }
    let parsed = preflight_links(text);
    if !parsed.valid() {
        return Err(import_error(
            "Every file entry must be a supported Qobuz URL, with at least one URL.",
        ));
    }
    Ok(parsed)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub valid: bool,
    pub filename: String,
    pub size_bytes: u64,
    pub encoding: &'static str,
    pub content: Option<String>,
    pub total_urls: usize,
    pub unique_urls: usize,
    pub duplicates: usize,
    pub invalid_count: usize,
    pub invalid: Vec<InvalidLink>,
    pub reason: Option<String>,
}

impl ImportReport {
    fn rejected(filename: String, size_bytes: u64, reason: &str) -> Self {
        Self {
            valid: false,
            filename,
            size_bytes,
            encoding: "UTF-8",
            content: None,
            total_urls: 0,
            unique_urls: 0,
            duplicates: 0,
            invalid_count: 0,
            invalid: Vec::new(),
            reason: Some(reason.to_owned()),
        }
    }
}

pub fn inspect_link_file(path: &Path) -> ImportReport {
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rejected = |size, reason| ImportReport::rejected(filename.clone(), size, reason);
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
    {
        return rejected(0, "Choose one .txt file.");
    }
    // Inspect before opening and inspect the opened handle too. Reparse points and special
    // files cannot turn a drag/drop path into an unbounded device or pipe read.
    if !fs::symlink_metadata(path).is_ok_and(|info| info.file_type().is_file()) {
        return rejected(
            0,
            "Choose a regular .txt file. Folders and links are unsupported.",
        );
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let Ok(file) = options.open(path) else {
        return rejected(
            0,
            "The file could not be opened. Check access and choose it again.",
        );
    };
    let Ok(info) = file.metadata() else {
        return rejected(0, "The file could not be inspected.");
    };
    if !info.file_type().is_file() {
        return rejected(info.len(), "Choose a regular .txt file.");
    }
    if info.len() > MAX_LINK_BYTES as u64 {
        return rejected(info.len(), "The file exceeds 2,000,000 bytes.");
    }
    let mut bytes = Vec::with_capacity((info.len() as usize).min(MAX_LINK_BYTES));
    if file
        .take(MAX_LINK_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return rejected(
            0,
            "The file could not be read. The text draft is still available.",
        );
    }
    let raw_size = bytes.len() as u64;
    if bytes.len() > MAX_LINK_BYTES {
        return rejected(raw_size, "The file exceeds 2,000,000 bytes.");
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return rejected(
            raw_size,
            "Use UTF-8 text. UTF-16, binary data, and malformed UTF-8 are unsupported.",
        );
    };
    if text.chars().any(|c| c.is_control() && !c.is_whitespace()) {
        return rejected(raw_size, "The file contains binary or control characters.");
    }
    let parsed = preflight_links(text);
    let valid = parsed.valid();
    let total_urls = parsed.links.len();
    ImportReport {
        valid,
        filename,
        size_bytes: raw_size,
        encoding: "UTF-8",
        content: valid.then(|| text.to_owned()),
        total_urls,
        unique_urls: total_urls - parsed.duplicates,
        duplicates: parsed.duplicates,
        invalid_count: parsed.invalid_count,
        invalid: parsed.invalid,
        reason: (!valid).then(|| {
            if total_urls == 0 && parsed.invalid_count == 0 {
                "The file must contain at least one supported Qobuz URL.".into()
            } else {
                "The file was rejected. Every entry must be a supported Qobuz URL.".into()
            }
        }),
    }
}

fn import_error(message: &str) -> AppError {
    AppError::new("link_import", "links", message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplied_round4_bytes_and_boundaries_are_read_through_regular_files() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/import-fixtures.json"
        ))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        for fixture in fixtures["cases"].as_array().unwrap() {
            let path = directory.path().join(fixture["name"].as_str().unwrap());
            let bytes = if let Some(text) = fixture["text"].as_str() {
                text.as_bytes().to_vec()
            } else {
                hex::decode(fixture["hex"].as_str().unwrap()).unwrap()
            };
            assert_eq!(bytes.len() as u64, fixture["bytes"].as_u64().unwrap());
            fs::write(&path, bytes).unwrap();
            let report = inspect_link_file(&path);
            assert_eq!(
                report.valid,
                fixture["valid"].as_bool().unwrap(),
                "{}",
                fixture["id"]
            );
            if let Some(unique) = fixture["unique"].as_u64() {
                assert_eq!(report.unique_urls as u64, unique);
            }
            if let Some(lines) = fixture["invalidLines"].as_array() {
                assert_eq!(
                    report
                        .invalid
                        .iter()
                        .map(|entry| entry.line as u64)
                        .collect::<Vec<_>>(),
                    lines
                        .iter()
                        .map(|line| line.as_u64().unwrap())
                        .collect::<Vec<_>>()
                );
            }
            if !report.valid {
                assert!(report.content.is_none());
            }
        }
        let path = directory.path().join("boundary.TXT");
        let mut bytes = b"https://play.qobuz.com/album/r4-limit".to_vec();
        bytes.resize(MAX_LINK_BYTES, b' ');
        fs::write(&path, &bytes).unwrap();
        let report = inspect_link_file(&path);
        assert!(report.valid);
        assert_eq!(report.size_bytes, 2_000_000);
        bytes.push(b' ');
        fs::write(&path, &bytes).unwrap();
        assert!(!inspect_link_file(&path).valid);
        let folder = directory.path().join("folder.txt");
        fs::create_dir(folder.clone()).unwrap();
        assert!(!inspect_link_file(&folder).valid);
        assert!(!inspect_link_file(&directory.path().join("removed.txt")).valid);
        let mut bom_bytes = vec![0xef, 0xbb, 0xbf];
        bom_bytes.extend_from_slice(b"https://play.qobuz.com/album/r4-limit");
        bom_bytes.resize(MAX_LINK_BYTES, b' ');
        fs::write(&path, &bom_bytes).unwrap();
        assert!(inspect_link_file(&path).valid);
        bom_bytes.push(b' ');
        fs::write(&path, &bom_bytes).unwrap();
        assert!(!inspect_link_file(&path).valid);
    }

    #[test]
    fn diagnostics_are_bounded_and_line_numbers_handle_all_newlines() {
        let parsed = preflight_links("bad\rbad\r\nbad\nbad");
        assert_eq!(
            parsed
                .invalid
                .iter()
                .map(|issue| issue.line)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let parsed = preflight_links(&"bad\n".repeat(30));
        assert_eq!(parsed.invalid_count, 30);
        assert_eq!(parsed.invalid.len(), 20);
        assert!(
            !preflight_links(
                "https://play.qobuz.com/album/a\u{feff}https://play.qobuz.com/album/b"
            )
            .valid()
        );
        assert!(
            preflight_links("https://play.qobuz.com/album/a\u{0085}https://play.qobuz.com/album/b")
                .valid()
        );
    }
}
