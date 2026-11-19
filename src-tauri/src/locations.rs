use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SavedLocation {
    Portable(PathBuf),
    Absolute(PathBuf),
}

impl SavedLocation {
    pub fn new(path: &Path, root: &Path) -> Result<Self> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(AppError::invalid(
                "settings",
                "Choose an absolute path without parent directory segments.",
            ));
        }
        let path = if path.exists() {
            let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            // Resolve links before deciding whether a location travels with the portable folder.
            if canonical.starts_with(root) {
                canonical
            } else {
                path.to_owned()
            }
        } else {
            path.to_owned()
        };
        Ok(match path.strip_prefix(root) {
            Ok(relative) => Self::Portable(relative.to_owned()),
            Err(_) => Self::Absolute(path),
        })
    }

    pub fn resolve(&self, root: &Path) -> PathBuf {
        match self {
            Self::Portable(path) => root.join(path),
            Self::Absolute(path) => path.clone(),
        }
    }
}

#[cfg(windows)]
pub fn music_folder() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Music, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
    unsafe {
        let value = SHGetKnownFolderPath(&FOLDERID_Music, KNOWN_FOLDER_FLAG(0x4000), None).ok()?;
        let path = value.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(value.0.cast()));
        path
    }
}

#[cfg(target_os = "macos")]
pub fn music_folder() -> Option<PathBuf> {
    dirs::audio_dir()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn music_folder() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Music"))
}

#[cfg(target_os = "macos")]
pub struct MacProfile {
    pub root: PathBuf,
    pub qa_name: Option<String>,
}

#[cfg(target_os = "macos")]
pub fn macos_profile(app: &tauri::App<tauri::Wry>) -> Result<MacProfile> {
    use tauri::Manager;

    let app_root = app.path().app_data_dir().map_err(|_| profile_error())?;
    let mut arguments = std::env::args_os().skip(1).filter(|argument| {
        argument == "--qa-profile" || argument.as_encoded_bytes().starts_with(b"--qa-profile=")
    });
    let Some(argument) = arguments.next() else {
        return Ok(MacProfile { root: app_root, qa_name: None });
    };
    let name = argument
        .to_str()
        .and_then(|argument| argument.strip_prefix("--qa-profile="))
        .filter(|name| {
            (1..=40).contains(&name.len())
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                })
        });
    let Some(name) = name.filter(|_| arguments.next().is_none()) else {
        return Err(AppError::invalid(
            "storage",
            "Choose one QA profile name using 1–40 lowercase letters, digits, or hyphens.",
        ));
    };
    let qa_root = app_root.join("qa");
    let root = qa_root.join(name);
    for path in [&app_root, &qa_root, &root, &root.join("data")] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => return Err(profile_error()),
        }
    }
    Ok(MacProfile { root, qa_name: Some(name.to_owned()) })
}

#[cfg(target_os = "macos")]
pub fn profile_error() -> AppError {
    AppError::new(
        "profile_storage",
        "storage",
        "suonelo needs a writable profile folder in Application Support.",
        true,
    )
}
