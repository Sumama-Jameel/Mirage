use base64::Engine;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::error::GatewayError;
use crate::session::SessionHandle;

const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Web-app file category used in `messageFiles` and the upload mutation.
/// Derived from the `FileTypes` constant in the Le Chat bundle:
/// `["image","document","spreadsheet","text","audio"]`.
const FILE_TYPES: [&str; 5] = ["image", "document", "spreadsheet", "text", "audio"];

/// A file reference attached to a Mistral message.
#[derive(Debug, Clone)]
pub struct MistralFile {
    /// Web-app file category (`image`, `document`, ...).
    pub file_type: String,
    /// Storage URL returned by the upload flow.
    pub url: String,
    pub name: String,
}

impl MistralFile {
    /// Map a MIME type to the web-app file category used by `/api/chat`.
    fn file_type_from_mime(mime: &str) -> &'static str {
        let mime = mime.to_ascii_lowercase();
        if mime.starts_with("image/") {
            "image"
        } else if mime.starts_with("audio/") {
            "audio"
        } else if mime.contains("spreadsheet")
            || mime.contains("excel")
            || mime.contains("csv")
            || mime.contains("ods")
            || mime.contains("officedocument.spreadsheetml")
        {
            "spreadsheet"
        } else if mime.starts_with("text/") {
            "text"
        } else {
            "document"
        }
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

#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    file: MistralFile,
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

    pub async fn get(&self, hash: &str) -> Option<MistralFile> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard
            .iter()
            .find(|e| e.hash == hash)
            .map(|e| e.file.clone())
    }

    pub async fn insert(&self, hash: String, file: MistralFile) {
        let mut guard = self.inner.lock().await;
        guard.retain(|e| Instant::now().duration_since(e.inserted) <= self.ttl);
        guard.push(CachedUpload {
            hash,
            file,
            inserted: Instant::now(),
        });
    }
}

/// Build the `cookie` header value for chat.mistral.ai from the session jar.
///
/// Returns an empty string when no cookies exist for the host (the caller can
/// then decide whether anonymous chat is acceptable; the web API itself
/// requires a session cookie).
pub fn build_cookie_header(session: &SessionHandle) -> String {
    let all = session.cookie_jar.get_all_cookies();
    all.into_iter()
        .filter(|c| {
            c.domain.contains("mistral.ai") || c.domain.contains("mistral")
        })
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Validate that the session cookie jar carries a Mistral session cookie.
///
/// The web app authenticates through `GET /api/session`; the session cookie
/// (e.g. `mistral-chat-session` or an anonymous-id cookie) is what makes that
/// call return `status: "assigned"`. We require at least one cookie that the
/// API can use rather than pretending to be anonymous.
pub fn validate_session_cookie(session: &SessionHandle) -> Result<(), GatewayError> {
    let header = build_cookie_header(session);
    if header.trim().is_empty() {
        return Err(GatewayError::Auth(
            "no Mistral cookies found. Log in to https://chat.mistral.ai in \
             your browser and re-run the gateway."
                .to_string(),
        ));
    }
    Ok(())
}

/// Upload a file to Mistral's storage and return a `MistralFile` reference.
///
/// Mirrors the web app's primary flow:
///
/// 1. `file.uploadFile` tRPC mutation -> `{ uploadURLs, readURLs }` (Azure SAS
///    URLs).
/// 2. `PUT` the bytes to the first upload URL with `Content-Type: <mime>` and
///    `x-ms-blob-type: BlockBlob`.
/// 3. Return `{ type, url: <uploadUrl>, name }` for `messageFiles`.
///
/// Falls back to the proxy endpoint `POST /api/file` (multipart `file` +
/// `type`) when the tRPC mutation is unavailable, matching the web app's
/// `NEXT_PUBLIC_ENABLE_FILE_UPLOAD_PROXY` branch.
pub async fn upload_file(
    data: Vec<u8>,
    filename: &str,
    mime_type: &str,
    session: &SessionHandle,
    cache: &UploadCache,
) -> Result<MistralFile, GatewayError> {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
    if let Some(cached) = cache.get(&hash).await {
        return Ok(cached);
    }
    if data.len() as u64 > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file exceeds maximum size of {} MB",
            MAX_FILE_SIZE / (1024 * 1024)
        )));
    }

    let file_type = MistralFile::file_type_from_mime(mime_type).to_string();

    // The whole upload goes through the stealth client so Cloudflare sees a
    // real Chrome TLS/HTTP fingerprint and the shared cookie jar provides the
    // session cookies (a plain reqwest client is answered with a 403 gate).
    let stealth = obscura_net::StealthHttpClient::new(session.cookie_jar.clone());

    // Primary: tRPC `file.uploadFile` mutation -> Azure Blob PUT.
    let upload = try_trpc_upload(&stealth, &data, mime_type, &file_type).await;
    let url = match upload {
        Ok(url) => url,
        Err(_) => {
            // Fallback: proxy endpoint, matching the web app's proxy branch.
            // Goes over plain reqwest (multipart, binary body) and may be
            // Cloudflare-gated; it is only reached if the tRPC upload failed.
            let client = reqwest::Client::new();
            try_proxy_upload(&client, &data, mime_type, &file_type, &build_cookie_header(session))
                .await?
        }
    };

    let file = MistralFile {
        file_type,
        url,
        name: filename.to_string(),
    };
    cache.insert(hash, file.clone()).await;
    Ok(file)
}

/// tRPC `file.uploadFile` mutation followed by an Azure Blob PUT.
async fn try_trpc_upload(
    stealth: &obscura_net::StealthHttpClient,
    data: &[u8],
    mime_type: &str,
    file_type: &str,
) -> Result<String, GatewayError> {
    let endpoint = "https://chat.mistral.ai/api/trpc/file.uploadFile?batch=1";
    // httpBatchStreamLink v10 payload: `{"0":{"json":{...}}}`.
    let body = serde_json::json!({
        "0": {
            "json": {
                "type": file_type,
                "count": 1,
                "includeReadUrl": true,
            }
        }
    });

    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "application/json".to_string());
    headers.insert("x-trpc-source".to_string(), "nextjs-react".to_string());
    headers.insert("accept".to_string(), "application/json".to_string());
    headers.insert("origin".to_string(), "https://chat.mistral.ai".to_string());
    headers.insert("referer".to_string(), "https://chat.mistral.ai/chat".to_string());
    let url = url::Url::parse(endpoint)
        .map_err(|e| GatewayError::Internal(format!("invalid upload URL: {e}")))?;
    let resp = stealth
        .send_single("POST", &url, &headers, &body.to_string().as_str())
        .await
        .map_err(|e| GatewayError::Internal(format!("Mistral upload request failed: {e}")))?;
    let status = resp.status;
    let text = resp.text();
    if !(200..300).contains(&status) {
        return Err(GatewayError::Provider(format!(
            "Mistral uploadFile mutation failed ({status}): {}",
            text.chars().take(200).collect::<String>()
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| GatewayError::Internal(format!("failed to parse upload response: {e}")))?;

    let upload_urls = parsed
        .get(0)
        .and_then(|v| v.get("result"))
        .and_then(|v| v.get("data"))
        .and_then(|v| v.get("json"))
        .and_then(|v| v.get("uploadURLs"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            GatewayError::Internal(format!(
                "upload response missing uploadURLs: {}",
                text.chars().take(200).collect::<String>()
            ))
        })?
        .to_string();

    // Azure Blob PUT: not Cloudflare-gated, and needs a binary body which the
    // stealth client's string-only `send_single` cannot carry, so it stays on
    // reqwest. The SAS URL authenticates the PUT (no session cookies needed).
    let put_resp = reqwest::Client::new()
        .put(&upload_urls)
        .header("Content-Type", mime_type)
        .header("x-ms-blob-type", "BlockBlob")
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("Mistral blob PUT failed: {e}")))?;

    if !put_resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "Mistral blob PUT failed ({}): {}",
            put_resp.status(),
            put_resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }

    Ok(upload_urls)
}

/// Proxy upload: `POST /api/file` with multipart `file` + `type`.
async fn try_proxy_upload(
    client: &reqwest::Client,
    data: &[u8],
    mime_type: &str,
    file_type: &str,
    cookie_header: &str,
) -> Result<String, GatewayError> {
    let file_part = reqwest::multipart::Part::bytes(data.to_vec())
        .mime_str(mime_type)
        .map_err(|e| GatewayError::Internal(format!("invalid mime type: {e}")))?;
    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("type", file_type.to_string());

    let mut req = client
        .post("https://chat.mistral.ai/api/file")
        .header("origin", "https://chat.mistral.ai")
        .header("referer", "https://chat.mistral.ai/chat")
        .multipart(form);
    if !cookie_header.is_empty() {
        req = req.header("cookie", cookie_header);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("Mistral proxy upload failed: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(GatewayError::Provider(format!(
            "Mistral proxy upload failed ({}): {}",
            status,
            text.chars().take(200).collect::<String>()
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| GatewayError::Internal(format!("failed to parse proxy upload response: {e}")))?;
    let url = parsed
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            GatewayError::Internal(format!(
                "proxy upload response missing url: {}",
                text.chars().take(200).collect::<String>()
            ))
        })?
        .to_string();
    Ok(url)
}

/// Validate that a remote URL does not point to a private network.
/// SSRF protection - blocks loopback, RFC1918, link-local, and IPv6 ULA,
/// mirroring the other providers' upload paths (`chatgpt/upload.rs`,
/// `claude/upload.rs`).
pub fn validate_remote_url(url: &str) -> Result<(), GatewayError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| GatewayError::BadRequest(format!("invalid URL: {url}")))?;

    let host = parsed
        .host()
        .ok_or_else(|| GatewayError::BadRequest(format!("no host in URL: {url}")))?;

    match host {
        url::Host::Domain(domain) => {
            if domain == "localhost" || domain == "127.0.0.1" || domain == "0.0.0.0" {
                return Err(GatewayError::BadRequest(
                    "local addresses are blocked".to_string(),
                ));
            }
            Ok(())
        }
        url::Host::Ipv4(ip) => {
            if ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() {
                return Err(GatewayError::BadRequest(
                    "private IPv4 addresses are blocked".to_string(),
                ));
            }
            Ok(())
        }
        url::Host::Ipv6(ip) => {
            // IPv4-mapped IPv6 addresses (e.g. `[::ffff:127.0.0.1]`) resolve
            // to IPv4 loopback/private ranges, so check the mapped address too.
            if ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unique_local() {
                return Err(GatewayError::BadRequest(
                    "private IPv6 addresses are blocked".to_string(),
                ));
            }
            if let Some(mapped) = ip.to_ipv4_mapped() {
                if mapped.is_loopback() || mapped.is_private() || mapped.is_link_local() {
                    return Err(GatewayError::BadRequest(
                        "private IPv4-mapped addresses are blocked".to_string(),
                    ));
                }
            }
            Ok(())
        }
    }
}

/// Download a remote URL into memory with a streaming size cap, enforcing the
/// same 64 MB limit as `upload_file`. Returns `(bytes, mime, filename)`.
async fn download_with_cap(url: &str) -> Result<(Vec<u8>, String, String), GatewayError> {
    validate_remote_url(url)?;

    // Timeout and redirect limits: a slow or redirecting host behind a
    // user-supplied URL must not stall the worker or bypass the SSRF gate.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| GatewayError::Internal(format!("download client build failed: {e}")))?;

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

    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| GatewayError::Internal(format!("read response failed: {e}")))?;
        if data.len() as u64 + chunk.len() as u64 > MAX_FILE_SIZE {
            return Err(GatewayError::BadRequest(format!(
                "remote file exceeds maximum size of {} MB",
                MAX_FILE_SIZE / (1024 * 1024)
            )));
        }
        data.extend_from_slice(&chunk);
    }

    let filename = url
        .rsplit('/')
        .next()
        .map(|f| f.split('?').next().unwrap_or(f))
        .filter(|f| !f.is_empty())
        .unwrap_or("download.bin")
        .to_string();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok((data, mime, filename))
}

/// Resolve an image/file URL into a `MistralFile`. Data URIs are uploaded
/// directly; remote URLs are downloaded, then uploaded.
pub async fn resolve_url(
    url: &str,
    session: &SessionHandle,
    cache: &UploadCache,
) -> Result<MistralFile, GatewayError> {
    if url.starts_with("data:") {
        let (data, mime) = decode_data_uri(url)
            .ok_or_else(|| GatewayError::BadRequest("invalid data URI".to_string()))?;
        let ext = mime.split('/').last().unwrap_or("bin");
        let filename = format!("upload.{ext}");
        upload_file(data, &filename, &mime, session, cache).await
    } else {
        let parsed = url::Url::parse(url)
            .map_err(|_| GatewayError::BadRequest(format!("invalid URL: {url}")))?;

        if parsed.scheme() == "http" || parsed.scheme() == "https" {
            let (data, mime, filename) = download_with_cap(url).await?;
            upload_file(data, &filename, &mime, session, cache).await
        } else {
            Err(GatewayError::BadRequest(format!(
                "unsupported URL scheme: {}",
                parsed.scheme()
            )))
        }
    }
}

/// Upload already-downloaded and hashed file bytes. Wrapper for concurrent downloads.
pub async fn upload_hashed_file(
    bytes: &[u8],
    filename: &str,
    session: &SessionHandle,
    cache: &UploadCache,
) -> Result<MistralFile, GatewayError> {
    // Determine MIME type from filename if possible
    let mime = match filename.split('.').last() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    };
    
    upload_file(bytes.to_vec(), filename, mime, session, cache).await
}

/// Validate that a MIME type maps to a supported web-app file category.
#[allow(dead_code)]
pub fn is_supported_mime(mime: &str) -> bool {
    let ft = MistralFile::file_type_from_mime(mime);
    FILE_TYPES.contains(&ft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_maps_to_web_file_type() {
        assert_eq!(MistralFile::file_type_from_mime("image/png"), "image");
        assert_eq!(
            MistralFile::file_type_from_mime("application/pdf"),
            "document"
        );
        assert_eq!(MistralFile::file_type_from_mime("text/plain"), "text");
        assert_eq!(
            MistralFile::file_type_from_mime(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            ),
            "spreadsheet"
        );
        assert_eq!(MistralFile::file_type_from_mime("audio/mpeg"), "audio");
    }

    #[test]
    fn decodes_data_uri() {
        let (bytes, mime) =
            decode_data_uri("data:image/png;base64,aGVsbG8=").expect("decodes");
        assert_eq!(bytes, b"hello");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn upload_cache_dedupes() {
        let cache = UploadCache::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            cache.insert("hash-1".to_string(), MistralFile {
                file_type: "image".to_string(),
                url: "https://x/1".to_string(),
                name: "a.png".to_string(),
            }).await;
            let got = cache.get("hash-1").await.expect("cached");
            assert_eq!(got.url, "https://x/1");
            assert!(cache.get("nope").await.is_none());
        });
    }

    #[test]
    fn validate_remote_url_rejects_private_hosts() {
        for url in [
            "http://localhost:3000/x.png",
            "http://127.0.0.1/x.png",
            "http://192.168.1.1/x.png",
            "http://10.0.0.1/x.png",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/x.png",
            "http://[fd00::1]/x.png",
        ] {
            assert!(
                validate_remote_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn validate_remote_url_accepts_public_hosts() {
        for url in [
            "https://example.com/a.png",
            "http://8.8.8.8/a.png",
            "https://example.org/x?q=1",
        ] {
            assert!(
                validate_remote_url(url).is_ok(),
                "expected {url} to be accepted"
            );
        }
    }

    #[test]
    fn validate_remote_url_rejects_invalid_urls() {
        assert!(validate_remote_url("not a url").is_err());
        // A valid host over a non-http scheme is fine; only the SSRF check
        // matters here (the caller rejects non-http schemes separately).
        assert!(validate_remote_url("ftp://example.com/x").is_ok());
    }
}
