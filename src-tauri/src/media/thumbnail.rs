use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use url::Url;

use super::{Media, assets::image_data_info};

const MAX_IMAGE_BYTES: usize = 512 * 1024;
const MAX_CACHED_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;
const DEADLINE: Duration = Duration::from_secs(10);

struct CachedImage {
    key: [u8; 32],
    image: Option<String>,
    expires: Instant,
}

pub(super) struct ThumbnailCache {
    entries: Mutex<VecDeque<CachedImage>>,
    requests: Semaphore,
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            requests: Semaphore::new(2),
        }
    }
}

impl ThumbnailCache {
    fn get(&self, key: &[u8; 32]) -> Option<Option<String>> {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|entry| entry.expires > Instant::now());
        let index = entries.iter().position(|entry| &entry.key == key)?;
        let entry = entries.remove(index)?;
        let image = entry.image.clone();
        entries.push_back(entry);
        Some(image)
    }

    fn put(&self, key: [u8; 32], image: Option<String>) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|entry| entry.key != key && entry.expires > Instant::now());
        let expires = Instant::now()
            + if image.is_some() {
                Duration::from_secs(3_600)
            } else {
                Duration::from_secs(30)
            };
        entries.push_back(CachedImage {
            key,
            image,
            expires,
        });
        let mut bytes = entries
            .iter()
            .filter_map(|entry| entry.image.as_ref())
            .map(String::len)
            .sum::<usize>();
        while entries.len() > MAX_ENTRIES || bytes > MAX_CACHED_BYTES {
            let Some(removed) = entries.pop_front() else {
                break;
            };
            bytes -= removed.image.map_or(0, |image| image.len());
        }
    }
}

fn source_url(source: &str) -> Option<Url> {
    if source.len() > 4_096 {
        return None;
    }
    let url = Url::parse(source).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn data_uri(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let image = image_data_info(bytes).ok()?;
    if image
        .dimensions
        .is_none_or(|(width, height)| width.max(height) > 1_200)
    {
        return None;
    }
    let mime = match image.extension {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

impl Media {
    /// The webview only receives a bounded image data URI, never its source URL.
    pub async fn thumbnail(&self, source: &str) -> Option<String> {
        let url = source_url(source)?;
        let key: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        if let Some(image) = self.thumbnails.get(&key) {
            return image;
        }
        let image = tokio::time::timeout(DEADLINE, async {
            let _permit = self.thumbnails.requests.acquire().await.ok()?;
            if let Some(image) = self.thumbnails.get(&key) {
                return image;
            }
            self.fetch_thumbnail(url).await
        })
        .await
        .ok()
        .flatten();
        self.thumbnails.put(key, image.clone());
        image
    }

    async fn fetch_thumbnail(&self, mut url: Url) -> Option<String> {
        for _ in 0..=5 {
            let mut response = self
                .client
                .get(url.clone())
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .ok()?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)?
                    .to_str()
                    .ok()?;
                let next = response.url().join(location).ok()?;
                url = source_url(next.as_str())?;
                continue;
            }
            if response.status() != reqwest::StatusCode::OK
                || response
                    .content_length()
                    .is_some_and(|size| size == 0 || size > MAX_IMAGE_BYTES as u64)
            {
                return None;
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.ok()? {
                if body.len() + chunk.len() > MAX_IMAGE_BYTES {
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            return data_uri(&body);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_are_bounded_data_images_with_fixed_cache_limits() {
        let image = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5u0AAAAASUVORK5CYII=").unwrap();
        assert!(
            data_uri(&image)
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(data_uri(b"<svg onload='unexpected'>").is_none());
        assert!(data_uri(&vec![0; MAX_IMAGE_BYTES + 1]).is_none());
        assert!(source_url("https://user:secret@example.com/image.jpg").is_none());
        assert!(source_url("file:///image.jpg").is_none());
        let cache = ThumbnailCache::default();
        for index in 0..MAX_ENTRIES + 10 {
            let key: [u8; 32] = Sha256::digest(index.to_le_bytes()).into();
            cache.put(key, Some("a".repeat(90_000)));
        }
        let entries = cache.entries.lock().unwrap();
        assert!(entries.len() <= MAX_ENTRIES);
        assert!(
            entries
                .iter()
                .filter_map(|entry| entry.image.as_ref())
                .map(String::len)
                .sum::<usize>()
                <= MAX_CACHED_BYTES
        );
    }
}
