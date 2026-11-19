use super::{
    Credentials, QobuzClient, QobuzError, ResolvedTrack, bounded_body, media_url, nonempty,
    signed_params, u32_value,
};
use reqwest::StatusCode;
use serde_json::Value;
use std::time::Duration;

const LRCLIB: &str = "https://lrclib.net/api/get";
const LYRICS_TIMEOUT: Duration = Duration::from_secs(12);
const LYRICS_LIMIT: usize = 1024 * 1024;
// Keep the adapter gated until an entitled Qobuz account verifies this endpoint.
const QOBUZ_LYRICS_VERIFIED: bool = false;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LyricsSource {
    Qobuz,
    Lrclib,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lyrics {
    pub text: String,
    pub timed: bool,
    pub source: LyricsSource,
}

impl QobuzClient {
    pub async fn resolve_lyrics(
        &self,
        track: &ResolvedTrack,
        credentials: &Credentials,
    ) -> Result<Option<Lyrics>, QobuzError> {
        if !QOBUZ_LYRICS_VERIFIED {
            return self.lrclib_lyrics(track).await;
        }
        let qobuz = self.qobuz_lyrics(track, credentials).await;
        if let Ok(Some(lyrics)) = qobuz {
            return Ok(Some(lyrics));
        }
        match self.lrclib_lyrics(track).await {
            Ok(Some(lyrics)) => Ok(Some(lyrics)),
            Ok(None) => qobuz,
            Err(error) => Err(error),
        }
    }

    async fn qobuz_lyrics(
        &self,
        track: &ResolvedTrack,
        credentials: &Credentials,
    ) -> Result<Option<Lyrics>, QobuzError> {
        if track.track["has_lyrics"].as_bool() == Some(false) {
            return Ok(None);
        }
        // The mobile lyrics service uses a signed document URL. The document
        // fetch is credential-free and carries no Qobuz API headers.
        let signature = format!("tracklyricsUrltrack_id{}", track.id);
        let response = self
            .request(
                "track/lyricsUrl",
                || {
                    let mut params = signed_params(&signature, &credentials.app_secret);
                    params.extend([
                        ("track_id", track.id.clone()),
                        ("app_id", credentials.app_id.clone()),
                    ]);
                    params
                },
                credentials,
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error)
                if matches!(
                    error.status,
                    Some(StatusCode::NOT_FOUND | StatusCode::FORBIDDEN)
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.error),
        };
        let Some(url) = media_url(&response["lyrics_url"]) else {
            return Ok(None);
        };
        let response = self
            .public
            .get(url)
            .timeout(LYRICS_TIMEOUT)
            .send()
            .await
            .map_err(|_| lyrics_error("Could not download the Qobuz lyrics document."))?;
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
        ) {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(lyrics_error("Qobuz could not supply the lyrics document."));
        }
        let data = bounded_body(response, LYRICS_LIMIT).await?;
        let document = serde_json::from_slice::<Value>(&data)
            .map_err(|_| lyrics_error("Qobuz returned unreadable lyrics."))?;
        Ok(qobuz_document(&document))
    }

    async fn lrclib_lyrics(&self, track: &ResolvedTrack) -> Result<Option<Lyrics>, QobuzError> {
        let Some(artist) = nonempty(&track.track["performer"]["name"])
            .or_else(|| nonempty(&track.album["artist"]["name"]))
        else {
            return Ok(None);
        };
        let Some(title) = nonempty(&track.track["title"]) else {
            return Ok(None);
        };
        let minimal = vec![
            ("artist_name", artist.to_owned()),
            ("track_name", title.to_owned()),
        ];
        let mut precise = minimal.clone();
        if let Some(album) = nonempty(&track.album["title"]) {
            precise.push(("album_name", album.to_owned()));
        }
        let duration = u32_value(&track.track["duration"]).or_else(|| {
            track.track["duration"]
                .as_f64()
                .filter(|duration| (1.0..86400.0).contains(duration))
                .map(|duration| duration as u32)
        });
        if let Some(duration) = duration.filter(|duration| *duration > 0) {
            precise.push(("duration", duration.to_string()));
        }
        let mut attempts = vec![precise];
        if attempts[0] != minimal {
            attempts.push(minimal);
        }
        let mut failure = None;
        for query in attempts {
            match self.lrclib_request(query).await {
                Ok(Some(lyrics)) => return Ok(Some(lyrics)),
                Ok(None) => {}
                Err(error) => failure = Some(error),
            }
        }
        failure.map_or(Ok(None), Err)
    }

    async fn lrclib_request(
        &self,
        query: Vec<(&str, String)>,
    ) -> Result<Option<Lyrics>, QobuzError> {
        let response = self
            .public
            .get(LRCLIB)
            .query(&query)
            .header("User-Agent", concat!("suonelo/", env!("CARGO_PKG_VERSION")))
            .timeout(LYRICS_TIMEOUT)
            .send()
            .await
            .map_err(|_| lyrics_error("Could not connect to LRCLIB."))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(lyrics_error("LRCLIB could not complete the lyrics lookup."));
        }
        let body = bounded_body(response, LYRICS_LIMIT).await?;
        let data = serde_json::from_slice::<Value>(&body)
            .map_err(|_| lyrics_error("LRCLIB returned unreadable lyrics."))?;
        Ok(lyric_fields(&data, LyricsSource::Lrclib))
    }
}

fn lyrics_error(message: &str) -> QobuzError {
    QobuzError::new("lyrics_lookup", message, true)
}

fn lyric_fields(value: &Value, source: LyricsSource) -> Option<Lyrics> {
    [("syncedLyrics", true), ("plainLyrics", false)]
        .iter()
        .find_map(|(field, timed)| {
            nonempty(&value[field]).map(|text| Lyrics {
                text: text.to_owned(),
                timed: *timed,
                source,
            })
        })
}

fn qobuz_document(document: &Value) -> Option<Lyrics> {
    let original = &document["original"];
    let lines: Vec<_> = original["lines"]
        .as_array()?
        .iter()
        .filter_map(|line| nonempty(&line["line"]).map(|text| (text, u32_value(&line["start"]))))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let synced = matches!(original["type"].as_str(), Some("lsync" | "wsync"));
    let timed = synced && lines.iter().all(|(_, start)| start.is_some());
    let text = lines
        .into_iter()
        .map(|(line, start)| {
            if timed && let Some(start) = start {
                let minutes = start / 60_000;
                let seconds = start / 1_000 % 60;
                let centiseconds = start / 10 % 100;
                format!("[{minutes:02}:{seconds:02}.{centiseconds:02}]{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(Lyrics {
        text,
        timed,
        source: LyricsSource::Qobuz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_qobuz_line_timings_to_lrc() {
        let lyrics = qobuz_document(&json!({ "original": { "type": "lsync", "lines": [
            {"line": "First line", "start": 0}, {"line": "Second line", "start": 69_123}
        ] }}))
        .unwrap();
        assert!(lyrics.timed);
        assert_eq!(lyrics.text, "[00:00.00]First line\n[01:09.12]Second line");
        assert_eq!(lyrics.source, LyricsSource::Qobuz);
    }

    #[test]
    fn incomplete_timings_preserve_the_entire_lyric_as_plain_text() {
        let lyrics = qobuz_document(&json!({ "original": { "type": "lsync", "lines": [
            {"line": "First line"}, {"line": "Second line", "start": 69_123}
        ] }}))
        .unwrap();
        assert!(!lyrics.timed);
        assert_eq!(lyrics.text, "First line\nSecond line");
    }

    #[test]
    fn lrclib_prioritizes_nonblank_timed_lyrics() {
        let lyrics = lyric_fields(
            &json!({"syncedLyrics": "[00:00.00]Here", "plainLyrics": "Here"}),
            LyricsSource::Lrclib,
        )
        .unwrap();
        assert!(lyrics.timed);
        assert_eq!(lyrics.text, "[00:00.00]Here");
        let lyrics = lyric_fields(
            &json!({"syncedLyrics": "   ", "plainLyrics": "Here"}),
            LyricsSource::Lrclib,
        )
        .unwrap();
        assert!(!lyrics.timed);
        assert_eq!(lyrics.text, "Here");
    }
}
