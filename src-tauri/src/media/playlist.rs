use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use tokio_util::sync::CancellationToken;

use super::{
    Media, MediaError, existing_root,
    output_store::{CheckCancelled, Receipt},
    template::sanitize,
};

impl Media {
    pub async fn write_playlist(
        &self,
        title: &str,
        playlist_id: &str,
        destination: &Path,
        recorded_path: Option<&Path>,
        ordered_outputs: &[PathBuf],
        safe_mode: bool,
        cancel: &CancellationToken,
    ) -> Result<PathBuf, MediaError> {
        cancel.check()?;
        let safe_id = sanitize(playlist_id, safe_mode);
        if safe_id.is_empty() || safe_id != playlist_id {
            return Err(MediaError::new(
                "playlist_id",
                "Qobuz did not provide a valid playlist ID.",
                false,
            ));
        }
        let safe_title = sanitize(title, safe_mode);
        let safe_title = if safe_title.is_empty() {
            "Playlist"
        } else {
            &safe_title
        };
        let prefix = safe_title.floor_char_boundary(220_usize.saturating_sub(safe_id.len() + 3));
        let name = format!("{} ({safe_id}).m3u", safe_title[..prefix].trim_end());
        let (directory, path) = if let Some(recorded) = recorded_path {
            let directory = recorded.parent().ok_or_else(|| {
                MediaError::new(
                    "playlist_location",
                    "The recorded playlist path has no containing folder.",
                    false,
                )
            })?;
            let directory = existing_root(directory).await?;
            let name = recorded.file_name().ok_or_else(|| {
                MediaError::new(
                    "playlist_location",
                    "The recorded playlist filename is invalid.",
                    false,
                )
            })?;
            (directory.clone(), directory.join(name))
        } else {
            let directory = existing_root(destination).await?;
            (directory.clone(), directory.join(name))
        };
        let _guard = self.lock_path(&path, cancel).await?;
        let mut verified = HashMap::new();
        let mut body = String::from("#EXTM3U\n");
        let mut entries = 0;
        for output in ordered_outputs {
            cancel.check()?;
            let is_verified = if let Some(&verified) = verified.get(output) {
                verified
            } else {
                let result = self.store.verified_audio(output, cancel).await?;
                verified.insert(output, result);
                result
            };
            if !is_verified {
                continue;
            }
            let Some(entry) = playlist_entry(output, &directory) else {
                continue;
            };
            body.push_str(&entry);
            body.push('\n');
            entries += 1;
        }
        if entries == 0 {
            return Err(MediaError::new(
                "playlist_empty",
                "No verified audio is available for this playlist.",
                true,
            )
            .at_path(&path));
        }
        let stage = self
            .store
            .stage(&directory, ".m3u")
            .await
            .map_err(|error| error.at_path(&path))?;
        if let Err(error) = tokio::fs::write(stage.path(), body.as_bytes()).await {
            self.store.discard(stage).await;
            return Err(MediaError::disk(error).at_path(&path));
        }
        let receipt = Receipt {
            kind: "playlist",
            identity: playlist_id.to_owned(),
            variant: "m3u".into(),
        };
        self.store
            .publish_playlist(stage, &path, &receipt, recorded_path.is_some(), cancel)
            .await?;
        Ok(path)
    }
}

fn playlist_entry(path: &Path, directory: &Path) -> Option<String> {
    let path = path.strip_prefix(directory).unwrap_or(path);
    let entry = path.to_str()?;
    if entry.is_empty() || entry.contains(['\r', '\n']) {
        return None;
    }
    if let Some(network) = entry.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{network}"));
    }
    if let Some(plain) = entry.strip_prefix(r"\\?\") {
        return Some(plain.to_owned());
    }
    if entry.starts_with('#') {
        return Some(format!("./{entry}"));
    }
    Some(entry.to_owned())
}

#[cfg(test)]
mod tests {
    use super::super::output_store::Receipt;
    use super::*;

    #[tokio::test]
    async fn playlist_preserves_occurrences_and_refreshes_only_verified_exports() {
        let root = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(root.path()).unwrap();
        let database = directory.join("files.sqlite3");
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(super::super::RECEIPT_SCHEMA)
            .unwrap();
        let media = Media::new(reqwest::Client::new(), database, directory.clone());
        let cancel = CancellationToken::new();
        let first = directory.join("first.flac");
        let second = directory.join("second.flac");
        for (path, id) in [(&first, "1"), (&second, "2")] {
            let stage = media.store.stage(&directory, ".flac").await.unwrap();
            tokio::fs::write(stage.path(), format!("verified fixture {id}"))
                .await
                .unwrap();
            media
                .store
                .publish(
                    stage,
                    path,
                    &Receipt {
                        kind: "audio",
                        identity: id.into(),
                        variant: "fixture".into(),
                    },
                    &cancel,
                )
                .await
                .unwrap();
        }
        let missing = directory.join("missing.flac");
        let path = media
            .write_playlist(
                "Late Night Jazz",
                "12345",
                &directory,
                None,
                &[first.clone(), missing, first.clone()],
                true,
                &cancel,
            )
            .await
            .unwrap();
        assert!(path.ends_with("Late Night Jazz (12345).m3u"));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "#EXTM3U\nfirst.flac\nfirst.flac\n"
        );
        let fresh = vec![first.clone(), second.clone(), first];
        let conflict = media
            .write_playlist(
                "Late Night Jazz",
                "12345",
                &directory,
                None,
                &fresh,
                true,
                &cancel,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(conflict.code, "output_conflict");
        assert_eq!(conflict.path.as_ref(), Some(&path));
        media
            .write_playlist(
                "Updated title",
                "12345",
                &directory,
                Some(&path),
                &fresh,
                true,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "#EXTM3U\nfirst.flac\nsecond.flac\nfirst.flac\n"
        );
        tokio::fs::write(&path, "user edits\n").await.unwrap();
        assert_eq!(
            media
                .write_playlist(
                    "Late Night Jazz",
                    "12345",
                    &directory,
                    Some(&path),
                    &fresh,
                    true,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .code,
            "output_conflict"
        );
        assert_eq!(
            tokio::fs::read_to_string(path).await.unwrap(),
            "user edits\n"
        );
        let on = media
            .write_playlist(
                "Late: Jazz",
                "4321",
                &directory,
                None,
                &fresh,
                true,
                &cancel,
            )
            .await
            .unwrap();
        assert!(on.ends_with("Late Jazz (4321).m3u"));
        let off = media
            .write_playlist(
                "Late: Jazz",
                "4321",
                &directory,
                None,
                &fresh,
                false,
                &cancel,
            )
            .await
            .unwrap();
        assert!(off.ends_with("Late_ Jazz (4321).m3u"));
    }
}
