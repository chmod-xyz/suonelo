use crate::error::{AppError, Result};
#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::{collections::BTreeMap, path::PathBuf};

fn unavailable(message: &str) -> AppError {
    AppError::invalid("queue", message)
}

/// Only recorded output/layout pairs enter this resolver. Never infer a common ancestor.
pub fn resolve(evidence: Vec<(PathBuf, PathBuf)>) -> Result<PathBuf> {
    if evidence.is_empty() {
        return Err(unavailable(
            "No saved output folder is recorded for this release.",
        ));
    }
    let mut roots = BTreeMap::new();
    for (file, root) in evidence {
        let root = dunce::canonicalize(root)
            .map_err(|_| unavailable("The saved release folder is missing or inaccessible."))?;
        let file = dunce::canonicalize(file)
            .map_err(|_| unavailable("A saved output is missing or inaccessible."))?;
        if !root.is_dir() || !file.is_file() || !file.starts_with(&root) {
            return Err(unavailable(
                "Saved output locations no longer match their release folder.",
            ));
        }
        let key = if cfg!(windows) {
            root.to_string_lossy().to_lowercase()
        } else {
            root.to_string_lossy().into_owned()
        };
        roots.insert(key, root);
    }
    if roots.len() != 1 {
        return Err(unavailable(
            "This release has multiple saved folders; no single release folder is available.",
        ));
    }
    Ok(roots.into_values().next().unwrap())
}

#[cfg(not(target_os = "macos"))]
pub fn open(folder: &Path) -> Result<()> {
    if !folder.is_dir() {
        return Err(unavailable(
            "The saved release folder is missing or inaccessible.",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        let path: Vec<u16> = dunce::simplified(folder)
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();
        let opened = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if opened.0 as isize > 32 {
            return Ok(());
        }
    }
    Err(AppError::new(
        "open_folder",
        "queue",
        "Windows could not open the saved release folder.",
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recorded_roots_cover_multidisc_custom_and_auxiliary_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("Custom album");
        std::fs::create_dir_all(album.join("Disc 1")).unwrap();
        std::fs::create_dir_all(album.join("Disc 2")).unwrap();
        let mut evidence = Vec::new();
        for name in [
            "Disc 1/audio.flac",
            "Disc 2/audio.flac",
            "cover.jpg",
            "booklet.pdf",
            "playlist.m3u8",
            "Disc 1/lyrics.lrc",
        ] {
            let file = album.join(name);
            std::fs::write(&file, b"saved").unwrap();
            evidence.push((file, album.clone()));
        }
        assert_eq!(
            resolve(evidence.clone()).unwrap(),
            dunce::canonicalize(&album).unwrap()
        );
        assert_eq!(
            resolve(vec![evidence[3].clone()]).unwrap(),
            dunce::canonicalize(&album).unwrap()
        );
        assert_eq!(
            resolve(vec![evidence[2].clone()]).unwrap(),
            dunce::canonicalize(&album).unwrap()
        );
        let other = temp.path().join("Another album");
        std::fs::create_dir(&other).unwrap();
        let file = other.join("track.mp3");
        std::fs::write(&file, b"saved").unwrap();
        evidence.push((file, other));
        assert!(resolve(evidence).is_err());
        assert!(resolve(vec![]).is_err());
        assert!(resolve(vec![(album.join("absent"), album.clone())]).is_err());
        let file = album.join("cover.jpg");
        assert!(resolve(vec![(file, temp.path().join("disconnected"))]).is_err());
        // A symlink/junction redirect or unrelated saved file cannot escape a recorded root.
        assert!(resolve(vec![(temp.path().join("Another album/track.mp3"), album)]).is_err());
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore = "opens Explorer; run explicitly in an isolated native smoke session"]
    fn native_explorer_smoke() {
        let root =
            std::env::var_os("SUONELO_FOLDER_SMOKE").expect("set an isolated existing folder");
        open(Path::new(&root)).unwrap();
    }
}
