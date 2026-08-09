use base64::Engine;

use crate::error::GatewayError;

const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Structured attachment matching the web app's format (new API — snake_case).
#[derive(Debug, Clone)]
pub struct FileAttachment {
    /// `"image"` or `"file"`
    pub file_type: String,
    /// OSS path or `mm_file://{file_id}` reference
    pub file_path: String,
    pub file_name: String,
    pub mime_type: String,
    /// Preview URL (can be empty for mm_file:// references)
    pub data_url: String,
}

impl FileAttachment {
    fn file_type_from_mime(mime: &str) -> &str {
        if mime.starts_with("image/") { "image" } else { "file" }
    }
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

use std::sync::Arc;
use std::time::{Duration, Instant};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    attachment: FileAttachment,
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

    pub async fn get(&self, hash: &str) -> Option<FileAttachment> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard.iter().find(|e| e.hash == hash).map(|e| e.attachment.clone())
    }

    pub async fn insert(&self, hash: String, attachment: FileAttachment) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload { hash, attachment, inserted: Instant::now() });
    }
}

/// Resolve the auth token for Minimax file upload.
///
/// Requires `MINIMAX_API_KEY` environment variable. The Minimax file upload
/// API at `api.minimax.io` requires a platform API key, not a JWT from the
/// web app. Falling back to a JWT always returns error code 1004.
fn resolve_upload_token() -> Result<String, GatewayError> {
    let key = std::env::var("MINIMAX_API_KEY").map_err(|_| {
        GatewayError::Auth(
            "MINIMAX_API_KEY environment variable must be set for file uploads. \
             Get your API key at https://platform.minimax.io/user-center/basic-information/interface-key"
                .to_string(),
        )
    })?;

    // Validate the key is not a JWT (3 dot-separated base64 parts).
    // Minimax API keys are single opaque strings.
    if key.split('.').count() == 3 && key.len() > 30 {
        return Err(GatewayError::Auth(
            "The MINIMAX_API_KEY looks like a JWT from the web app, not a Minimax platform API key. \
             Get a real API key at https://platform.minimax.io/user-center/basic-information/interface-key"
                .to_string(),
        ));
    }

    Ok(key)
}

/// Upload a file to the Minimax platform and return a structured attachment.
///
/// Uses the platform Files API at `https://api.minimax.io/v1/files/upload`.
/// The file is uploaded with `purpose=video_understanding` so it can be
/// referenced in agent chat as `mm_file://{file_id}`.
pub async fn upload_file(
    data: Vec<u8>,
    filename: &str,
    mime_type: &str,
    cache: &UploadCache,
) -> Result<FileAttachment, GatewayError> {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
    if let Some(cached) = cache.get(&hash).await {
        return Ok(cached);
    }
    let token = resolve_upload_token()?;
    if data.len() as u64 > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file exceeds maximum size of {} MB",
            MAX_FILE_SIZE / (1024 * 1024)
        )));
    }

    let client = reqwest::Client::new();
    let file_part = reqwest::multipart::Part::bytes(data)
        .file_name(filename.to_string())
        .mime_str(mime_type)
        .map_err(|e| GatewayError::Internal(format!("invalid mime type: {e}")))?;

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("purpose", "video_understanding");

    let url = "https://api.minimax.io/v1/files/upload";

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .multipart(form)
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("upload request failed: {e}")))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(GatewayError::Provider(format!(
            "Minimax file upload failed ({}): {}",
            status,
            body.chars().take(300).collect::<String>()
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| GatewayError::Internal(format!("failed to parse upload response: {e}, body: {body}")))?;

    let base_resp = parsed.get("base_resp").and_then(|v| v.as_object());
    if let Some(br) = base_resp {
        let code = br.get("status_code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = br.get("status_msg").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(GatewayError::Provider(format!(
                "Minimax upload error (code={code}): {msg}"
            )));
        }
    }

    let file_id = parsed
        .pointer("/file/file_id")
        .and_then(|v| v.as_i64())
        .map(|id| id.to_string())
        .or_else(|| {
            parsed
                .get("file")
                .and_then(|f| f.get("file_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| {
            GatewayError::Internal(format!(
                "upload response missing file_id: {}",
                body.chars().take(200).collect::<String>()
            ))
        })?;

    let attachment = FileAttachment {
        file_type: FileAttachment::file_type_from_mime(mime_type).to_string(),
        file_path: format!("mm_file://{}", file_id),
        file_name: filename.to_string(),
        mime_type: mime_type.to_string(),
        data_url: String::new(),
    };
    cache.insert(hash, attachment.clone()).await;
    Ok(attachment)
}

/// Resolve an image/file URL. Data URIs are uploaded; remote URLs are
/// downloaded, uploaded, and returned as a structured `FileAttachment`.
pub async fn resolve_url(
    url: &str,
    cache: &UploadCache,
) -> Result<FileAttachment, GatewayError> {
    if url.starts_with("data:") {
        let (data, mime) = decode_data_uri(url)
            .ok_or_else(|| GatewayError::BadRequest("invalid data URI".to_string()))?;
        let ext = mime.split('/').last().unwrap_or("bin");
        let filename = format!("upload.{}", ext);
        upload_file(data, &filename, &mime, cache).await
    } else {
        let parsed = url::Url::parse(url)
            .map_err(|_| GatewayError::BadRequest(format!("invalid URL: {url}")))?;

        if parsed.scheme() == "http" || parsed.scheme() == "https" {
            let resp = reqwest::get(url).await
                .map_err(|e| GatewayError::Internal(format!("download failed: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(GatewayError::Provider(format!(
                    "remote URL returned {status}: {url}"
                )));
            }
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let data = resp.bytes().await
                .map_err(|e| GatewayError::Internal(format!("read response failed: {e}")))?
                .to_vec();

            let filename = url.rsplit('/').next().unwrap_or("download.bin");
            let mime = content_type.split(';').next().unwrap_or("application/octet-stream");
            upload_file(data, filename, mime, cache).await
        } else {
            Err(GatewayError::BadRequest(format!("unsupported URL scheme: {}", parsed.scheme())))
        }
    }
}
