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
    url: String,
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
        guard.iter().find(|e| e.hash == hash).map(|e| e.url.clone())
    }

    pub async fn insert(&self, hash: String, url: String) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload { hash, url, inserted: Instant::now() });
    }
}

/// Validate that a remote URL does not point to a private network.

/// Upload files to Kimi via the 4-step protocol:
/// 1. POST /api/pre-sign-url (get upload URL)
/// 2. PUT upload URL (push bytes)
/// 3. POST /api/file (register file)
/// 4. POST /api/file/parse_process (trigger parsing)
///
/// Set `is_image = true` for image uploads (uses "image" action/type vs "file").
pub async fn upload_files(
    http: &Client,
    files: &[(Vec<u8>, String)],
    chat_id: &str,
    cache: &UploadCache,
    is_image: bool,
) -> Result<Vec<String>, GatewayError> {
    let mut refs = Vec::with_capacity(files.len());

    // Upload files concurrently (bounded like the download half). The
    // pre-sign / put / register / parse chain is the latency bottleneck;
    // parallelizing keeps total latency close to the slowest single file.
    // Results are collected by index and reassembled in original order.
    use futures::stream::{self, StreamExt};
    let max_concurrent = 4;
    let mut results: Vec<(usize, Result<String, String>)> = Vec::with_capacity(files.len());

    let action = if is_image { "image" } else { "file" };
    let reg_type = if is_image { "image" } else { "file" };
    let chat_id_owned = chat_id.to_string();

    let mut buffered = stream::iter(
        files
            .iter()
            .cloned()
            .enumerate()
            .map(|(idx, (data, name))| {
                let http = http.clone();
                let cache = cache.clone();
                let chat_id = chat_id_owned.clone();
                async move {
                    let hash =
                        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));

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

                    // Step 1: Get pre-signed upload URL
                    let presign_resp = match http
                        .post("https://kimi.moonshot.cn/api/pre-sign-url")
                        .json(&serde_json::json!({
                            "action": action,
                            "name": name,
                        }))
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            return (idx, Err(format!("pre-sign-url failed: {e}")));
                        }
                    };

                    if !presign_resp.status().is_success() {
                        return (
                            idx,
                            Err(format!("pre-sign-url returned {}", presign_resp.status())),
                        );
                    }

                    let presign_data: serde_json::Value = match presign_resp.json().await {
                        Ok(v) => v,
                        Err(e) => return (idx, Err(format!("pre-sign-url parse failed: {e}"))),
                    };

                    let Some(upload_url) = presign_data["url"].as_str() else {
                        return (idx, Err("no url in pre-sign-url response".to_string()));
                    };
                    let Some(object_name) = presign_data["object_name"].as_str() else {
                        return (idx, Err("no object_name in pre-sign-url response".to_string()));
                    };
                    let Some(file_id) = presign_data["file_id"].as_str() else {
                        return (idx, Err("no file_id in pre-sign-url response".to_string()));
                    };
                    let (upload_url, object_name, file_id) = (
                        upload_url.to_string(),
                        object_name.to_string(),
                        file_id.to_string(),
                    );

                    // Step 2: Upload bytes to presigned URL
                    let put_resp = match http
                        .put(&upload_url)
                        .header("Content-Type", mime)
                        .body(data.clone())
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => return (idx, Err(format!("upload put failed: {e}"))),
                    };

                    if !put_resp.status().is_success() {
                        return (
                            idx,
                            Err(format!("upload put returned {}", put_resp.status())),
                        );
                    }

                    // Step 3: Register file
                    let register_resp = match http
                        .post("https://kimi.moonshot.cn/api/file")
                        .json(&serde_json::json!({
                            "type": reg_type,
                            "name": name,
                            "object_name": object_name,
                            "file_id": file_id,
                            "chat_id": chat_id,
                        }))
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => return (idx, Err(format!("file register failed: {e}"))),
                    };

                    if !register_resp.status().is_success() {
                        return (
                            idx,
                            Err(format!("file register returned {}", register_resp.status())),
                        );
                    }

                    // Parse register response to get the file reference ID.
                    let register_data: serde_json::Value = match register_resp.json().await {
                        Ok(v) => v,
                        Err(e) => return (idx, Err(format!("register parse failed: {e}"))),
                    };

                    let file_ref = register_data["id"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| file_id.clone());

                    // Step 4: Start parse process (optional — file is already
                    // uploaded and registered). Failure here means the server
                    // will process the file asynchronously; we should not block.
                    match http
                        .post("https://kimi.moonshot.cn/api/file/parse_process")
                        .json(&serde_json::json!({
                            "ids": [file_id],
                        }))
                        .send()
                        .await
                    {
                        Ok(resp) => {
                            if !resp.status().is_success() {
                                tracing::warn!(
                                    status = resp.status().as_u16(),
                                    file_id = %file_id,
                                    "Kimi parse_process endpoint returned non-success (file will be processed async)"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                file_id = %file_id,
                                "Kimi parse_process request failed (file will be processed async)"
                            );
                        }
                    }

                    cache.insert(hash.clone(), file_ref.clone()).await;
                    (idx, Ok(file_ref))
                }
            }),
    )
    .buffered(max_concurrent);

    while let Some((idx, result)) = buffered.next().await {
        results.push((idx, result));
    }

    results.sort_by_key(|(idx, _)| *idx);
    for (_, result) in results {
        refs.push(result.map_err(GatewayError::Internal)?);
    }

    Ok(refs)
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

/// Download a remote file with SSRF validation.

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

/// Validate that a remote URL does not point to a private network.
#[cfg(test)]
pub fn validate_remote_url(url: &str) -> Result<(), GatewayError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| GatewayError::BadRequest(format!("invalid URL: {url}")))?;
    let host = parsed
        .host()
        .ok_or_else(|| GatewayError::BadRequest(format!("no host in URL: {url}")))?;
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
