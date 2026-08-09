use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use obscura_net::StealthHttpClient;
use sha2::{Digest, Sha256};

use crate::error::GatewayError;

const BASE_URL: &str = "https://chat.qwen.ai";
const API_PATH: &str = "/api/v2";
const MAX_FILE_SIZE: usize = 150 * 1024 * 1024;

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .expect("HMAC-SHA256 key length OK");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&result);
    arr
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    metadata: serde_json::Value,
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

    pub async fn get(&self, hash: &str) -> Option<serde_json::Value> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard.iter().find(|e| e.hash == hash).map(|e| e.metadata.clone())
    }

    pub async fn insert(&self, hash: String, metadata: serde_json::Value) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload { hash, metadata, inserted: Instant::now() });
    }
}

/// URL-encode per OSS V4 spec (uppercase hex digits, reserved = A-Z a-z 0-9 - _ . ~)
fn oss_urlencode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// Compute OSS V4 signing key and signature.
///
/// Matches the ali-oss SDK with `authorizationV4: true`.
fn oss_v4_sign(
    method: &str,
    file_path: &str,
    bucket: &str,
    endpoint: &str,
    region: &str,
    access_key_id: &str,
    access_key_secret: &str,
    security_token: &str,
    date_time: &DateTime<Utc>,
) -> Vec<(String, String)> {
    let yyyymmdd = date_time.format("%Y%m%d").to_string();
    let iso8601 = date_time.format("%Y%m%dT%H%M%SZ").to_string();

    // Canonical URI = /{urlencode(object_key)}
    let canonical_uri = format!("/{}", oss_urlencode(file_path));

    // Canonical Query String = empty for direct PUT (no query params)
    let canonical_query_string = "";

    // Canonical Headers (sorted by lowercase key name)
    let host = format!("{}.{}", bucket, endpoint);
    let signed_headers = "host;x-oss-content-sha256;x-oss-date;x-oss-security-token";
    let canonical_headers = format!(
        "host:{}\nx-oss-content-sha256:UNSIGNED-PAYLOAD\nx-oss-date:{}\nx-oss-security-token:{}\n",
        host, iso8601, security_token
    );

    // Canonical Request
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\nUNSIGNED-PAYLOAD",
        method, canonical_uri, canonical_query_string, canonical_headers, signed_headers
    );

    tracing::debug!(canonical_request = %canonical_request, "OSS V4 canonical request");

    // StringToSign
    let hashed_canonical_request = sha256_hex(canonical_request.as_bytes());
    let credential_scope = format!("{}/{}/oss/aliyun_v4_request", yyyymmdd, region);
    let string_to_sign = format!(
        "OSS4-HMAC-SHA256\n{}\n{}\n{}",
        iso8601, credential_scope, hashed_canonical_request
    );

    tracing::debug!(string_to_sign = %string_to_sign, "OSS V4 string to sign");

    // Signing Key: OSS4-HMAC-SHA256 derivation chain
    let init_key = format!("OSS4{}", access_key_secret);
    let date_key = hmac_sha256(init_key.as_bytes(), yyyymmdd.as_bytes());
    let date_region_key = hmac_sha256(&date_key, region.as_bytes());
    let date_region_service_key = hmac_sha256(&date_region_key, b"oss");
    let signing_key = hmac_sha256(&date_region_service_key, b"aliyun_v4_request");

    // Signature
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    // Authorization header
    let authorization = format!(
        "OSS4-HMAC-SHA256 Credential={}/{}/{}/oss/aliyun_v4_request,SignedHeaders={},Signature={}",
        access_key_id, yyyymmdd, region, signed_headers, signature
    );

    vec![
        ("Authorization".to_string(), authorization),
        ("x-oss-date".to_string(), iso8601),
        ("x-oss-content-sha256".to_string(), "UNSIGNED-PAYLOAD".to_string()),
        ("x-oss-security-token".to_string(), security_token.to_string()),
        ("host".to_string(), host),
    ]
}

fn build_request_headers(request_id: &str, token: Option<&str>) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("content-type".to_string(), "application/json".to_string());
    h.insert("accept".to_string(), "application/json, text/plain, */*".to_string());
    h.insert("version".to_string(), "0.2.73".to_string());
    h.insert("source".to_string(), "web".to_string());
    h.insert("x-request-id".to_string(), request_id.to_string());
    h.insert("x-accel-buffering".to_string(), "no".to_string());
    h.insert("referer".to_string(), "https://chat.qwen.ai/".to_string());
    h.insert("origin".to_string(), "https://chat.qwen.ai".to_string());
    if let Some(t) = token {
        h.insert("authorization".to_string(), format!("Bearer {}", t));
    }
    h
}

async fn send_stealth(
    stealth: &StealthHttpClient,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<obscura_net::Response, GatewayError> {
    let parsed_url = url::Url::parse(url)
        .map_err(|e| GatewayError::Internal(format!("invalid URL {url}: {e}")))?;

    for attempt in 1..=3 {
        match stealth.send_single(method, &parsed_url, headers, body).await {
            Ok(resp) => {
                let code = resp.status;
                if code >= 500 || code == 429 {
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                        continue;
                    }
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                    continue;
                }
                return Err(GatewayError::Internal(format!("stealth request failed: {e}")));
            }
        }
    }
    Err(GatewayError::Internal("stealth request exhausted retries".to_string()))
}

/// Decode a data URI into raw bytes and MIME type.
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

/// Derive the simplified file type category from MIME type.
fn simplify_filetype(mime: &str) -> &str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("video/") {
        "video"
    } else if mime.starts_with("audio/") {
        "audio"
    } else {
        "file"
    }
}

/// Request an STS upload token from Qwen's API.
///
/// POSTs to `/api/v2/files/getstsToken` with file metadata (lowercase keys).
/// Returns the STS credentials, file URL, and metadata needed for the
/// subsequent OSS upload.
async fn request_sts_token(
    filename: &str,
    file_size: usize,
    file_type: &str,
    stealth: &StealthHttpClient,
    request_id: &str,
    token: Option<&str>,
) -> Result<serde_json::Value, GatewayError> {
    let url = format!("{}{}/files/getstsToken", BASE_URL, API_PATH);
    let body = serde_json::json!({
        "filename": filename,
        "filesize": file_size,
        "filetype": file_type,
    });
    let body_str = serde_json::to_string(&body)
        .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

    tracing::info!(url = %url, filename = %filename, file_size = file_size, file_type = file_type, "Requesting STS token");

    let response = send_stealth(
        stealth,
        "POST",
        &url,
        &build_request_headers(request_id, token),
        &body_str,
    )
    .await?;

    let status = response.status;
    let resp_body = String::from_utf8(response.body)
        .unwrap_or_default();

    if status != 200 {
        return Err(GatewayError::Provider(format!(
            "Qwen STS token request failed ({}): {}",
            status,
            resp_body.chars().take(300).collect::<String>()
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(&resp_body)
        .map_err(|e| GatewayError::Internal(format!("failed to parse STS response: {e}, body: {resp_body}")))?;

    let success = parsed.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
    if !success {
        return Err(GatewayError::Provider(format!(
            "Qwen STS token request rejected: {}",
            resp_body.chars().take(300).collect::<String>()
        )));
    }

    let data = parsed.get("data").ok_or_else(|| {
        GatewayError::Internal(format!("STS response missing data field: {}", resp_body.chars().take(200).collect::<String>()))
    })?.clone();

    // Validate required fields exist
    for field in &["file_url", "access_key_id", "access_key_secret", "security_token", "bucketname", "region", "file_id"] {
        data.get(*field).and_then(|v| v.as_str()).ok_or_else(|| {
            GatewayError::Internal(format!("STS response missing field '{}'", field))
        })?;
    }

    Ok(data)
}

/// Upload raw bytes to Alibaba Cloud OSS using STS temporary credentials with
/// OSS V4 signature (matching the web app's `ali-oss` SDK with `authorizationV4: true`).
///
/// The web app does NOT use the pre-signed `file_url` — it creates an OSS SDK client
/// with the STS body credentials and lets the SDK sign each PUT. We replicate that
/// by computing the V4 signature ourselves.
async fn upload_to_oss(
    data: &[u8],
    content_type: &str,
    sts_data: &serde_json::Value,
) -> Result<String, GatewayError> {
    let bucket = sts_data["bucketname"].as_str().unwrap_or("");
    let endpoint = sts_data["endpoint"].as_str().unwrap_or("");
    let file_path = sts_data["file_path"].as_str().unwrap_or("");
    let access_key_id = sts_data["access_key_id"].as_str().unwrap_or("");
    let access_key_secret = sts_data["access_key_secret"].as_str().unwrap_or("");
    let security_token = sts_data["security_token"].as_str().unwrap_or("");
    let region = sts_data["region"].as_str().unwrap_or("");

    if file_path.is_empty() || bucket.is_empty() || endpoint.is_empty() {
        return Err(GatewayError::Internal("STS response missing bucket/endpoint/file_path".to_string()));
    }

    let now = Utc::now();
    let oss_headers = oss_v4_sign(
        "PUT", file_path, bucket, endpoint, region,
        access_key_id, access_key_secret, security_token, &now,
    );

    let url = format!("https://{}.{}/{}", bucket, endpoint, oss_urlencode(file_path));
    tracing::info!(url = %url, file_path = %file_path, "Qwen OSS V4 upload starting");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build OSS client: {e}")))?;

    let mut req_builder = client.put(&url).body(data.to_vec());
    for (k, v) in &oss_headers {
        req_builder = req_builder.header(k.as_str(), v.as_str());
    }
    if !content_type.is_empty() {
        req_builder = req_builder.header("content-type", content_type);
    }

    let response = req_builder.send().await
        .map_err(|e| GatewayError::Internal(format!("OSS upload failed: {e}")))?;

    let oss_status = response.status();
    let oss_body = response.text().await.unwrap_or_default();

    if !oss_status.is_success() {
        return Err(GatewayError::Provider(format!(
            "Qwen OSS V4 upload failed ({}): {}",
            oss_status,
            oss_body.chars().take(500).collect::<String>()
        )));
    }

    // Return the canonical CDN URL (without query params) for use in the chat API
    let cdn_url = format!("https://{}.{}/{}", bucket, endpoint, file_path);
    Ok(cdn_url)
}

/// Build the full file metadata object that Qwen expects in the chat API.
fn build_file_metadata(
    file_url: &str,
    filename: &str,
    mime_type: &str,
    file_id: &str,
    file_size: usize,
) -> serde_json::Value {
    let file_type = mime_type.to_string();
    let show_type = if mime_type.starts_with("image/") { "image" } else { "file" };
    let file_class = if mime_type.starts_with("image/") { "vision" } else { "file" };
    let now_ms = unix_ms();

    serde_json::json!({
        "type": show_type,
        "file": {
            "created_at": now_ms,
            "data": {},
            "filename": filename,
            "hash": null,
            "id": file_id,
            "meta": {
                "name": filename,
                "size": file_size,
                "content_type": mime_type,
            },
            "update_at": now_ms,
        },
        "id": file_id,
        "url": file_url,
        "name": filename,
        "collection_name": "",
        "progress": 0,
        "status": "uploaded",
        "greenNet": "success",
        "size": file_size,
        "error": "",
        "itemId": new_uuid(),
        "file_type": file_type,
        "showType": show_type,
        "file_class": file_class,
        "uploadTaskId": new_uuid(),
    })
}

/// Upload a file to Qwen's web API via STS + OSS.
///
/// Flow:
/// 1. Request STS token from `/api/v2/files/getstsToken`
/// 2. Upload raw bytes to Alibaba Cloud OSS with v4 signing
/// 3. Build and return the full file metadata object
pub async fn upload_file(
    data: Vec<u8>,
    filename: &str,
    mime_type: &str,
    stealth: &StealthHttpClient,
    request_id: &str,
    token: Option<&str>,
    cache: &UploadCache,
) -> Result<serde_json::Value, GatewayError> {
    if data.len() > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file exceeds maximum size of {} MB",
            MAX_FILE_SIZE / (1024 * 1024)
        )));
    }

    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
    if let Some(cached) = cache.get(&hash).await {
        return Ok(cached);
    }

    // Step 1: Get STS token
    let filetype = simplify_filetype(mime_type);
    let sts_data = request_sts_token(filename, data.len(), filetype, stealth, request_id, token).await?;

    let file_id = sts_data["file_id"].as_str().unwrap_or("").to_string();

    // Step 2: Upload raw bytes to OSS with v4 signing
    let url = upload_to_oss(&data, mime_type, &sts_data).await?;

    // Step 3: Build and return the full file metadata object
    let metadata = build_file_metadata(&url, filename, mime_type, &file_id, data.len());
    cache.insert(hash, metadata.clone()).await;
    Ok(metadata)
}

/// Process an image/file URL: upload data URIs via STS+OSS, pass remote URLs through.
pub async fn resolve_url(
    url: &str,
    stealth: &StealthHttpClient,
    request_id: &str,
    token: Option<&str>,
    cache: &UploadCache,
) -> Result<serde_json::Value, GatewayError> {
    if url.starts_with("data:") {
        let (data, mime) = decode_data_uri(url)
            .ok_or_else(|| GatewayError::BadRequest("invalid data URI".to_string()))?;
        let ext = mime.split('/').last().unwrap_or("bin");
        let filename = format!("upload.{}", ext);
        upload_file(data, &filename, &mime, stealth, request_id, token, cache).await
    } else {
        Ok(serde_json::json!({"type": "file_url", "url": url}))
    }
}

/// Upload already-downloaded and hashed file bytes. Wrapper for concurrent downloads.
pub async fn upload_hashed_file(
    stealth: &StealthHttpClient,
    request_id: &str,
    token: Option<&str>,
    cache: &UploadCache,
    bytes: &[u8],
    filename: &str,
    mime_type: &str,
) -> Result<serde_json::Value, GatewayError> {
    upload_file(bytes.to_vec(), filename, mime_type, stealth, request_id, token, cache).await
}
