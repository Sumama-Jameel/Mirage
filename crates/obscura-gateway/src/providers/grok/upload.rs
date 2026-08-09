use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, COOKIE};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::GatewayError;
use crate::providers::send_with_retry;

use super::auth::build_cookie_header;
use super::statsig::generate_statsig_id;

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

fn build_http_client() -> Result<reqwest::Client, GatewayError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "User-Agent",
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        ),
    );
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Origin", HeaderValue::from_static("https://grok.com"));
    headers.insert("Referer", HeaderValue::from_static("https://grok.com/"));

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))
}

fn build_upload_headers(cookies: &[obscura_net::CookieInfo], challenge_config: &super::statsig::ChallengeConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let cookie_value = build_cookie_header(cookies);
    headers.insert(COOKIE, HeaderValue::from_str(&cookie_value).unwrap());
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let statsig_id = generate_statsig_id(challenge_config, "POST", UPLOAD_PATH);
    headers.insert("x-statsig-id", HeaderValue::from_str(&statsig_id).unwrap());
    headers.insert("x-xai-request-id", HeaderValue::from_str(&new_uuid()).unwrap());
    headers
}

pub async fn upload_file(
    data: Vec<u8>,
    filename: &str,
    mime_type: &str,
    cookies: &[obscura_net::CookieInfo],
    challenge_config: &super::statsig::ChallengeConfig,
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

    let http = build_http_client()?;
    let headers = build_upload_headers(cookies, challenge_config);
    let url = format!("{}{}", GROK_BASE_URL, UPLOAD_PATH);

    let builder = http
        .post(&url)
        .headers(headers)
        .json(&payload);

    let response = send_with_retry(builder).await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(GatewayError::Provider(format!(
            "Grok file upload failed ({}): {}",
            status, body
        )));
    }

    let result: serde_json::Value = response
        .json()
        .await
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
