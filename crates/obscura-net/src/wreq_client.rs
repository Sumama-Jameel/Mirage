#[cfg(feature = "stealth")]
use std::collections::HashMap;
#[cfg(feature = "stealth")]
use std::error::Error;
#[cfg(feature = "stealth")]
use std::sync::Arc;
#[cfg(feature = "stealth")]
use std::time::Duration;

#[cfg(feature = "stealth")]
use futures::stream::{BoxStream, StreamExt};
#[cfg(feature = "stealth")]
use tokio::sync::RwLock;
#[cfg(feature = "stealth")]
use url::Url;

#[cfg(feature = "stealth")]
use crate::cookies::CookieJar;
#[cfg(feature = "stealth")]
use crate::client::{Response, ObscuraNetError};

#[cfg(feature = "stealth")]
pub const STEALTH_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

// The wreq emulation (Profile::Chrome145, Platform::Windows) sends this exact
// UA and sec-ch-ua-platform "Windows" on the wire. navigator has to report the
// same identity, otherwise the TLS/HTTP layer and the JS layer disagree and a
// site cross-checks the mismatch as a bot signal.
#[cfg(feature = "stealth")]
pub const STEALTH_NAVIGATOR_PLATFORM: &str = "Win32";
#[cfg(feature = "stealth")]
pub const STEALTH_UA_PLATFORM: &str = "Windows";
#[cfg(feature = "stealth")]
pub const STEALTH_UA_PLATFORM_VERSION: &str = "15.0.0";

/// Per-client timeout tuning. `connect` bounds TCP+TLS setup, `request`
/// bounds the whole call including body read. Streaming callers should widen
/// `request` because SSE/NDJSON responses stay open while tokens stream.
#[cfg(feature = "stealth")]
#[derive(Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub request: Duration,
}

#[cfg(feature = "stealth")]
impl Timeouts {
    pub const DEFAULT: Timeouts = Timeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_secs(30),
    };

    /// For long-lived provider streams (LLM token SSE runs for the whole
    /// conversation turn, up to minutes).
    pub fn streaming() -> Timeouts {
        Timeouts {
            connect: Duration::from_secs(5),
            request: Duration::from_secs(300),
        }
    }
}

#[cfg(feature = "stealth")]
pub struct StealthHttpClient {
    client: wreq::Client,
    pub cookie_jar: Arc<CookieJar>,
    pub extra_headers: RwLock<HashMap<String, String>>,
    pub in_flight: Arc<std::sync::atomic::AtomicU32>,
}

/// A streaming response from `send_single_streaming`. Status and headers are
/// available immediately; the body arrives as a byte stream so NDJSON/SSE
/// callers can consume lines incrementally instead of buffering the whole
/// response. Used by Cloudflare-gated providers (Mistral) that need the
/// browser TLS fingerprint on the streaming connection too, not just on the
/// initial request.
#[cfg(feature = "stealth")]
pub struct StreamingResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub bytes: BoxStream<'static, Result<Vec<u8>, ObscuraNetError>>,
}

#[cfg(feature = "stealth")]
impl StealthHttpClient {
    pub fn new(cookie_jar: Arc<CookieJar>) -> Self {
        Self::with_timeouts(cookie_jar, None, None)
    }

    pub fn with_proxy(cookie_jar: Arc<CookieJar>, proxy_url: Option<&str>) -> Self {
        Self::with_timeouts(cookie_jar, proxy_url, None)
    }

    pub fn with_timeouts(
        cookie_jar: Arc<CookieJar>,
        proxy_url: Option<&str>,
        timeouts: Option<Timeouts>,
    ) -> Self {
        let timeouts = timeouts.unwrap_or(Timeouts::DEFAULT);
        let emulation_opts = wreq_util::Emulation::builder()
            .profile(wreq_util::Profile::Chrome145)
            .platform(wreq_util::Platform::Windows)
            .build();

        let mut builder = wreq::Client::builder()
            .emulation(emulation_opts)
            .connect_timeout(timeouts.connect)
            .timeout(timeouts.request)
            .redirect(wreq::redirect::Policy::none());

        if let Some(proxy) = proxy_url {
            if let Ok(p) = wreq::Proxy::all(proxy) {
                builder = builder.proxy(p);
            }
        }

        let client = builder.build().expect("failed to build wreq stealth client");

        StealthHttpClient {
            client,
            cookie_jar,
            extra_headers: RwLock::new(HashMap::new()),
            in_flight: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    pub async fn fetch(&self, url: &Url) -> Result<Response, ObscuraNetError> {
        let mut current_url = url.clone();

        if let Some(host) = current_url.host_str() {
            if crate::blocklist::is_blocked(host) {
                tracing::debug!("Blocked tracker: {}", current_url);
                return Ok(Response {
                    status: 0,
                    url: current_url,
                    headers: HashMap::new(),
                    body: Vec::new(),
                    redirected_from: Vec::new(),
                });
            }
        }

        let mut redirects = Vec::new();

        for _ in 0..20 {
            let mut req = self.client.get(current_url.as_str());

            let cookie_header = self.cookie_jar.get_cookie_header(&current_url);
            if !cookie_header.is_empty() {
                req = req.header("Cookie", &cookie_header);
            }

            for (k, v) in self.extra_headers.read().await.iter() {
                req = req.header(k.as_str(), v.as_str());
            }

            self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let resp = req.send().await.map_err(|e| {
                self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                ObscuraNetError::Network(format!("{}: {} (source: {:?})", current_url, e, e.source()))
            })?;
            self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

            let status = resp.status();

            for val in resp.headers().get_all("set-cookie") {
                if let Ok(s) = val.to_str() {
                    self.cookie_jar.set_cookie(s, &current_url);
                }
            }

            let response_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
                .collect();

            if status.is_redirection() {
                if let Some(location) = resp.headers().get("location") {
                    let location_str = location.to_str().map_err(|_| {
                        ObscuraNetError::Network("Invalid redirect Location".into())
                    })?;
                    let next_url = current_url.join(location_str).map_err(|e| {
                        ObscuraNetError::Network(format!("Invalid redirect URL: {}", e))
                    })?;
                    redirects.push(current_url.clone());
                    current_url = next_url;
                    continue;
                }
            }

            let body = resp.bytes().await.map_err(|e| {
                ObscuraNetError::Network(format!("Failed to read body: {}", e))
            })?.to_vec();

            return Ok(Response {
                url: current_url,
                status: status.as_u16(),
                headers: response_headers,
                body,
                redirected_from: redirects,
            });
        }

        Err(ObscuraNetError::TooManyRedirects(url.to_string()))
    }

    /// One request with no redirect following, for scripted fetch()/XHR. Reads
    /// the cookie jar for the Cookie header and stores Set-Cookie back into it,
    /// so the caller only owns redirect hops and SSRF re-validation. Used in
    /// stealth mode so JS-level requests carry the same Chrome TLS fingerprint
    /// and client hints as the main navigation instead of the rustls ClientHello
    /// that op_fetch_url would otherwise send (which bot managers read as a
    /// non-browser script and reject, e.g. the AWS WAF challenge verify call).
    pub async fn send_single(
        &self,
        method: &str,
        url: &Url,
        headers: &HashMap<String, String>,
        body: &str,
    ) -> Result<Response, ObscuraNetError> {
        if let Some(host) = url.host_str() {
            if crate::blocklist::is_blocked(host) {
                tracing::debug!("Blocked tracker: {}", url);
                return Ok(Response {
                    status: 0,
                    url: url.clone(),
                    headers: HashMap::new(),
                    body: Vec::new(),
                    redirected_from: Vec::new(),
                });
            }
        }

        let req_method = method
            .parse::<wreq::Method>()
            .map_err(|e| ObscuraNetError::Network(format!("invalid method '{}': {}", method, e)))?;
        let mut req = self.client.request(req_method, url.as_str());

        let cookie_header = self.cookie_jar.get_cookie_header(url);
        if !cookie_header.is_empty() {
            req = req.header("cookie", &cookie_header);
        }
        for (k, v) in self.extra_headers.read().await.iter() {
            req = req.header(k.as_str(), v.as_str());
        }
        for (k, v) in headers.iter() {
            req = req.header(k.as_str(), v.as_str());
        }
        if !body.is_empty() {
            req = req.body(body.to_string());
        }

        let cookie_preview = if cookie_header.len() > 60 {
            format!("{}...", &cookie_header[..60])
        } else {
            cookie_header.clone()
        };
        tracing::debug!(method = %method, url = %url, body_len = body.len(), cookie = %cookie_preview, "wreq send_single request");

        self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let resp = req.send().await.map_err(|e| {
            self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            ObscuraNetError::Network(format!("{}: {}", url, e))
        })?;
        self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        let status = resp.status();
        for val in resp.headers().get_all("set-cookie") {
            if let Ok(s) = val.to_str() {
                self.cookie_jar.set_cookie(s, url);
            }
        }
        let response_headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let resp_body = resp
            .bytes()
            .await
            .map_err(|e| ObscuraNetError::Network(format!("Failed to read body: {}", e)))?
            .to_vec();

        Ok(Response {
            url: url.clone(),
            status: status.as_u16(),
            headers: response_headers,
            body: resp_body,
            redirected_from: Vec::new(),
        })
    }

    /// One request with no redirect following, returning the response body as
    /// a byte stream. Mirrors `send_single` (cookies read/written from the
    /// jar, extra + request headers applied) but yields body chunks
    /// incrementally so NDJSON/SSE responses can be parsed line-by-line as
    /// they arrive. Used by Cloudflare-gated providers whose streaming
    /// endpoint is fingerprinted too, so the whole connection must carry the
    /// same Chrome TLS emulation as the initial request.
    pub async fn send_single_streaming(
        &self,
        method: &str,
        url: &Url,
        headers: &HashMap<String, String>,
        body: &str,
    ) -> Result<StreamingResponse, ObscuraNetError> {
        if let Some(host) = url.host_str() {
            if crate::blocklist::is_blocked(host) {
                tracing::debug!("Blocked tracker: {}", url);
                return Ok(StreamingResponse {
                    status: 0,
                    headers: HashMap::new(),
                    bytes: Box::pin(futures::stream::empty()),
                });
            }
        }

        let req_method = method
            .parse::<wreq::Method>()
            .map_err(|e| ObscuraNetError::Network(format!("invalid method '{}': {}", method, e)))?;
        let mut req = self.client.request(req_method, url.as_str());

        let cookie_header = self.cookie_jar.get_cookie_header(url);
        if !cookie_header.is_empty() {
            req = req.header("cookie", &cookie_header);
        }
        for (k, v) in self.extra_headers.read().await.iter() {
            req = req.header(k.as_str(), v.as_str());
        }
        for (k, v) in headers.iter() {
            req = req.header(k.as_str(), v.as_str());
        }
        if !body.is_empty() {
            req = req.body(body.to_string());
        }

        self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let resp = req.send().await.map_err(|e| {
            self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            ObscuraNetError::Network(format!("{}: {}", url, e))
        })?;
        self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        let status = resp.status();
        for val in resp.headers().get_all("set-cookie") {
            if let Ok(s) = val.to_str() {
                self.cookie_jar.set_cookie(s, url);
            }
        }
        let response_headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let bytes = resp
            .bytes_stream()
            .map(|item| {
                item.map(|b| b.to_vec())
                    .map_err(|e| ObscuraNetError::Network(format!("Failed to read body: {}", e)))
            })
            .boxed();

        Ok(StreamingResponse {
            status: status.as_u16(),
            headers: response_headers,
            bytes,
        })
    }

    pub async fn set_extra_headers(&self, headers: HashMap<String, String>) {
        *self.extra_headers.write().await = headers;
    }

    pub fn active_requests(&self) -> u32 {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_network_idle(&self) -> bool {
        self.active_requests() == 0
    }

    /// Get the `Cookie` header value for the given URL from this client's
    /// cookie jar. Returns an empty string if no cookies match.
    pub fn cookie_header_for_url(&self, url: &Url) -> String {
        self.cookie_jar.get_cookie_header(url)
    }

    /// Get the current extra headers map. Returns a snapshot at call time.
    pub async fn get_extra_headers(&self) -> HashMap<String, String> {
        self.extra_headers.read().await.clone()
    }
}
