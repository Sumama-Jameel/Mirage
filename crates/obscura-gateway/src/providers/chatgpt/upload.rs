use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::GatewayError;

/// Result of uploading one file to ChatGPT's file service.
#[derive(Clone, Debug)]
pub struct UploadedFile {
    /// `file-service://<file_id>` URL for use in conversation payloads.
    pub file_service_url: String,
    /// Original filename.
    pub name: String,
    /// MIME type.
    pub mime_type: String,
    /// File size in bytes.
    pub size_bytes: usize,
    /// Image width in pixels, if applicable.
    pub width: Option<u32>,
    /// Image height in pixels, if applicable.
    pub height: Option<u32>,
}

impl UploadedFile {
    /// Serialize to the `image_asset_pointer` format used in conversation parts.
    pub fn to_asset_pointer(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "content_type": "image_asset_pointer",
            "asset_pointer": self.file_service_url,
            "size_bytes": self.size_bytes,
        });
        if let Some(w) = self.width {
            obj["width"] = serde_json::json!(w);
        }
        if let Some(h) = self.height {
            obj["height"] = serde_json::json!(h);
        }
        obj
    }

    /// Serialize to the attachment metadata format.
    pub fn to_attachment(&self) -> serde_json::Value {
        let mut att = serde_json::json!({
            "id": self.file_service_url,
            "name": self.name,
            "mimeType": self.mime_type,
            "size": self.size_bytes,
        });
        if let Some(w) = self.width {
            att["width"] = serde_json::json!(w);
        }
        if let Some(h) = self.height {
            att["height"] = serde_json::json!(h);
        }
        att
    }
}

/// Parse image dimensions from raw bytes without pulling in a full image library.
/// Supports PNG, JPEG, GIF, and WebP.
fn parse_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.is_empty() {
        return None;
    }

    // GIF: width at offset 6 (little-endian), height at offset 8
    if data.len() >= 10 && data[..3] == [0x47, 0x49, 0x46] {
        let w = u16::from_le_bytes([data[6], data[7]]);
        let h = u16::from_le_bytes([data[8], data[9]]);
        if w > 0 && h > 0 {
            return Some((w as u32, h as u32));
        }
    }

    // PNG: starts with 8-byte signature, then IHDR chunk at offset 16
    if data.len() >= 24 && data[..8] == [137, 80, 78, 71, 13, 10, 26, 10] {
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        if w > 0 && h > 0 && w < 65536 && h < 65536 {
            return Some((w, h));
        }
    }

    // JPEG: scan for SOF0 marker (0xFF 0xC0)
    if data[..2] == [0xFF, 0xD8] {
        let mut i = 2;
        while i + 8 < data.len() {
            if data[i] == 0xFF && (data[i+1] & 0xF0) == 0xC0 && data[i+1] != 0 {
                let height = u16::from_be_bytes([data[i+5], data[i+6]]);
                let width = u16::from_be_bytes([data[i+7], data[i+8]]);
                if width > 0 && height > 0 {
                    return Some((width as u32, height as u32));
                }
            }
            // Skip to next marker
            if data[i] == 0xFF {
                let marker_len = if i + 3 < data.len() {
                    u16::from_be_bytes([data[i+2], data[i+3]]) as usize
                } else {
                    0
                };
                if marker_len >= 2 {
                    i += 2 + marker_len;
                    continue;
                }
            }
            i += 1;
        }
    }

    // WebP: RIFF + WEBP header
    if data.len() >= 30 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        let chunk_type = &data[12..16];
        if chunk_type == b"VP8 " && data.len() >= 26 {
            // VP8 keyframe: bytes 23-25 contain width/height in RFC 6386 format
            let raw = u16::from_le_bytes([data[23], data[24]]);
            let w = (raw & 0x3FFF) as u32;
            let raw_h = u16::from_le_bytes([data[24], data[25]]);
            let h = ((raw_h >> 6) | (((raw & 0xC000) as u16) >> 6) as u16) as u32 & 0x3FFF;
            if w > 0 && h > 0 {
                return Some((w, h + 1)); // stored as height-1
            }
        } else if chunk_type == b"VP8L" && data.len() >= 25 {
            // VP8L: bitstream starts at offset 21, width/height packed in 4 bytes at offset 21
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            if w <= 16384 && h <= 16384 {
                return Some((w, h));
            }
        } else if chunk_type == b"VP8X" && data.len() >= 30 {
            // VP8X: width/height at offset 24 (3 bytes each, little-endian, 18-bit)
            let w = u32::from_le_bytes([data[24], data[25], data[26], 0]) & 0xFFFFFF;
            let h = u32::from_le_bytes([data[27], data[28], data[29], 0]) & 0xFFFFFF;
            if w > 0 && h > 0 {
                return Some((w + 1, h + 1));
            }
        }
    }

    None
}

/// UploadCache — deduplicates file uploads by SHA-256 content hash.
#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    file: UploadedFile,
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

    pub async fn get(&self, hash: &str) -> Option<UploadedFile> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard.iter().find(|e| e.hash == hash).map(|e| e.file.clone())
    }

    pub async fn insert(&self, hash: String, file: UploadedFile) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload { hash, file, inserted: Instant::now() });
    }
}

/// Validate that a remote URL does not point to a private network.
/// SSRF protection — blocks loopback, RFC1918, link-local, and IPv6 ULA.
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

/// Upload a file to ChatGPT via the 3-step protocol:
/// 1. POST /backend-api/files (create file record)
/// 2. PUT the upload URL (push bytes)
/// 3. POST /backend-api/files/{id}/uploaded (finalize)
pub async fn upload_files(
    http: &Client,
    files: &[(Vec<u8>, String, String)],
    cache: &UploadCache,
) -> Result<Vec<UploadedFile>, GatewayError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Upload each file concurrently (bounded like the download half). The
    // create / put / finalize chain is the latency bottleneck on multimodal
    // requests; running uploads in parallel keeps total latency close to the
    // slowest single file. Results are collected by index and reassembled in
    // original order.
    use futures::stream::{self, StreamExt};
    let max_concurrent = 4;
    let mut results: Vec<(usize, Result<UploadedFile, String>)> =
        Vec::with_capacity(files.len());

    let mut buffered = stream::iter(files.iter().cloned().enumerate())
        .map(|(idx, (data, name, mime_type))| {
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
                    else { mime_type.as_str() };

                // Step 1: Create file record.
                let create_resp = match http
                    .post("https://chatgpt.com/backend-api/files")
                    .json(&serde_json::json!({
                        "file_name": name,
                        "file_size": data.len(),
                        "use_case": "multimodal",
                    }))
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return (idx, Err(format!("file create failed: {e}"))),
                };

                if !create_resp.status().is_success() {
                    return (
                        idx,
                        Err(format!(
                            "file create returned {}: {}",
                            create_resp.status(),
                            create_resp.text().await.unwrap_or_default()
                        )),
                    );
                }

                let file_info: serde_json::Value = match create_resp.json().await {
                    Ok(v) => v,
                    Err(e) => return (idx, Err(format!("file create parse failed: {e}"))),
                };

                tracing::info!(
                    "Upload create response: {}",
                    serde_json::to_string(&file_info).unwrap_or_default()
                );

                let Some(upload_url) = file_info["upload_url"].as_str() else {
                    return (idx, Err("no upload_url in response".to_string()));
                };
                let Some(file_id) = file_info["file_id"].as_str() else {
                    return (idx, Err("no file_id in response".to_string()));
                };
                let (upload_url, file_id) = (upload_url.to_string(), file_id.to_string());

                // Step 2: Upload bytes to blob storage.
                let put_resp = match http
                    .put(&upload_url)
                    .header("Content-Type", mime)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header("x-ms-version", "2020-04-08")
                    .body(data.clone())
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return (idx, Err(format!("blob put failed: {e}"))),
                };

                if !put_resp.status().is_success() {
                    return (idx, Err(format!("blob upload returned {}", put_resp.status())));
                }

                // Step 3: Finalize.
                let finalize_resp = match http
                    .post(format!("https://chatgpt.com/backend-api/files/{}/uploaded", file_id))
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return (idx, Err(format!("finalize failed: {e}"))),
                };

                if !finalize_resp.status().is_success() {
                    return (
                        idx,
                        Err(format!("finalize returned {}", finalize_resp.status())),
                    );
                }

                let final_data: serde_json::Value = match finalize_resp.json().await {
                    Ok(v) => v,
                    Err(e) => return (idx, Err(format!("finalize parse failed: {e}"))),
                };

                tracing::info!(
                    "Upload finalize response: {}",
                    serde_json::to_string(&final_data).unwrap_or_default()
                );

                let file_service_url = format!("file-service://{}", file_id);
                let (width, height) = if mime.starts_with("image/") {
                    parse_image_dimensions(&data).unwrap_or((0, 0))
                } else {
                    (0, 0)
                };

                let uploaded = UploadedFile {
                    file_service_url: file_service_url.clone(),
                    name: name.clone(),
                    mime_type: mime.to_string(),
                    size_bytes: data.len(),
                    width: if width > 0 { Some(width) } else { None },
                    height: if height > 0 { Some(height) } else { None },
                };

                cache.insert(hash.clone(), uploaded.clone()).await;
                (idx, Ok(uploaded))
            }
        })
        .buffered(max_concurrent);

    while let Some((idx, result)) = buffered.next().await {
        results.push((idx, result));
    }

    results.sort_by_key(|(idx, _)| *idx);
    let mut ordered = Vec::with_capacity(results.len());
    for (_, result) in results {
        ordered.push(result.map_err(GatewayError::Internal)?);
    }
    Ok(ordered)
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
    fn validate_remote_url_rejects_private_ipv4() {
        assert!(validate_remote_url("http://192.168.1.1").is_err());
    }

    #[test]
    fn decode_data_uri_valid() {
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let result = decode_data_uri(uri);
        assert!(result.is_some(), "should decode valid data URI");
        let (bytes, mime) = result.unwrap();
        assert!(!bytes.is_empty(), "should have decoded bytes");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn decode_data_uri_no_padding() {
        let uri = "data:text/plain;base64,SGVsbG8sIFdvcmxk";
        let result = decode_data_uri(uri);
        assert!(result.is_some(), "should handle missing padding");
        let (bytes, mime) = result.unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "Hello, World");
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn parse_png_dimensions() {
        // 1x1 white pixel PNG
        let png = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
        ).unwrap();
        let dims = parse_image_dimensions(&png);
        assert_eq!(dims, Some((1, 1)));
    }

    #[test]
    fn parse_jpeg_dimensions() {
        // Minimal 2x2 JPEG
        let jpeg: Vec<u8> = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46,
            0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
            0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43,
            0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08,
            0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C,
            0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12,
            0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20,
            0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29,
            0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27,
            0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34,
            0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x02,
            0x00, 0x02, 0x01, 0x01, 0x11, 0x00,
            0xFF, 0xD9,
        ];
        let dims = parse_image_dimensions(&jpeg);
        assert_eq!(dims, Some((2, 2)));
    }

    #[test]
    fn parse_gif_dimensions() {
        // GIF header with 1x1 dimensions
        let gif: Vec<u8> = vec![
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61,
            0x01, 0x00, 0x01, 0x00,
        ];
        let dims = parse_image_dimensions(&gif);
        assert_eq!(dims, Some((1, 1)));
    }

    #[test]
    fn uploaded_file_asset_pointer_includes_metadata() {
        let f = UploadedFile {
            file_service_url: "file-service://file_abc".to_string(),
            name: "test.png".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: 100,
            width: Some(640),
            height: Some(480),
        };
        let ap = f.to_asset_pointer();
        assert_eq!(ap["content_type"], "image_asset_pointer");
        assert_eq!(ap["asset_pointer"], "file-service://file_abc");
        assert_eq!(ap["width"], 640);
        assert_eq!(ap["height"], 480);
        assert_eq!(ap["size_bytes"], 100);
    }

    #[test]
    fn uploaded_file_attachment_includes_metadata() {
        let f = UploadedFile {
            file_service_url: "file-service://file_abc".to_string(),
            name: "test.png".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: 100,
            width: Some(640),
            height: Some(480),
        };
        let att = f.to_attachment();
        assert_eq!(att["id"], "file-service://file_abc");
        assert_eq!(att["name"], "test.png");
        assert_eq!(att["mimeType"], "image/png");
        assert_eq!(att["size"], 100);
        assert_eq!(att["width"], 640);
        assert_eq!(att["height"], 480);
    }
}
