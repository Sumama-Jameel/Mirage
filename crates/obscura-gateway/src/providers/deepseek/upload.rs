//! DeepSeek file upload service.
//!
//! Handles resolving OpenAI-style `image_url` and `file_url` references
//! (base64 data URLs and remote HTTP URLs), uploading them to DeepSeek's file
//! service, and caching the resulting `file-...` ids per session.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;
use reqwest::multipart;
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

use crate::error::GatewayError;
use crate::models::{ChatMessage, ToolCall};
use crate::session::SessionHandle;

use crate::providers::solver::{PoWHeader, SolverChain, SolverRegistry};
use crate::providers::tool_call::format_tool_results;
use crate::providers::streaming_upload::download_and_hash_batch;
use super::state::SessionStore;

const UPLOAD_PATH: &str = "/api/v0/file/upload_file";
const UPLOAD_URL: &str = "https://chat.deepseek.com/api/v0/file/upload_file";
const MAX_FILE_SIZE: usize = 20 * 1024 * 1024;
const DEFAULT_IMAGE_NAME: &str = "image.png";
const DEFAULT_FILE_NAME: &str = "file";

/// Maximum concurrent uploads to prevent resource exhaustion. Mirrors the
/// download concurrency cap so the two halves stay symmetric.
const MAX_CONCURRENT_UPLOADS: usize = 4;

/// Cached upload result for one session.
#[derive(Clone)]
#[allow(dead_code)]
struct CachedUpload {
    file_id: String,
    inserted: Instant,
}

/// Per-session file upload cache.
#[derive(Clone)]
#[allow(dead_code)]
pub struct UploadCache {
    inner: Arc<Mutex<HashMap<String, CachedUpload>>>,
    ttl: Duration,
}

impl UploadCache {
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(60 * 60))
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl,
        }
    }

    pub async fn get(&self, hash: &str) -> Option<String> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|_, v| now.duration_since(v.inserted) <= self.ttl);
        guard.get(hash).map(|v| v.file_id.clone())
    }

    pub async fn insert(&self, hash: String, file_id: String) {
        let mut guard = self.inner.lock().await;
        guard.insert(
            hash,
            CachedUpload {
                file_id,
                inserted: Instant::now(),
            },
        );
    }
}

impl Default for UploadCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Service that resolves and uploads files for a single DeepSeek session.
pub struct FileUploadService {
    http: reqwest::Client,
    #[allow(dead_code)]
    session: SessionHandle,
    solvers: SolverRegistry,
    solver_chain: SolverChain,
    cache: UploadCache,
}

impl FileUploadService {
    pub fn new(
        http: reqwest::Client,
        session: SessionHandle,
        solvers: SolverRegistry,
        solver_chain: SolverChain,
        cache: UploadCache,
    ) -> Self {
        Self {
            http,
            session,
            solvers,
            solver_chain,
            cache,
        }
    }

    /// Prepare the last user message for DeepSeek.
    ///
    /// Returns the text prompt and any uploaded file ids. Image and file parts
    /// are resolved and uploaded; text parts are concatenated into the prompt.
    /// When the last message is `role: "tool"`, the prompt is annotated with
    /// the exact tool call that produced it so the model can continue the
    /// conversation correctly.
    pub async fn prepare_last_user_message(
        &self,
        session_id: &str,
        messages: &[ChatMessage],
        model_type: &str,
        thinking_enabled: bool,
        store: &SessionStore,
    ) -> Result<(String, Vec<String>), GatewayError> {
        let last_idx = messages
            .iter()
            .rposition(|m| m.role == "user" || m.role == "tool");

        let (prompt, image_urls, file_urls) = if let Some(idx) = last_idx {
            if messages[idx].role == "tool" {
                // Collect all consecutive tool messages at the end of the request.
                let start = messages[..=idx]
                    .iter()
                    .rposition(|m| m.role != "tool")
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let tool_msgs = &messages[start..=idx];

                let mut items: Vec<(Option<ToolCall>, Option<String>, String)> =
                    Vec::with_capacity(tool_msgs.len());
                for msg in tool_msgs {
                    let (call, fallback_id) = if let Some(call_id) = msg.tool_call_id.as_deref() {
                        (store.get_tool_call(session_id, call_id).await, Some(call_id.to_string()))
                    } else {
                        (None, None)
                    };
                    items.push((call, fallback_id, msg.content.as_text()));
                }

                let refs: Vec<(Option<&ToolCall>, Option<&str>, &str)> = items
                    .iter()
                    .map(|(c, id, o)| (c.as_ref(), id.as_deref(), o.as_str()))
                    .collect();

                (format_tool_results(&refs), Vec::new(), Vec::new())
            } else {
                let msg = &messages[idx];
                (
                    msg.content.as_text(),
                    msg.content.image_urls(),
                    msg.content.file_urls(),
                )
            }
        } else {
            (String::new(), Vec::new(), Vec::new())
        };

        let mut file_ids = Vec::new();

        // Concurrently download and hash all images and files
        let all_urls: Vec<String> = image_urls.iter().chain(file_urls.iter()).cloned().collect();
        
        if !all_urls.is_empty() {
            match download_and_hash_batch(&self.http, all_urls.clone()).await {
                Ok(hashed_files) => {
                    // Upload each hashed file concurrently, mirroring the
                    // parallel download half. DeepSeek's upload endpoint is
                    // the latency bottleneck on multimodal requests; running
                    // the uploads in parallel (bounded like the downloads)
                    // keeps total latency close to the slowest single file
                    // instead of the sum. Results are collected and then
                    // processed in original order so the file_ids vector stays
                    // order-stable regardless of completion order.
                    let max_concurrent = MAX_CONCURRENT_UPLOADS;
                    let num_images = image_urls.len();
                    // (hash, name) per index, kept alongside so the cache key
                    // and warning context survive moving each HashedFile into
                    // its upload future.
                    let meta: Vec<(String, String)> = hashed_files
                        .iter()
                        .map(|h| (h.hash_base64.clone(), h.name.clone()))
                        .collect();
                    let mut results: Vec<(usize, Result<String, String>)> =
                        Vec::with_capacity(hashed_files.len());

                    {
                        use futures::stream::{self, StreamExt};
                        let mut buffered = stream::iter(hashed_files.into_iter().enumerate())
                            .map(|(idx, hashed)| async move {
                                let is_image = idx < num_images;
                                let model = if is_image { "vision" } else { model_type };
                                let result = self
                                    .upload_file(&hashed.bytes, &hashed.name, &hashed.mime_type, model, thinking_enabled)
                                    .await
                                    .map_err(|e| e.to_string());
                                (idx, result)
                            })
                            .buffered(max_concurrent);

                        while let Some((idx, result)) = buffered.next().await {
                            results.push((idx, result));
                        }
                    }

                    results.sort_by_key(|(idx, _)| *idx);
                    for (idx, result) in results {
                        match result {
                            Ok(file_id) => {
                                self.cache
                                    .insert(meta[idx].0.clone(), file_id.clone())
                                    .await;
                                file_ids.push(file_id);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    url = ?meta[idx].1,
                                    error = %e,
                                    "failed to upload attachment"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "concurrent download of attachments failed");
                }
            }
        }

        Ok((prompt, file_ids))
    }

    /// Resolve a single file URL and upload it, using the cache when possible.
    #[allow(dead_code)]
    async fn upload_file_url(
        &self,
        url: &str,
        upload_model_type: &str,
        thinking_enabled: bool,
    ) -> Result<String, GatewayError> {
        let (bytes, filename, content_type) = resolve_file(url).await?;
        if bytes.len() > MAX_FILE_SIZE {
            return Err(GatewayError::BadRequest(format!(
                "file too large: {} bytes (max {})",
                bytes.len(),
                MAX_FILE_SIZE
            )));
        }

        let hash = content_hash(&bytes);
        if let Some(cached) = self.cache.get(&hash).await {
            tracing::debug!(file_id = %cached, "reusing cached file upload");
            return Ok(cached);
        }

        let file_id = self
            .upload_file(&bytes, &filename, &content_type, upload_model_type, thinking_enabled)
            .await?;
        self.cache.insert(hash, file_id.clone()).await;
        Ok(file_id)
    }

    /// Fork an uploaded image file to the vision model pipeline.
    ///
    /// DeepSeek's initial upload only performs OCR text extraction on images.
    /// To make the image visible to the vision model, we must fork it to
    /// the "vision" model type via `POST /api/v0/file/fork_file_task`.
    async fn fork_file_to_vision(&self, file_id: &str) -> Result<String, GatewayError> {
        let fork_url = "https://chat.deepseek.com/api/v0/file/fork_file_task";
        let resp = self
            .http
            .post(fork_url)
            .json(&serde_json::json!({
                "file_id": file_id,
                "to_model_type": "vision"
            }))
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("fork file request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| GatewayError::Provider(format!("fork file decode failed: {e}")))?;

        tracing::info!("DeepSeek fork: status={} body={}", status, body);
        let data = body.get("data").and_then(|v| v.as_object());
        let biz = data.and_then(|d| d.get("biz_data"));

        // If the file is already VISION model_kind, fork returns
        // biz_data: null with biz_code=2 ("model kind satisfied").
        // In that case the original file_id is already usable, but
        // it may still be PENDING — wait for it to become ready.
        if biz.is_none() || biz.and_then(|v| v.as_object()).is_none() {
            tracing::info!(
                "DeepSeek fork: file already VISION, waiting for original id={}",
                file_id
            );
            self.wait_for_file_ready(file_id).await?;
            return Ok(file_id.to_string());
        }

        let biz = biz.unwrap();
        let new_file_id = biz
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| GatewayError::Provider("fork response missing id".to_string()))?;

        self.wait_for_file_ready(&new_file_id).await?;
        tracing::info!("DeepSeek fork: vision file ready id={}", new_file_id);
        Ok(new_file_id)
    }

    /// Upload raw file bytes to DeepSeek.
    async fn upload_file(
        &self,
        bytes: &[u8],
        filename: &str,
        content_type: &str,
        upload_model_type: &str,
        thinking_enabled: bool,
    ) -> Result<String, GatewayError> {
        tracing::info!("DeepSeek upload: starting PoW solve");
        let header = self.create_upload_pow_header().await?;
        tracing::info!("DeepSeek upload: PoW solved, sending upload request");

        let part = multipart::Part::bytes(bytes.to_vec())
            .file_name(filename.to_string())
            .mime_str(content_type)
            .map_err(|e| GatewayError::Internal(format!("invalid file mime: {e}")))?;
        let form = multipart::Form::new().part("file", part);

        let thinking_flag = if thinking_enabled { "1" } else { "0" };

        let resp = self
            .http
            .post(UPLOAD_URL)
            .header(&header.name, &header.value)
            .header("x-file-size", bytes.len().to_string())
            .header("x-model-type", upload_model_type)
            .header("x-thinking-enabled", thinking_flag)
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("file upload request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| GatewayError::Provider(format!("file upload decode failed: {e}")))?;

        tracing::info!("DeepSeek upload: upload response status={} body={}", status, body);
        let biz = unwrap_biz(body, status)?;
        tracing::debug!(response = %biz, "file upload response");

        let file_id = biz
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| GatewayError::Provider("file upload response missing id".to_string()))?;

        // Fork images to the vision pipeline.
        //
        // When upload_model_type is "vision", the file is already submitted
        // to the vision pipeline via the x-model-type header. No fork needed.
        if is_image_mime(content_type) {
            if upload_model_type != "vision" {
                tracing::info!("DeepSeek upload: waiting for OCR before fork");
                self.wait_for_file_ready(&file_id).await?;
                tracing::info!("DeepSeek upload: OCR complete, forking to vision pipeline");
                let vision_file_id = self.fork_file_to_vision(&file_id).await?;
                return Ok(vision_file_id);
            }
            tracing::info!("DeepSeek upload: file already VISION, waiting for ready");
            self.wait_for_file_ready(&file_id).await?;
            return Ok(file_id);
        }

        tracing::info!("DeepSeek upload: waiting for file ready");
        self.wait_for_file_ready(&file_id).await?;
        tracing::info!("DeepSeek upload: file ready");

        Ok(file_id)
    }

    /// Poll DeepSeek until the uploaded file is ready to be attached.
    async fn wait_for_file_ready(&self, file_id: &str) -> Result<(), GatewayError> {
        let url = format!("https://chat.deepseek.com/api/v0/file/fetch_files?file_ids={file_id}");
        for attempt in 0..20 {
            let resp = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| GatewayError::Provider(format!("fetch_files request failed: {e}")))?;

            let status = resp.status();
            let body = resp
                .json::<serde_json::Value>()
                .await
                .map_err(|e| GatewayError::Provider(format!("fetch_files decode failed: {e}")))?;
            let biz = unwrap_biz(body, status)?;
            tracing::debug!(response = %biz, attempt, "fetch_files response");

            let files = biz
                .get("files")
                .and_then(|v| v.as_array())
                .map(|arr| arr.as_slice())
                .unwrap_or_else(|| {
                    if let Some(arr) = biz.as_array() {
                        arr.as_slice()
                    } else {
                        &[]
                    }
                });
            let file_obj = files
                .iter()
                .find(|v| v.get("id").and_then(|i| i.as_str()) == Some(file_id))
                .or_else(|| if biz.get("id").is_some() { Some(&biz) } else { None });

            if let Some(obj) = file_obj {
                if let Some(err) = obj.get("error_code").and_then(|v| v.as_str()) {
                    if !err.is_empty() {
                        return Err(GatewayError::Provider(format!(
                            "image upload rejected: {err}"
                        )));
                    }
                }
                let status_str = obj.get("status").and_then(|v| v.as_str()).unwrap_or("");
                if status_str == "SUCCESS" {
                    // Some servers require the client to fetch the signed path once
                    // before the file id can be attached to a completion. Do it
                    // opportunistically.
                    if let Some(signed_path) =
                        obj.get("signed_path").and_then(|v| v.as_str())
                    {
                        let confirm_url = format!("https://files.deepseeksvc.com{signed_path}");
                        if let Err(e) = self.http.get(&confirm_url).send().await {
                            tracing::debug!(error = %e, "file confirmation fetch failed");
                        }
                    }
                    return Ok(());
                }
            }

            let delay = Duration::from_millis(1000);
            tokio::time::sleep(delay).await;
        }

        Err(GatewayError::Provider(
            "image upload did not become ready in time".to_string(),
        ))
    }

    /// Solve a PoW challenge for the upload endpoint.
    async fn create_upload_pow_header(&self) -> Result<PoWHeader, GatewayError> {
        let url = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({"target_path": UPLOAD_PATH}))
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("upload pow challenge request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| GatewayError::Provider(format!("upload pow challenge decode failed: {e}")))?;

        let wrapper = unwrap_biz(body, status)?;
        let challenge = wrapper.get("challenge").ok_or_else(|| {
            GatewayError::Provider("upload pow challenge response missing challenge object".to_string())
        })?;
        self.solvers.solve_fallback(&self.solver_chain, challenge)
    }
}

/// Resolve a file URL into bytes, filename, and content type.
async fn resolve_file(url: &str) -> Result<(Vec<u8>, String, String), GatewayError> {
    if let Some(data) = url.strip_prefix("data:") {
        return decode_data_url(data);
    }

    let parsed = Url::parse(url)
        .map_err(|e| GatewayError::BadRequest(format!("invalid file url: {e}")))?;
    validate_remote_url(&parsed)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build file download client: {e}")))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("failed to download file: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(GatewayError::Provider(format!(
            "file download returned {status}"
        )));
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).to_string())
        .or_else(|| guess_mime_from_path(parsed.path()))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Provider(format!("failed to read file bytes: {e}")))?
        .to_vec();

    let filename = parsed
        .path_segments()
        .and_then(|segs| segs.last())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_filename_for_mime(&content_type));

    Ok((bytes, filename, content_type))
}

/// Parse a `data:[<mime>];base64,<data>` URL.
fn decode_data_url(data: &str) -> Result<(Vec<u8>, String, String), GatewayError> {
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
        b64.replace(" ", "+") // form posts sometimes encode '+' as ' '.
            .replace("\n", "")
            .replace("\r", ""),
    )
    .map_err(|e| GatewayError::BadRequest(format!("invalid base64 file: {e}")))?;

    let filename = if let Some(ext) = extension_for_mime(&content_type) {
        format!("file.{ext}")
    } else {
        DEFAULT_FILE_NAME.to_string()
    };

    Ok((bytes, filename, content_type))
}

/// Reject private-network URLs unless explicitly allowed.
fn validate_remote_url(url: &Url) -> Result<(), GatewayError> {
    if std::env::var("OBSCURA_ALLOW_PRIVATE_NETWORK").is_ok() {
        return Ok(());
    }

    let host = url
        .host_str()
        .ok_or_else(|| GatewayError::BadRequest("file url missing host".to_string()))?;

    if host == "localhost" || host == "127.0.0.1" || host.starts_with("[::1]") {
        return Err(GatewayError::BadRequest(
            "private file URLs are blocked by default".to_string(),
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(GatewayError::BadRequest(
                "private file URLs are blocked by default".to_string(),
            ));
        }
    }

    Ok(())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments()[0..2] == [0xfe80, 0],
    }
}

fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

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
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        _ => None,
    }
}

fn default_filename_for_mime(mime: &str) -> String {
    if is_image_mime(mime) {
        DEFAULT_IMAGE_NAME.to_string()
    } else {
        DEFAULT_FILE_NAME.to_string()
    }
}

fn is_image_mime(mime: &str) -> bool {
    mime.starts_with("image/")
}

/// Unwrap DeepSeek's standard `data.biz_data` envelope.
fn unwrap_biz(body: serde_json::Value, status: reqwest::StatusCode) -> Result<serde_json::Value, GatewayError> {
    if status.is_success() {
        if body.get("code").and_then(|v| v.as_i64()) != Some(0) {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown DeepSeek API error");
            return Err(GatewayError::Provider(format!("DeepSeek API error: {msg}")));
        }
        if let Some(biz) = body.get("data").and_then(|d| d.get("biz_data")) {
            return Ok(biz.clone());
        }
    }
    Err(GatewayError::Provider(format!(
        "DeepSeek API returned {status}: {body}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_png_data_url() {
        let (bytes, filename, ct) = decode_data_url("image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(filename, "file.png");
        assert_eq!(ct, "image/png");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn decode_pdf_data_url() {
        let (bytes, filename, ct) = decode_data_url("application/pdf;base64,JVBERi0xLjQ=").unwrap();
        assert_eq!(filename, "file.pdf");
        assert_eq!(ct, "application/pdf");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn reject_private_image_url() {
        let url = Url::parse("http://192.168.1.1/x.png").unwrap();
        assert!(validate_remote_url(&url).is_err());
    }

    #[test]
    fn allow_public_image_url() {
        let url = Url::parse("https://example.com/x.png").unwrap();
        assert!(validate_remote_url(&url).is_ok());
    }

    #[test]
    fn content_hash_is_stable() {
        let a = content_hash(b"hello");
        let b = content_hash(b"hello");
        assert_eq!(a, b);
        assert_ne!(a, content_hash(b"world"));
    }
}
