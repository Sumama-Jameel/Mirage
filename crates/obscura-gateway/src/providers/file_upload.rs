//! Generic file upload trait for providers that support multimodal
//! (image / file) inputs.
//!
//! Each provider implements the trait with its own upload endpoint,
//! authentication, and protocol.

use crate::error::GatewayError;

/// A reference to an uploaded file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileReference {
    /// Provider-specific file id (e.g. `file-abc123`).
    pub id: String,
    /// Original filename (if available).
    pub name: String,
    /// MIME type (if known).
    pub mime_type: String,
}

/// Provider file upload interface.
///
/// Implementors handle:
/// 1. Downloading remote URLs / decoding base64 data URIs.
/// 2. Uploading the raw bytes to the provider's infrastructure.
/// 3. Returning a provider-specific file reference.
#[allow(dead_code)]
pub trait FileUploadProvider: Send + Sync {
    /// Upload raw bytes and return a file reference.
    fn upload_bytes(
        &self,
        data: &[u8],
        filename: &str,
        mime_type: &str,
    ) -> impl std::future::Future<Output = Result<FileReference, GatewayError>> + Send;

    /// Resolve an `image_url` or `file_url` content part and upload it.
    ///
    /// `url` may be a `data:` URI or a remote HTTP(S) URL.
    fn resolve_and_upload(
        &self,
        url: &str,
        alt_filename: &str,
    ) -> impl std::future::Future<Output = Result<FileReference, GatewayError>> + Send;
}
