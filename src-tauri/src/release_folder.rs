use crate::error::{AppError, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, PartialEq, Eq)]
pub struct Evidence {
    pub path: PathBuf,
    pub root: PathBuf,
}

pub struct VerifiedFolder {
    pub path: PathBuf,
    pub evidence: Evidence,
}

fn unavailable(message: &str) -> AppError {
    AppError::invalid("queue", message)
}

/// Each folder needs its own verified saved file and recorded layout.
fn resolve(evidence: Vec<Evidence>) -> Vec<VerifiedFolder> {
    let mut roots = BTreeMap::new();
    for evidence in evidence {
        let Some(root) = verify(&evidence) else {
            continue;
        };
        let key = if cfg!(windows) {
            root.to_string_lossy().to_lowercase()
        } else {
            root.to_string_lossy().into_owned()
        };
        roots.entry(key).or_insert(VerifiedFolder {
            path: root,
            evidence,
        });
    }
    roots.into_values().collect()
}

fn verify(evidence: &Evidence) -> Option<PathBuf> {
    let root = dunce::canonicalize(&evidence.root).ok()?;
    let file = dunce::canonicalize(&evidence.path).ok()?;
    (root.is_dir() && file.is_file() && file.starts_with(&root)).then_some(root)
}

pub fn select(evidence: Vec<Evidence>) -> Result<VerifiedFolder> {
    let folders = resolve(evidence);
    let [folder] = folders.try_into().map_err(|folders: Vec<VerifiedFolder>| {
        if folders.is_empty() {
            unavailable("No verified output folder is available for this release.")
        } else {
            unavailable("This row contains multiple saved release folders.")
        }
    })?;
    Ok(folder)
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
            evidence.push(Evidence {
                path: file,
                root: album.clone(),
            });
        }
        assert_eq!(
            select(evidence.clone()).unwrap().path,
            dunce::canonicalize(&album).unwrap()
        );
        assert_eq!(
            select(vec![evidence[3].clone()]).unwrap().path,
            dunce::canonicalize(&album).unwrap()
        );
        assert_eq!(
            select(vec![evidence[2].clone()]).unwrap().path,
            dunce::canonicalize(&album).unwrap()
        );
        let other = temp.path().join("Another album");
        std::fs::create_dir(&other).unwrap();
        let file = other.join("track.mp3");
        std::fs::write(&file, b"saved").unwrap();
        evidence.push(Evidence {
            path: file.clone(),
            root: other.clone(),
        });
        assert_eq!(resolve(evidence.clone()).len(), 2);
        assert_eq!(
            select(vec![evidence[6].clone()]).unwrap().path,
            dunce::canonicalize(other).unwrap()
        );
        assert!(select(evidence).is_err());
        assert!(select(vec![]).is_err());
        for (path, root) in [
            (album.join("absent"), album.clone()),
            (album.join("cover.jpg"), temp.path().join("disconnected")),
            (file, album),
        ] {
            assert!(resolve(vec![Evidence { path, root }]).is_empty());
        }
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
