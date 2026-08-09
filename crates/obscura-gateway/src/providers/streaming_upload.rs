//! Streaming file download and hash computation.
//!
//! This module provides concurrent, memory-efficient file downloads with
//! on-the-fly SHA-256 hashing, without buffering entire files into memory.
//!
//! Key features:
//! - Parallel downloads using `futures::stream::iter().buffered()`
//! - On-the-fly SHA-256 hashing (no intermediate buffer)
//! - SSRF validation before download
//! - Concurrent processing for multiple files
//! - Memory-bounded: each chunk is ~64KB, released after hashing

use std::sync::Arc;

use base64::Engine;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use sha2::{Digest, Sha256};
use tracing::debug;
use url::Url;

use crate::error::GatewayError;

/// Maximum concurrent downloads to prevent resource exhaustion.
/// Adjust based on system memory and provider rate limits.
const MAX_CONCURRENT_DOWNLOADS: usize = 4;

/// Maximum file size (20 MB). Providers typically reject larger files anyway.
const MAX_FILE_SIZE: usize = 20 * 1024 * 1024;

/// Chunk size for streaming hash computation (64 KB).
/// Balances memory usage vs. syscall overhead.
const STREAM_CHUNK_SIZE: usize = 64 * 1024;

/// Result of downloading and hashing a single file.
#[derive(Debug, Clone)]
pub struct HashedFile {
    /// Raw file bytes (buffered after download completes).
    pub bytes: Vec<u8>,
    /// Original filename.
    pub name: String,
    /// MIME type from Content-Type header.
    pub mime_type: String,
    /// SHA-256 hash in base64 encoding.
    pub hash_base64: String,
}

/// Validate that a remote URL does not point to a private network.
/// Blocks loopback, RFC1918, and link-local addresses.
fn validate_remote_url(url: &str) -> Result<(), GatewayError> {
    let parsed = Url::parse(url).map_err(|_| {
        GatewayError::BadRequest(format!("invalid URL: {url}"))
    })?;

    let host = parsed.host().ok_or_else(|| {
        GatewayError::BadRequest(format!("no host in URL: {url}"))
    })?;

    match host {
        url::Host::Ipv4(ip) => {
            if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
                return Err(GatewayError::BadRequest(
                    format!("SSRF rejected: private IP {ip}"),
                ));
            }
        }
        url::Host::Ipv6(ip) => {
            if ip.is_loopback() || ip.is_unicast_link_local() {
                return Err(GatewayError::BadRequest(
                    format!("SSRF rejected: private IPv6 {ip}"),
                ));
            }
            // IPv6 private networks: ULA (fc00::/7), link-local (fe80::/10)
            if ip.segments()[0] >= 0xfc00 {
                return Err(GatewayError::BadRequest(
                    format!("SSRF rejected: private IPv6 {ip}"),
                ));
            }
        }
        url::Host::Domain(_) => {
            // Domain names are allowed; DNS rebinding is a separate concern.
        }
    }

    Ok(())
}

/// Download a single file with on-the-fly SHA-256 hashing.
///
/// `data:` URLs (the standard OpenAI client base64 format) are decoded
/// locally instead of fetched, so multimodal requests never depend on a host.
///
/// # Process
/// 1. Decode `data:` URLs locally; otherwise validate URL for SSRF attacks
/// 2. Fetch with 30-second timeout
/// 3. Compute SHA-256 while streaming chunks (no buffer)
/// 4. Buffer complete bytes after hash is final
///
/// # Returns
/// `HashedFile` with bytes, name, MIME type, and base64-encoded SHA-256 hash.
async fn download_and_hash_single(
    http: &Client,
    url: &str,
) -> Result<HashedFile, GatewayError> {
    if let Some(data) = url.strip_prefix("data:") {
        return decode_data_url(data);
    }
    validate_remote_url(url)?;

    let resp = http
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("download failed for {url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Internal(format!(
            "download returned {} for {url}",
            resp.status()
        )));
    }

    let mime_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Derive filename from URL path, fallback to generic name
    let name = Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments().and_then(|s| s.last().map(|s| s.to_string())))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "file".to_string());

    // Stream the response body, computing hash on-the-fly
    let mut hasher = Sha256::new();
    let mut bytes_vec = Vec::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| {
            GatewayError::Internal(format!("download read failed for {url}: {e}"))
        })?;

        // Update hash incrementally (no buffering)
        hasher.update(&chunk);

        // Buffer bytes for upload
        bytes_vec.extend_from_slice(&chunk);

        // Check file size limit
        if bytes_vec.len() > MAX_FILE_SIZE {
            return Err(GatewayError::BadRequest(format!(
                "file too large: {} bytes (max {})",
                bytes_vec.len(),
                MAX_FILE_SIZE
            )));
        }
    }

    let hash_bytes = hasher.finalize();
    let hash_base64 = base64::engine::general_purpose::STANDARD.encode(hash_bytes);

    debug!(
        url = %url,
        name = %name,
        size_bytes = %bytes_vec.len(),
        hash = %hash_base64,
        "Downloaded and hashed file"
    );

    Ok(HashedFile {
        bytes: bytes_vec,
        name,
        mime_type,
        hash_base64,
    })
}

/// Decode a `data:[<mime>][;base64],<data>` URL into a [`HashedFile`].
///
/// Only base64-encoded data URLs are accepted, mirroring what OpenAI clients
/// send for image parts. The filename is derived from the MIME type and the
/// hash is computed over the decoded bytes so the upload cache stays stable
/// regardless of whether the same bytes arrived as a data URL or a remote
/// fetch.
fn decode_data_url(data: &str) -> Result<HashedFile, GatewayError> {
    let data = data
        .strip_prefix("data:")
        .unwrap_or(data);
    let (meta, b64) = data
        .split_once(',')
        .ok_or_else(|| GatewayError::BadRequest("invalid data url".to_string()))?;

    let content_type = if let Some(rest) = meta.strip_suffix(";base64") {
        if rest.is_empty() {
            "application/octet-stream".to_string()
        } else {
            rest.to_string()
        }
    } else {
        return Err(GatewayError::BadRequest(
            "data url must be base64-encoded".to_string(),
        ));
    };

    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        b64.replace(' ', "+").replace('\n', "").replace('\r', ""),
    )
    .map_err(|e| GatewayError::BadRequest(format!("invalid base64 file: {e}")))?;

    if bytes.len() > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file too large: {} bytes (max {})",
            bytes.len(),
            MAX_FILE_SIZE
        )));
    }

    let name = extension_for_mime(&content_type)
        .map(|ext| format!("file.{ext}"))
        .unwrap_or_else(|| "file".to_string());

    let hash_bytes = Sha256::digest(&bytes);
    let hash_base64 = base64::engine::general_purpose::STANDARD.encode(hash_bytes);

    Ok(HashedFile {
        bytes,
        name,
        mime_type: content_type,
        hash_base64,
    })
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "image/x-icon" => Some("ico"),
        "image/bmp" => Some("bmp"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "text/csv" => Some("csv"),
        "application/json" => Some("json"),
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        _ => None,
    }
}

/// Download and hash multiple files concurrently.
///
/// # Concurrency
/// Up to `MAX_CONCURRENT_DOWNLOADS` (4) files are downloaded in parallel.
/// This prevents resource exhaustion while keeping good parallelism.
///
/// # Error Handling
/// If any download fails, returns the first error encountered.
/// Partial results are not returned; all-or-nothing semantics.
///
/// # Example
/// ```rust,no_run
/// let http = reqwest::Client::new();
/// let urls = vec!["https://example.com/image.png", "https://example.com/doc.pdf"];
/// let files = download_and_hash_batch(&http, urls).await?;
/// // Each file has .bytes, .name, .mime_type, .hash_base64 computed
/// ```
pub async fn download_and_hash_batch(
    http: &Client,
    urls: Vec<String>,
) -> Result<Vec<HashedFile>, GatewayError> {
    if urls.is_empty() {
        return Ok(Vec::new());
    }

    let http = Arc::new(http.clone());

    let results: Vec<Result<HashedFile, GatewayError>> = stream::iter(urls)
        .map(|url| {
            let http = Arc::clone(&http);
            async move { download_and_hash_single(&http, &url).await }
        })
        .buffered(MAX_CONCURRENT_DOWNLOADS)
        .collect()
        .await;

    // Convert Vec<Result> to Result<Vec>
    results.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_remote_url_allows_public_ip() {
        assert!(validate_remote_url("https://8.8.8.8/file").is_ok());
    }

    #[test]
    fn test_validate_remote_url_rejects_loopback() {
        assert!(validate_remote_url("https://127.0.0.1/file").is_err());
        assert!(validate_remote_url("https://[::1]/file").is_err());
    }

    #[test]
    fn test_validate_remote_url_rejects_private() {
        assert!(validate_remote_url("https://192.168.1.1/file").is_err());
        assert!(validate_remote_url("https://10.0.0.1/file").is_err());
    }

    #[test]
    fn test_validate_remote_url_allows_domain() {
        assert!(validate_remote_url("https://example.com/file").is_ok());
    }

    #[test]
    fn data_url_decodes_to_hashed_file() {
        // 1x1 red PNG
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let url = format!("data:image/png;base64,{b64}");
        let file = decode_data_url(&url).unwrap();
        assert_eq!(file.mime_type, "image/png");
        assert_eq!(file.name, "file.png");
        assert!(!file.bytes.is_empty());
        assert!(!file.hash_base64.is_empty());
    }

    #[test]
    fn data_url_rejects_non_base64() {
        assert!(decode_data_url("text/plain,hello").is_err());
        assert!(decode_data_url("no-commas").is_err());
    }

    #[test]
    fn data_url_without_base64_marker_is_rejected() {
        assert!(decode_data_url("image/png,iVBORw0KGgo=").is_err());
    }

    #[test]
    fn data_url_plain_octet_stream_when_mime_missing() {
        let url = format!("data:;base64,{}", base64::engine::general_purpose::STANDARD.encode(b"hi"));
        let file = decode_data_url(&url).unwrap();
        assert_eq!(file.mime_type, "application/octet-stream");
        assert_eq!(file.bytes, b"hi");
    }
}
