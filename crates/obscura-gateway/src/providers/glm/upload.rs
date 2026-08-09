//! GLM/Z.AI file upload helpers for the direct internal-API path.
//!
//! Images and files are uploaded to `POST /api/v1/files/` as multipart form
//! data. The response `{id, filename}` is turned into a short reference that
//! the upstream chat endpoint accepts in place of a data/remote URL.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use reqwest::multipart;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::GatewayError;
use crate::models::{ChatMessage, ContentPart};
use crate::providers::streaming_upload::download_and_hash_batch;

use super::rpc::UploadedFile;

const UPLOAD_URL: &str = "https://chat.z.ai/api/v1/files/";

/// Per-request upload service. Holds a deduplication cache so the same
/// attachment is not uploaded twice within a request.
#[derive(Clone)]
pub struct UploadService {
    http: reqwest::Client,
    token: String,
    chat_id: String,
    cache: Arc<Mutex<UploadCache>>,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    file: UploadedFile,
    inserted: Instant,
}

#[derive(Clone)]
struct UploadCache {
    entries: Vec<CachedUpload>,
    ttl: Duration,
}

impl UploadService {
    pub fn new(http: reqwest::Client, token: String, chat_id: String) -> Self {
        Self {
            http,
            token,
            chat_id,
            cache: Arc::new(Mutex::new(UploadCache {
                entries: Vec::new(),
                ttl: Duration::from_secs(3600),
            })),
        }
    }

    /// Prepare attachments in the last user message, returning the list of
    /// uploaded file references. If any upload fails, an error is returned so
    /// the caller can fall back to UI automation.
    pub async fn prepare_attachments(
        &self,
        messages: &mut [ChatMessage],
    ) -> Result<Vec<UploadedFile>, GatewayError> {
        let last_idx = messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or_else(|| GatewayError::BadRequest("no user message for attachments".to_string()))?;

        let parts = match &messages[last_idx].content {
            crate::models::ChatContent::String(_) => return Ok(Vec::new()),
            crate::models::ChatContent::Array(parts) => parts.clone(),
        };

        let mut remote_urls = Vec::new();
        let mut part_types = Vec::new();
        let mut new_parts = Vec::new();
        
        // First pass: collect remote URLs and text parts
        for part in &parts {
            match part {
                ContentPart::Text { .. } => new_parts.push(part.clone()),
                ContentPart::ImageUrl { image_url } => {
                    if image_url.url.starts_with("data:") {
                        // Defer data URIs for sequential processing
                        new_parts.push(part.clone());
                    } else {
                        remote_urls.push(image_url.url.clone());
                        part_types.push(("image_url", image_url.detail.clone()));
                    }
                }
                ContentPart::FileUrl { file_url } => {
                    if file_url.url.starts_with("data:") {
                        // Defer data URIs for sequential processing
                        new_parts.push(part.clone());
                    } else {
                        remote_urls.push(file_url.url.clone());
                        part_types.push(("file_url", None));
                    }
                }
            }
        }

        let mut files = Vec::new();

        // Concurrently download and hash remote URLs
        if !remote_urls.is_empty() {
            match download_and_hash_batch(&self.http, remote_urls.clone()).await {
                Ok(hashed_files) => {
                    for (hashed, (part_type, detail)) in hashed_files.into_iter().zip(part_types) {
                        let reference = self.upload_bytes(&hashed.bytes, &hashed.name).await?;
                        let file = UploadedFile {
                            reference: reference.clone(),
                            part_type: part_type.to_string(),
                        };

                        // Cache by SHA-256 hash
                        let hash = hashed.hash_base64;
                        self.cache.lock().await.insert(hash, file.clone());
                        files.push(file.clone());

                        // Add to new parts
                        if part_type == "image_url" {
                            new_parts.push(ContentPart::ImageUrl {
                                image_url: crate::models::ImageUrl {
                                    url: reference,
                                    detail,
                                },
                            });
                        } else {
                            new_parts.push(ContentPart::FileUrl {
                                file_url: crate::models::FileUrl { url: reference },
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "concurrent GLM attachment download failed");
                }
            }
        }

        // Process remaining data URIs sequentially
        for part in &parts {
            match part {
                ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:") => {
                    let file = self.upload_url(&image_url.url, "image_url").await?;
                    new_parts.push(ContentPart::ImageUrl {
                        image_url: crate::models::ImageUrl {
                            url: file.reference.clone(),
                            detail: image_url.detail.clone(),
                        },
                    });
                    files.push(file);
                }
                ContentPart::FileUrl { file_url } if file_url.url.starts_with("data:") => {
                    let file = self.upload_url(&file_url.url, "file_url").await?;
                    new_parts.push(ContentPart::FileUrl {
                        file_url: crate::models::FileUrl {
                            url: file.reference.clone(),
                        },
                    });
                    files.push(file);
                }
                _ => {}
            }
        }

        messages[last_idx].content = crate::models::ChatContent::Array(new_parts);
        Ok(files)
    }

    /// Upload a single URL (data URI or remote URL) and return its wire
    /// reference. Caches by SHA-256 of the decoded bytes.
    async fn upload_url(&self, url: &str, part_type: &str) -> Result<UploadedFile, GatewayError> {
        let (bytes, filename) = resolve_source(url).await?;
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));

        {
            let guard = self.cache.lock().await;
            if let Some(cached) = guard.get(&hash) {
                debug!(hash = %hash, "GLM upload cache hit");
                return Ok(cached.clone());
            }
        }

        let reference = self.upload_bytes(&bytes, &filename).await?;
        let file = UploadedFile {
            reference,
            part_type: part_type.to_string(),
        };

        self.cache.lock().await.insert(hash, file.clone());
        Ok(file)
    }

    /// POST the bytes to Z.AI's file upload endpoint.
    async fn upload_bytes(&self, bytes: &[u8], filename: &str) -> Result<String, GatewayError> {
        let part = multipart::Part::bytes(bytes.to_vec())
            .file_name(filename.to_string())
            .mime_str(mime_for_filename(filename))
            .map_err(|e| GatewayError::Internal(format!("invalid MIME type: {e}")))?;
        let form = multipart::Form::new().part("file", part);

        let resp = self
            .http
            .post(UPLOAD_URL)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Referer", format!("https://chat.z.ai/c/{}", self.chat_id))
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("GLM upload request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| GatewayError::Provider(format!("GLM upload decode failed: {e}")))?;

        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "GLM upload returned {status}: {body}"
            )));
        }

        let id = body
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Provider("GLM upload response missing id".to_string()))?;
        let filename = body
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or(filename);

        Ok(format!("{}_{}", id, filename))
    }
}

impl UploadCache {
    fn get(&self, hash: &str) -> Option<UploadedFile> {
        let now = Instant::now();
        self.entries
            .iter()
            .find(|e| e.hash == hash && now.duration_since(e.inserted) <= self.ttl)
            .map(|e| e.file.clone())
    }

    fn insert(&mut self, hash: String, file: UploadedFile) {
        let now = Instant::now();
        self.entries
            .retain(|e| now.duration_since(e.inserted) <= self.ttl);
        self.entries.push(CachedUpload {
            hash,
            file,
            inserted: now,
        });
    }
}

/// Resolve a URL into (bytes, filename). Data URIs are decoded; remote URLs
/// are downloaded with SSRF protection.
async fn resolve_source(url: &str) -> Result<(Vec<u8>, String), GatewayError> {
    if let Some(rest) = url.strip_prefix("data:") {
        decode_data_uri(rest)
    } else {
        download_remote(url).await
    }
}

/// Decode a base64 data URI.
fn decode_data_uri(rest: &str) -> Result<(Vec<u8>, String), GatewayError> {
    let (meta, b64) = rest.split_once(',').ok_or_else(|| {
        GatewayError::BadRequest("invalid data URI: missing comma separator".to_string())
    })?;

    let mime = meta.split(';').next().unwrap_or("application/octet-stream");
    let filename = format!("upload.{}", extension_for_mime(mime));

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| GatewayError::BadRequest(format!("invalid data URI base64: {e}")))?;

    Ok((bytes, filename))
}

/// Download a remote URL with SSRF protection.
async fn download_remote(url: &str) -> Result<(Vec<u8>, String), GatewayError> {
    super::validate_remote_url(url)?;

    let parsed = url::Url::parse(url)
        .map_err(|e| GatewayError::BadRequest(format!("invalid attachment URL: {e}")))?;
    let filename = parsed
        .path_segments()
        .and_then(|s| s.last())
        .unwrap_or("upload.bin")
        .to_string();

    // Use a fresh client with a short timeout; the direct client's cookie jar
    // is not needed for public attachment URLs.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| GatewayError::Internal(format!("attachment download client failed: {e}")))?;

    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("attachment download failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "attachment download returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Provider(format!("attachment download body failed: {e}")))?;
    Ok((bytes.to_vec(), filename))
}

fn mime_for_filename(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".txt") {
        "text/plain"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".doc") || lower.ends_with(".docx") {
        "application/msword"
    } else {
        "application/octet-stream"
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "application/json" => "json",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uri_decode_works() {
        let (bytes, name) = decode_data_uri("image/png;base64,iVBORw0KGgo=").unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(name, "upload.png");
    }

    #[test]
    fn validate_blocks_private_addresses() {
        assert!(super::super::validate_remote_url("http://localhost/foo").is_err());
        assert!(super::super::validate_remote_url("http://192.168.1.1/foo").is_err());
        assert!(super::super::validate_remote_url("https://example.com/foo").is_ok());
    }
}
