use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::GatewayError;

use super::statsig::browser_statsig_id;

const GROK_BASE_URL: &str = "https://grok.com";
const UPLOAD_PATH: &str = "/rest/app-chat/upload-file";
const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

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

fn build_upload_headers() -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "*/*".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("Origin".to_string(), GROK_BASE_URL.to_string());
    headers.insert("Referer".to_string(), format!("{GROK_BASE_URL}/"));
    headers.insert("x-statsig-id".to_string(), browser_statsig_id());
    headers.insert("x-xai-request-id".to_string(), new_uuid());
    headers
}

pub async fn upload_file(
    data: Vec<u8>,
    filename: &str,
    mime_type: &str,
    stealth: &obscura_net::StealthHttpClient,
    cache: &UploadCache,
) -> Result<String, GatewayError> {
    validate_size(data.len() as u64)?;

    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
    if let Some(cached) = cache.get(&hash).await {
        return Ok(cached);
    }

    let content_b64 = base64::engine::general_purpose::STANDARD.encode(&data);

    let payload = serde_json::json!({
        "content": content_b64,
        "fileName": filename,
        "fileSize": data.len(),
        "mimeType": mime_type,
    });

    let url = format!("{GROK_BASE_URL}{UPLOAD_PATH}");
    let body = serde_json::to_string(&payload)
        .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;
    let headers = build_upload_headers();
    let parsed_url = url::Url::parse(&url)
        .map_err(|e| GatewayError::Internal(format!("invalid upload URL {url}: {e}")))?;

    // Same stealth client as the chat path: the upload endpoint sits behind
    // the same Cloudflare Enterprise gate, so it must carry the Chrome TLS
    // fingerprint and a fresh browser-error x-statsig-id marker.
    let response = stealth
        .send_single("POST", &parsed_url, &headers, &body)
        .await
        .map_err(|e| GatewayError::Internal(format!("Grok upload request failed: {e}")))?;

    if !(200..300).contains(&response.status) {
        let preview = String::from_utf8_lossy(&response.body)
            .chars()
            .take(2000)
            .collect::<String>();
        return Err(GatewayError::Provider(format!(
            "Grok file upload failed ({}): {}",
            response.status, preview
        )));
    }

    let result: serde_json::Value = serde_json::from_slice(&response.body)
        .map_err(|e| GatewayError::Internal(format!("upload response parse failed: {e}")))?;

    let file_url = result
        .get("url")
        .and_then(|v| v.as_str())
        .or_else(|| result.get("file_url").and_then(|v| v.as_str()))
        .or_else(|| result.get("download_url").and_then(|v| v.as_str()))
        .or_else(|| result.get("fileUri").and_then(|v| v.as_str()))
        .or_else(|| result.get("parsedFileUri").and_then(|v| v.as_str()))
        .or_else(|| result.get("fileMetadataId").and_then(|v| v.as_str()))
        .ok_or_else(|| {
            GatewayError::Internal(format!(
                "upload response missing url field: {}",
                serde_json::to_string(&result).unwrap_or_default()
            ))
        })?
        .to_string();

    cache.insert(hash, file_url.clone()).await;
    Ok(file_url)
}

pub fn validate_size(size: u64) -> Result<(), GatewayError> {
    if size > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file exceeds maximum size of {} MB",
            MAX_FILE_SIZE / (1024 * 1024)
        )));
    }
    Ok(())
}

pub fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (mime_etc, encoded) = rest.split_once(',')?;
    let mime = mime_etc.split(';').next().unwrap_or("application/octet-stream").to_string();

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
