use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::GatewayError;

/// UploadCache — deduplicates file uploads by SHA-256 content hash.
#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    file_id: String,
    inserted: Instant,
}

impl UploadCache {
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(3600))
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
            ttl,
        }
    }

    pub async fn get(&self, hash: &str) -> Option<String> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard.iter().find(|e| e.hash == hash).map(|e| e.file_id.clone())
    }

    pub async fn insert(&self, hash: String, file_id: String) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload { hash, file_id, inserted: Instant::now() });
    }
}

/// Validate that a remote URL does not point to a private network.
pub fn validate_remote_url(url: &str) -> Result<(), GatewayError> {
    let parsed = url::Url::parse(url).map_err(|_| {
        GatewayError::BadRequest(format!("invalid URL: {url}"))
    })?;

    let host = parsed.host().ok_or_else(|| {
        GatewayError::BadRequest(format!("no host in URL: {url}"))
    })?;

    match host {
        url::Host::Domain(domain) => {
            if domain == "localhost" || domain == "127.0.0.1" {
                return Err(GatewayError::BadRequest("local addresses are blocked".to_string()));
            }
            Ok(())
        }
        url::Host::Ipv4(ip) => {
            if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
                return Err(GatewayError::BadRequest("private IPv4 addresses are blocked".to_string()));
            }
            Ok(())
        }
        url::Host::Ipv6(ip) => {
            if ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unique_local() {
                return Err(GatewayError::BadRequest("private IPv6 addresses are blocked".to_string()));
            }
            Ok(())
        }
    }
}

/// Upload files to Claude via `POST /api/oauth/file_upload` multipart endpoint.
/// Returns file IDs that can be placed in the `attachments` or `files` array.
pub async fn upload_files(
    http: &Client,
    files: &[(Vec<u8>, String)],
    cache: &UploadCache,
) -> Result<Vec<String>, GatewayError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Upload files concurrently (bounded like the download half). The blob
    // upload is the latency bottleneck; parallelizing keeps total latency
    // close to the slowest single file. Results are collected by index and
    // reassembled in original order so the returned id vector stays
    // order-stable. Duplicate hashes across files are handled by the cache
    // in each per-file task.
    use futures::stream::{self, StreamExt};
    let max_concurrent = 4;
    let mut results: Vec<(usize, Result<String, String>)> = Vec::with_capacity(files.len());

    let mut buffered = stream::iter(files.iter().cloned().enumerate())
        .map(|(idx, (data, name))| {
            let http = http.clone();
            let cache = cache.clone();
            async move {
            let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));

            if let Some(cached) = cache.get(&hash).await {
                return (idx, Ok(cached));
            }

            let mime = if name.ends_with(".png") { "image/png" }
                else if name.ends_with(".jpg") || name.ends_with(".jpeg") { "image/jpeg" }
                else if name.ends_with(".pdf") { "application/pdf" }
                else if name.ends_with(".txt") { "text/plain" }
                else if name.ends_with(".csv") { "text/csv" }
                else if name.ends_with(".json") { "application/json" }
                else { "application/octet-stream" };

            // Multipart upload to /api/oauth/file_upload
            let part = match reqwest::multipart::Part::bytes(data.clone())
                .file_name(name.clone())
                .mime_str(mime)
            {
                Ok(p) => p,
                Err(e) => return (idx, Err(format!("mime parse failed: {e}"))),
            };

            let form = reqwest::multipart::Form::new().part("file", part);

            let upload_resp = match http
                .post("https://claude.ai/api/oauth/file_upload")
                .multipart(form)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => return (idx, Err(format!("file upload failed: {e}"))),
            };

            if !upload_resp.status().is_success() {
                return (
                    idx,
                    Err(format!(
                        "file upload returned {}: {}",
                        upload_resp.status(),
                        upload_resp.text().await.unwrap_or_default()
                    )),
                );
            }

            let file_info: serde_json::Value = match upload_resp.json().await {
                Ok(v) => v,
                Err(e) => return (idx, Err(format!("file upload parse failed: {e}"))),
            };

            let Some(file_id) = file_info["id"]
                .as_str()
                .or_else(|| file_info["file_id"].as_str())
            else {
                return (idx, Err("no id in upload response".to_string()));
            };
            let file_id = file_id.to_string();

            cache.insert(hash.clone(), file_id.clone()).await;
            (idx, Ok(file_id))
        }
        })
        .buffered(max_concurrent);

    while let Some((idx, result)) = buffered.next().await {
        results.push((idx, result));
    }

    results.sort_by_key(|(idx, _)| *idx);
    results
        .into_iter()
        .map(|(_, result)| result.map_err(GatewayError::Internal))
        .collect()
}

/// Download a remote file with SSRF validation.
pub async fn download_remote(http: &Client, url: &str) -> Result<(Vec<u8>, String), GatewayError> {
    validate_remote_url(url)?;
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("download failed for {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(GatewayError::Internal(format!(
            "download returned {status} for {url}"
        )));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Internal(format!("download read failed for {url}: {e}")))?
        .to_vec();
    Ok((bytes, content_type))
}

/// Derive a filename from a URL or MIME type.
pub fn derive_filename(url: &str, mime: &str) -> String {
    if let Some(name) = url.split('/').last().filter(|s| s.contains('.')) {
        return name.to_string();
    }
    match mime {
        m if m.starts_with("image/png") => "image.png".to_string(),
        m if m.starts_with("image/jpeg") || m.starts_with("image/jpg") => "image.jpg".to_string(),
        m if m.starts_with("image/webp") => "image.webp".to_string(),
        m if m.starts_with("image/gif") => "image.gif".to_string(),
        m if m.starts_with("application/pdf") => "document.pdf".to_string(),
        m if m.starts_with("text/") => format!("file.{}", m.split('/').last().unwrap_or("txt")),
        _ => "file.bin".to_string(),
    }
}

/// Decode a data URI into raw bytes and MIME type.
pub fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (mime, encoded) = rest.split_once(',')?;
    let (mime, _) = mime.split_once(';').unwrap_or((mime, ""));
    let mime = mime.to_string();

    let padded = if encoded.len() % 4 != 0 {
        let pad = 4 - (encoded.len() % 4);
        format!("{}{}", encoded, "=".repeat(pad))
    } else {
        encoded.to_string()
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .ok()?;
    Some((bytes, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_remote_url_rejects_localhost() {
        assert!(validate_remote_url("http://localhost:3000").is_err());
    }

    #[test]
    fn validate_remote_url_accepts_public() {
        assert!(validate_remote_url("https://example.com").is_ok());
    }

    #[test]
    fn decode_data_uri_valid() {
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let result = decode_data_uri(uri);
        assert!(result.is_some());
        let (bytes, mime) = result.unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn decode_data_uri_no_padding() {
        let uri = "data:text/plain;base64,SGVsbG8sIFdvcmxk";
        let result = decode_data_uri(uri);
        assert!(result.is_some());
        let (bytes, mime) = result.unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "Hello, World");
        assert_eq!(mime, "text/plain");
    }
}
