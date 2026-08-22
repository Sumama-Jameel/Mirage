//! Gemini file upload service.
//!
//! Handles uploading files (images, PDFs, text, etc.) to Google's
//! content-push infrastructure using the resumable upload protocol.
//! Follows the same approach as the gpt4free Gemini provider.
//!
//! Upload flow (resumable protocol):
//!   1. OPTIONS → `content-push.googleapis.com/upload/`
//!   2. POST   → start upload, receive `X-Goog-Upload-Url`
//!   3. OPTIONS → upload URL
//!   4. POST   → upload + finalize with raw bytes
//!   5. Response body = content URL

use std::net::IpAddr;

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Method;
use tracing::{debug, info};
use url::Url;

use crate::error::GatewayError;

const UPLOAD_URL: &str = "https://content-push.googleapis.com/upload/";
const UPLOAD_AUTH: &str = "Basic c2F2ZXM6cyNMdGhlNmxzd2F2b0RsN3J1d1U=";
const PUSH_ID: &str = "feeds/mcudyrk2a4khkz";
const TENANT_ID: &str = "bard-storage";
const MAX_FILE_SIZE: usize = 20 * 1024 * 1024;

/// Build the standard upload headers for content-push.googleapis.com.
fn upload_headers() -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        HeaderName::from_static("authority"),
        HeaderValue::from_static("content-push.googleapis.com"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(
        HeaderName::from_static("accept-language"),
        HeaderValue::from_static("en-US,en;q=0.7"),
    );
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static(UPLOAD_AUTH),
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded;charset=UTF-8"),
    );
    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://gemini.google.com"),
    );
    headers.insert(
        HeaderName::from_static("push-id"),
        HeaderValue::from_static(PUSH_ID),
    );
    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://gemini.google.com/"),
    );
    headers.insert(
        HeaderName::from_static("x-goog-upload-protocol"),
        HeaderValue::from_static("resumable"),
    );
    headers.insert(
        HeaderName::from_static("x-tenant-id"),
        HeaderValue::from_static(TENANT_ID),
    );

    Ok(headers)
}

/// Upload a single file to Gemini's content-push service.
///
/// The MIME type (e.g. `image/png`) is sent as `X-Goog-Upload-Header-Content-Type`
/// during the start phase so the storage backend tags the file correctly.
/// Returns a pair of `(file_url, file_name)`.
#[allow(dead_code)]
async fn upload_single_file(
    http: &reqwest::Client,
    data: &[u8],
    name: &str,
    mime_type: &str,
) -> Result<(String, String), GatewayError> {
    if data.is_empty() {
        return Err(GatewayError::BadRequest("empty file data".to_string()));
    }
    if data.len() > MAX_FILE_SIZE {
        return Err(GatewayError::BadRequest(format!(
            "file too large: {} bytes (max {})",
            data.len(),
            MAX_FILE_SIZE
        )));
    }

    let headers = upload_headers()?;

    // Step 1: OPTIONS to establish upload capability
    let resp = http
        .request(Method::OPTIONS, UPLOAD_URL)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("upload OPTIONS failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "upload OPTIONS returned {}",
            resp.status()
        )));
    }

    // Step 2: POST to start the upload
    //
    // MIME type is sent as X-Goog-Upload-Header-Content-Type so the storage
    // backend tags the file correctly. The body is "File name: {name}" per
    // the gpt4free reference implementation.
    let start_headers = {
        let mut h = headers.clone();
        h.insert(
            HeaderName::from_static("x-goog-upload-command"),
            HeaderValue::from_static("start"),
        );
        h.insert(
            HeaderName::from_static("x-goog-upload-header-content-length"),
            HeaderValue::from_str(&data.len().to_string())
                .or_else(|_| HeaderValue::from_str(""))
                .map_err(|e| GatewayError::Internal(format!("invalid length header: {e}")))?,
        );
        h.insert(
            HeaderName::from_static("x-goog-upload-header-content-type"),
            HeaderValue::from_str(mime_type)
                .map_err(|e| GatewayError::Internal(format!("invalid mime header: {e}")))?,
        );
        h
    };
    let start_body = format!("File name: {name}");

    let resp = http
        .post(UPLOAD_URL)
        .headers(start_headers)
        .body(start_body)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("upload start failed: {e}")))?;

    let start_status = resp.status();
    let upload_url = resp
        .headers()
        .get("x-goog-upload-url")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let start_body = resp.text().await.unwrap_or_default();
    let upload_url = upload_url.ok_or_else(|| {
        GatewayError::Provider(format!(
            "upload start returned {start_status}, body={start_body}, no X-Goog-Upload-Url header"
        ))
    })?;

    info!(
        status = %start_status,
        body = %start_body.chars().take(200).collect::<String>(),
        upload_url = %upload_url,
        "Gemini upload start done"
    );

    // Step 3: OPTIONS to the upload URL
    let resp = http
        .request(Method::OPTIONS, &upload_url)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("upload OPTIONS to URL failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "upload OPTIONS to URL returned {}",
            resp.status()
        )));
    }

    // Step 4: POST to upload and finalize
    // Keep the default form-urlencoded Content-Type as the gpt4free reference
    // implementation does; the actual file type is communicated via
    // X-Goog-Upload-Header-Content-Type in the start phase.
    let final_headers = {
        let mut h = headers.clone();
        h.insert(
            HeaderName::from_static("x-goog-upload-command"),
            HeaderValue::from_static("upload, finalize"),
        );
        h.insert(
            HeaderName::from_static("x-goog-upload-offset"),
            HeaderValue::from_static("0"),
        );
        h
    };

    let resp = http
        .post(&upload_url)
        .headers(final_headers)
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("upload finalize failed: {e}")))?;

    let http_status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();

    let raw_body = resp
        .text()
        .await
        .unwrap_or_else(|_| "<body read error>".to_string());

    info!(
        status = %http_status,
        content_type = %content_type,
        body_preview = %raw_body.chars().take(500).collect::<String>(),
        "Gemini upload finalize response"
    );

    if !http_status.is_success() {
        return Err(GatewayError::Provider(format!(
            "upload finalize returned {http_status}: {raw_body}"
        )));
    }

    let file_url = raw_body;

    info!(
        file_url = %file_url,
        "Gemini upload final file URL"
    );

    if file_url.is_empty() {
        return Err(GatewayError::Provider(
            "upload returned empty file URL".to_string(),
        ));
    }

    debug!(
        file_url_len = file_url.len(),
        "Gemini file uploaded successfully"
    );

    Ok((file_url, name.to_string()))
}

/// Upload multiple files to Gemini's content-push service.
///
/// Each entry is `(file_bytes, file_name, mime_type)`.
/// Returns `Vec<(file_url, file_name)>`.
pub async fn upload_files(
    http: &reqwest::Client,
    files: &[(Vec<u8>, String, String)],
) -> Result<Vec<(String, String)>, GatewayError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let mut results = Vec::with_capacity(files.len());
    for (data, name, mime) in files {
        let result = upload_single_file(http, data, name, mime).await?;
        results.push(result);
    }

    Ok(results)
}

/// Decode a base64 data URI into raw bytes and MIME type.
///
/// Supports formats like:
/// - `data:image/png;base64,iVBOR...`
/// - `data:image/png;charset=utf-8;base64,AAAA...`
/// - `data:application/pdf;base64,JVBER...`
pub fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    let uri = uri.strip_prefix("data:")?;
    // Find `;base64,` from the end so charset params between the MIME
    // type and the encoding (e.g. `text/plain;charset=utf-8;base64,...`)
    // don't break parsing.
    let base64_marker = ";base64,";
    let marker_pos = uri.rfind(base64_marker)?;
    let mime_part = &uri[..marker_pos];
    let b64 = &uri[marker_pos + base64_marker.len()..];
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()?;
    let mime_type = mime_part.split(';').next().unwrap_or("application/octet-stream").to_string();
    Some((bytes, mime_type))
}

/// Download a file from a remote URL.
///
/// Validates the URL against private network ranges to prevent SSRF attacks
/// before making any HTTP request.
#[allow(dead_code)]
pub async fn download_file(
    http: &reqwest::Client,
    url: &str,
) -> Result<(Vec<u8>, String), GatewayError> {
    let parsed = Url::parse(url)
        .map_err(|e| GatewayError::BadRequest(format!("invalid download URL: {e}")))?;
    validate_remote_url(&parsed)?;

    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("download file failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "download file returned {}",
            resp.status()
        )));
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).to_string())
        .or_else(|| {
            parsed
                .path_segments()
                .and_then(|s| s.last())
                .and_then(guess_mime_from_path)
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Provider(format!("download file bytes failed: {e}")))?
        .to_vec();

    Ok((bytes, content_type))
}

/// Reject private-network URLs unless explicitly allowed.
///
/// Prevents SSRF attacks by blocking requests to loopback, RFC1918 private
/// addresses, and link-local addresses. Can be overridden with the
/// `OBSCURA_ALLOW_PRIVATE_NETWORK` environment variable for local testing.
pub(crate) fn validate_remote_url(url: &Url) -> Result<(), GatewayError> {
    if std::env::var("OBSCURA_ALLOW_PRIVATE_NETWORK").is_ok() {
        return Ok(());
    }

    match url.host() {
        None => {
            return Err(GatewayError::BadRequest(
                "file url missing host".to_string(),
            ));
        }
        Some(url::Host::Domain(host)) => {
            if host == "localhost" {
                return Err(GatewayError::BadRequest(
                    "private file URLs are blocked by default".to_string(),
                ));
            }
        }
        Some(url::Host::Ipv4(ip)) => {
            if is_private_ip(IpAddr::V4(ip)) {
                return Err(GatewayError::BadRequest(
                    "private file URLs are blocked by default".to_string(),
                ));
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if is_private_ip(IpAddr::V6(ip)) {
                return Err(GatewayError::BadRequest(
                    "private file URLs are blocked by default".to_string(),
                ));
            }
        }
    }

    Ok(())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            let segments = v6.segments();
            // fe80::/10 link-local
            if segments[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // fc00::/7 unique local address (IPv6 private equivalent)
            if segments[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            false
        }
    }
}

/// Build the file list payload element for the Gemini request.
///
/// Uses the format `[[url, 1, Null, mime], name, Null×7, [0]]` reported as
/// working in gpt4free issue #3254 (Nov 2025). The MIME type is embedded
/// at index [3] of the inner URL array, and a `[0]` suffix terminates the
/// per-file record.
pub fn build_file_list(uploads: &[(String, String, String)]) -> serde_json::Value {
    if uploads.is_empty() {
        return serde_json::Value::Array(vec![]);
    }
    serde_json::Value::Array(
        uploads
            .iter()
            .map(|(url, name, mime)| {
                serde_json::json!([
                    [url, 1, null, mime],
                    name,
                    null, null, null, null, null, null, null,
                    [0]
                ])
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// MIME-type utilities (mirrors DeepSeek's implementation)
// ---------------------------------------------------------------------------

fn guess_mime_from_path(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_lowercase();
    match ext.as_str() {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "gif" => Some("image/gif".to_string()),
        "webp" => Some("image/webp".to_string()),
        "svg" => Some("image/svg+xml".to_string()),
        "pdf" => Some("application/pdf".to_string()),
        "txt" => Some("text/plain".to_string()),
        "md" => Some("text/markdown".to_string()),
        "csv" => Some("text/csv".to_string()),
        "json" => Some("application/json".to_string()),
        "doc" => Some("application/msword".to_string()),
        "docx" => Some(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string(),
        ),
        _ => None,
    }
}

#[allow(dead_code)]
fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "text/csv" => Some("csv"),
        "application/json" => Some("json"),
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some("docx")
        }
        _ => None,
    }
}

fn default_filename_for_mime(mime: &str) -> String {
    if is_image_mime(mime) {
        "image.png".to_string()
    } else {
        "file".to_string()
    }
}

fn is_image_mime(mime: &str) -> bool {
    mime.starts_with("image/")
}

/// Determine the best filename for a downloaded file based on URL path and
/// content type. Returns `(name, content_type)`.
pub fn derive_filename(path: &str, content_type: &str) -> String {
    // Prefer the last path segment from the URL
    if let Some(segment) = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
    {
        return segment.to_string();
    }
    default_filename_for_mime(content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_private_ipv4_url() {
        let url = Url::parse("http://192.168.1.1/x.png").unwrap();
        assert!(validate_remote_url(&url).is_err());
    }

    #[test]
    fn reject_loopback_ipv4_url() {
        let url = Url::parse("http://127.0.0.1/x.png").unwrap();
        assert!(validate_remote_url(&url).is_err());
    }

    #[test]
    fn reject_localhost_url() {
        let url = Url::parse("http://localhost:8080/x.png").unwrap();
        assert!(validate_remote_url(&url).is_err());
    }

    #[test]
    fn reject_link_local_url() {
        let url = Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(validate_remote_url(&url).is_err());
    }

    #[test]
    fn reject_private_ipv6_url() {
        let url = Url::parse("http://[fc00::1]/x.png").unwrap();
        assert!(validate_remote_url(&url).is_err());
    }

    #[test]
    fn allow_public_url() {
        let url = Url::parse("https://example.com/x.png").unwrap();
        assert!(validate_remote_url(&url).is_ok());
    }

    #[test]
    fn allow_public_ip() {
        let url = Url::parse("http://8.8.8.8/x.png").unwrap();
        assert!(validate_remote_url(&url).is_ok());
    }

    #[test]
    fn is_private_ip_v4_private() {
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.0.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v4_loopback() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v4_link_local() {
        assert!(is_private_ip("169.254.0.1".parse().unwrap()));
        assert!(is_private_ip("169.254.255.255".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v6_loopback() {
        assert!(is_private_ip("::1".parse().unwrap()));
    }

    #[test]
    fn is_not_private_ip() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v6_unique_local() {
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd00::1".parse().unwrap()));
        assert!(is_private_ip("fcff::1".parse().unwrap()));
    }

    #[test]
    fn decode_valid_png_data_uri() {
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let (bytes, mime) = decode_data_uri(uri).unwrap();
        assert_eq!(mime, "image/png");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn decode_pdf_data_uri() {
        let uri = "data:application/pdf;base64,JVBERi0xLjQKMSAwIG9iago8PC9UeXBlIC9DYXRhbG9nL1BhZ2VzIDIgMCBSPj4KZW5kb2JqCjMgMCBvYmoKPDwvVHlwZSAvUGFnZS9QYXJlbnQgMiAwIFIvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXT4+CmVuZG9iagp4cmVmCjAgNAowMDAwMDAwMDAwIDY1NTM1IGYgCjAwMDAwMDAwMDkgMDAwMDAgbiAKMDAwMDAwMDA1OSAwMDAwMCBuIAowMDAwMDAwMTE2IDAwMDAwIG4gCnRyYWlsZXIKPDwvU2l6ZSA0L1Jvb3QgMSAwIFI+PgpzdGFydHhyZWYKMTUwCiUlRU9G";
        let (bytes, mime) = decode_data_uri(uri).unwrap();
        assert_eq!(mime, "application/pdf");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn decode_invalid_data_uri() {
        assert!(decode_data_uri("not-a-data-uri").is_none());
        assert!(decode_data_uri("data:text/plain,hello").is_none());
    }

    #[test]
    fn build_file_list_empty() {
        let list = build_file_list(&[] as &[(String, String, String)]);
        assert_eq!(list.as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_file_list_non_empty() {
        let uploads = vec![
            ("http://example.com/img1".to_string(), "img1.png".to_string(), "image/png".to_string()),
            ("http://example.com/doc1".to_string(), "doc1.pdf".to_string(), "application/pdf".to_string()),
        ];
        let list = build_file_list(&uploads);
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // Format: [[url, 1, null, mime], name, null, null, null, null, null, null, null, [0]]
        assert_eq!(arr[0][0][0], "http://example.com/img1");
        assert_eq!(arr[0][0][1], 1);
        assert!(arr[0][0][2].is_null());
        assert_eq!(arr[0][0][3], "image/png");
        assert_eq!(arr[0][1], "img1.png");
    }

    #[test]
    fn guess_mime_from_path_for_png() {
        assert_eq!(guess_mime_from_path("image.png"), Some("image/png".to_string()));
    }

    #[test]
    fn guess_mime_from_path_for_pdf() {
        assert_eq!(guess_mime_from_path("doc.pdf"), Some("application/pdf".to_string()));
    }

    #[test]
    fn guess_mime_from_path_unknown() {
        assert!(guess_mime_from_path("file.xyz").is_none());
    }

    #[test]
    fn derive_filename_from_path() {
        let name = derive_filename("/images/photo.png", "image/png");
        assert_eq!(name, "photo.png");
    }

    #[test]
    fn derive_filename_from_mime_fallback() {
        let name = derive_filename("", "application/pdf");
        assert_eq!(name, "file");
    }

    #[test]
    fn derive_filename_image_mime_fallback() {
        let name = derive_filename("", "image/png");
        assert_eq!(name, "image.png");
    }
}
