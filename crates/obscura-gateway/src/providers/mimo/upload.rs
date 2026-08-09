use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use md5::{Digest, Md5};
use sha2::Sha256;
use tokio::sync::Mutex;

use crate::error::GatewayError;

/// MiMo batches uploads are capped at 50MB by the web app.
const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// Structured media item matching the MiMo web app's `multiMedias` entry.
#[derive(Debug, Clone)]
pub struct MediaItem {
    /// `"image"`, `"audio"`, `"video"` or `"file"`
    pub media_type: String,
    /// Public resource URL returned by `genUploadInfo`.
    pub file_url: String,
    pub name: String,
    pub size: usize,
    pub status: String,
    pub object_name: String,
    /// Final resource id used in the chat request (parse id or resourceId).
    pub url: String,
    pub token_usage: i64,
}

impl MediaItem {
    /// `mediaType` category from a MIME type, matching `GetMediaTypeFromMime`.
    fn media_type_from_mime(mime: &str) -> &str {
        if mime.starts_with("image/") {
            "image"
        } else if mime.starts_with("audio/") {
            "audio"
        } else if mime.starts_with("video/") {
            "video"
        } else {
            "file"
        }
    }
}

/// File extension for a MIME type, matching the web app's map.
/// Unknown MIME types fall back to `.bin`.
fn mime_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        "audio/mpeg" => ".mp3",
        "audio/wav" => ".wav",
        "audio/flac" => ".flac",
        "audio/x-m4a" => ".m4a",
        "audio/ogg" => ".ogg",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "video/x-msvideo" => ".avi",
        "video/x-ms-wmv" => ".wmv",
        _ => ".bin",
    }
}

/// Decode a data URI into raw bytes and MIME type.
pub fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (mime_etc, encoded) = rest.split_once(',')?;
    let mime = mime_etc
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();

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

fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Content-addressed cache for uploaded files. Files are only uploaded once
/// per process lifetime; identical content is reused via its md5.
#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    md5: String,
    item: MediaItem,
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

    pub async fn get(&self, md5: &str) -> Option<MediaItem> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard
            .iter()
            .find(|e| e.md5 == md5)
            .map(|e| e.item.clone())
    }

    pub async fn insert(&self, md5: String, item: MediaItem) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload {
            md5,
            item,
            inserted: Instant::now(),
        });
    }
}

/// Build the full cookie header for aistudio.xiaomimimo.com from the session
/// cookie jar.
pub fn build_mimo_cookie_header(cookies: &[obscura_net::CookieInfo]) -> String {
    let names = [
        "xiaomichatbot_serviceToken",
        "userId",
        "xiaomichatbot_ph",
    ];
    let mut parts: Vec<String> = Vec::new();
    for name in names {
        if let Some(c) = cookies.iter().find(|c| c.name == name) {
            parts.push(format!("{}={}", c.name, c.value));
        }
    }
    parts.join("; ")
}

/// Extract the `xiaomichatbot_ph` cookie value (URL-escaped on the wire).
///
/// Firefox stores the value wrapped in double quotes (for example
/// `"Ja1BTWN5..."`). The server rejects a quoted ph in the URL with HTTP 400,
/// so strip the surrounding quotes here, mirroring the reference client.
pub fn extract_ph(cookies: &[obscura_net::CookieInfo]) -> Option<String> {
    cookies
        .iter()
        .find(|c| c.name == "xiaomichatbot_ph")
        .map(|c| c.value.trim_matches('"').to_string())
}

/// Validate that the session carries the three MiMo auth cookies.
pub fn validate_mimo_cookies(cookies: &[obscura_net::CookieInfo]) -> Result<(), GatewayError> {
    let missing: Vec<&str> = [
        "xiaomichatbot_serviceToken",
        "userId",
        "xiaomichatbot_ph",
    ]
    .iter()
    .filter(|name| !cookies.iter().any(|c| c.name == **name))
    .copied()
    .collect();
    if !missing.is_empty() {
        return Err(GatewayError::Auth(format!(
            "MiMo session cookies missing: [{}]. \
             Log in to https://aistudio.xiaomimimo.com in your browser and re-run.",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Upload a file to the MiMo FDS and return a `MediaItem` for `multiMedias`.
///
/// Mirrors the web app flow:
/// 1. `POST /open-apis/resource/genUploadInfo` → `{resourceId, resourceUrl,
///    uploadUrl, objectName}` (signed FDS upload URL).
/// 2. `PUT uploadUrl` with the raw bytes and `Content-MD5`.
/// 3. `POST /open-apis/resource/parse` to register the file, returning the
///    final resource id used in chat requests.
pub async fn upload_file(
    client: &reqwest::Client,
    cookie_header: &str,
    ph: &str,
    data: Vec<u8>,
    filename: &str,
    mime_type: &str,
    model_name: &str,
    cache: &UploadCache,
) -> Result<MediaItem, GatewayError> {
    if data.len() as u64 > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file exceeds maximum size of {} MB",
            MAX_FILE_SIZE / (1024 * 1024)
        )));
    }
    let md5 = md5_hex(&data);
    if let Some(cached) = cache.get(&md5).await {
        return Ok(cached);
    }

    // 1. Request signed upload info.
    let gen_url = format!(
        "https://aistudio.xiaomimimo.com/open-apis/resource/genUploadInfo?xiaomichatbot_ph={}",
        urlencode(ph)
    );
    let gen_body = serde_json::json!({
        "fileName": filename,
        "fileContentMd5": md5,
    });
    let gen_resp = client
        .post(&gen_url)
        .header("Content-Type", "application/json")
        .header("Accept-Language", "system")
        .header("x-timeZone", "Asia/Shanghai")
        .header("Cookie", cookie_header)
        .body(serde_json::to_string(&gen_body).map_err(|e| {
            GatewayError::Internal(format!("failed to serialize genUploadInfo body: {e}"))
        })?)
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("genUploadInfo request failed: {e}")))?;

    let status = gen_resp.status();
    let body = gen_resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(GatewayError::Provider(format!(
            "MiMo genUploadInfo failed ({}): {}",
            status,
            body.chars().take(300).collect::<String>()
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        GatewayError::Internal(format!("failed to parse genUploadInfo response: {e}, body: {body}"))
    })?;
    let data_obj = parsed.get("data").and_then(|v| v.as_object());
    let upload_url = data_obj
        .and_then(|d| d.get("uploadUrl"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            GatewayError::Provider(format!(
                "MiMo genUploadInfo missing uploadUrl: {}",
                body.chars().take(200).collect::<String>()
            ))
        })?
        .to_string();
    let resource_url = data_obj
        .and_then(|d| d.get("resourceUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let object_name = data_obj
        .and_then(|d| d.get("objectName"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let resource_id = data_obj
        .and_then(|d| d.get("resourceId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 2. PUT the raw bytes to the signed FDS URL.
    let put_resp = client
        .put(&upload_url)
        .header("Content-Type", "application/octet-stream")
        .header("Content-MD5", &md5)
        .body(data.clone())
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("FDS PUT failed: {e}")))?;
    let put_status = put_resp.status();
    if !put_status.is_success() {
        let put_body = put_resp.text().await.unwrap_or_default();
        return Err(GatewayError::Provider(format!(
            "MiMo FDS PUT failed ({}): {}",
            put_status,
            put_body.chars().take(300).collect::<String>()
        )));
    }

    // 3. Register / parse the uploaded file to get the final resource id.
    let mut final_id = resource_id;
    let mut token_usage: i64 = 0;
    if !resource_url.is_empty() && !object_name.is_empty() {
        let parse_url = format!(
            "https://aistudio.xiaomimimo.com/open-apis/resource/parse?fileUrl={}&objectName={}&model={}&xiaomichatbot_ph={}",
            urlencode(&resource_url),
            urlencode(&object_name),
            urlencode(model_name),
            urlencode(ph)
        );
        if let Ok(parse_resp) = client
            .post(&parse_url)
            .header("Content-Type", "application/json")
            .header("Cookie", cookie_header)
            .body("{}")
            .send()
            .await
        {
            if let Ok(parse_body) = parse_resp.text().await {
                if let Ok(parse_val) = serde_json::from_str::<serde_json::Value>(&parse_body) {
                    if let Some(id) = parse_val
                        .pointer("/data/id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        final_id = id.to_string();
                    }
                    if let Some(tu) = parse_val.pointer("/data/tokenUsage").and_then(|v| v.as_i64())
                    {
                        token_usage = tu;
                    }
                }
            }
        }
    }

    let item = MediaItem {
        media_type: MediaItem::media_type_from_mime(mime_type).to_string(),
        file_url: resource_url,
        name: filename.to_string(),
        size: data.len(),
        status: "completed".to_string(),
        object_name,
        url: final_id,
        token_usage,
    };
    cache.insert(md5, item.clone()).await;
    Ok(item)
}

/// Resolve an image/file URL to a `MediaItem`.
///
/// Data URIs are uploaded directly; remote HTTP(S) URLs are downloaded first
/// and then uploaded, mirroring the web app's uploader.
pub async fn resolve_url(
    client: &reqwest::Client,
    cookie_header: &str,
    ph: &str,
    url: &str,
    model_name: &str,
    cache: &UploadCache,
) -> Result<MediaItem, GatewayError> {
    if url.starts_with("data:") {
        let (data, mime) = decode_data_uri(url)
            .ok_or_else(|| GatewayError::BadRequest("invalid data URI".to_string()))?;
        let ext = mime_extension(&mime);
        let filename = format!("upload{}", ext);
        upload_file(
            client,
            cookie_header,
            ph,
            data,
            &filename,
            &mime,
            model_name,
            cache,
        )
        .await
    } else {
        let parsed = url::Url::parse(url)
            .map_err(|_| GatewayError::BadRequest(format!("invalid URL: {url}")))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(GatewayError::BadRequest(format!(
                "unsupported URL scheme: {}",
                parsed.scheme()
            )));
        }
        let resp = client
            .get(url)
            .send()
            .await
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
        let data = resp
            .bytes()
            .await
            .map_err(|e| GatewayError::Internal(format!("read response failed: {e}")))?
            .to_vec();

        let filename = url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("download.bin")
            .to_string();
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("application/octet-stream");
        upload_file(
            client,
            cookie_header,
            ph,
            data,
            &filename,
            mime,
            model_name,
            cache,
        )
        .await
    }
}

/// Upload already-downloaded and hashed file bytes. Wrapper for concurrent downloads.
pub async fn upload_hashed_file(
    client: &reqwest::Client,
    cookie_header: &str,
    ph: &str,
    bytes: &[u8],
    filename: &str,
    model_name: &str,
    cache: &UploadCache,
) -> Result<MediaItem, GatewayError> {
    let mime = mime_type_for_filename(filename);
    upload_file(
        client,
        cookie_header,
        ph,
        bytes.to_vec(),
        filename,
        &mime,
        model_name,
        cache,
    )
    .await
}

/// Infer MIME type from filename extension.
fn mime_type_for_filename(filename: &str) -> String {
    match filename.split('.').last() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Matches JS `encodeURIComponent`: spaces → `%20`, safe = `A-Za-z0-9-._~`.
/// Used for the `xiaomichatbot_ph` query parameter.
pub fn urlencode(s: &str) -> String {
    let mut result = String::new();
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

/// Short hex id used for `msgId` (web uses a random 16-byte hex string).
pub fn rand_hex(n: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..n).map(|_| rng.gen::<u8>()).collect();
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

/// Placeholder to keep the sha2 dependency usage explicit for future content
/// addressing (hashes are currently md5 per the upstream protocol).
#[allow(dead_code)]
fn _sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}
