//! The HTTP surface of the downloader (MVP-602/604/606).
//!
//! Everything above this module talks to the [`Transport`] trait, so the whole
//! fetch flow — resume, digest checks, cleanup, manifests — is exercised
//! offline by fixture transports in tests. [`UreqTransport`] is the only
//! implementation that touches the network, and it is rustls-only: no OpenSSL
//! linkage, no system TLS configuration to get wrong.

use std::io::Read;

use thiserror::Error;

/// Upper bound for control files pulled fully into memory (`SHA512SUMS`,
/// `Release`, detached signatures). The largest real one today is the archive
/// `Release` file at a few hundred kB.
pub const MAX_CONTROL_FILE_LEN: u64 = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("HTTP status {0}")]
    Status(u16),

    #[error("the server ignored the Range request and would restart the transfer")]
    RangeNotHonored,

    #[error("response exceeds the {limit} byte limit")]
    TooLarge { limit: u64 },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

/// A ranged, streaming response body.
pub struct Download {
    /// `true` when the server honored a non-zero `Range` request and the body
    /// therefore continues an existing partial file. `false` means the caller
    /// must restart from byte zero.
    pub resumed: bool,
    /// Total size of the complete resource when the server disclosed it.
    pub total_len: Option<u64>,
    /// The body, starting at the requested offset when `resumed` is `true`.
    pub body: Box<dyn Read + Send>,
}

/// The minimal HTTP surface the downloader needs.
pub trait Transport {
    /// Fetches a small resource into memory, refusing anything over `limit`.
    fn get_all(&self, url: &str, limit: u64) -> Result<Vec<u8>, TransportError>;

    /// Starts a streaming GET. When `offset > 0` the implementation asks for
    /// `Range: bytes=<offset>-` and reports through [`Download::resumed`]
    /// whether the server agreed.
    fn get_range(&self, url: &str, offset: u64) -> Result<Download, TransportError>;

    /// [`Transport::get_all`] with extra request headers.
    ///
    /// It exists for exactly one caller — the GitHub Releases API, which needs
    /// `Authorization` and `Accept` to hand over an asset of a **private**
    /// repository (`apps/entangled/src/artifact.rs`). Debian's mirrors need no
    /// headers at all, which is why this is a defaulted method rather than a
    /// changed signature: every existing implementation, fixtures included,
    /// keeps compiling and simply ignores them.
    fn get_all_with_headers(
        &self,
        url: &str,
        limit: u64,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, TransportError> {
        let _ = headers;
        self.get_all(url, limit)
    }

    /// [`Transport::get_range`] from byte zero, with extra request headers.
    /// Same caller, same reasoning as [`Transport::get_all_with_headers`].
    fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Download, TransportError> {
        let _ = headers;
        self.get_range(url, 0)
    }
}

/// Real network transport: `ureq` over rustls with webpki roots.
///
/// `ureq` is built with `default-features = false, features = ["rustls"]`, which
/// deliberately drops its `gzip` feature: transparent content decoding would make
/// the bytes we hash different from the bytes the signed checksum file describes.
/// Every digest in this crate is taken over the exact wire body.
#[derive(Debug, Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("entangled/", env!("CARGO_PKG_VERSION")))
            // Debian mirrors redirect (deb.debian.org → a CDN node); allow a
            // handful of hops but not an unbounded chain.
            .max_redirects(8)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

fn map_ureq(e: ureq::Error) -> TransportError {
    match e {
        ureq::Error::StatusCode(code) => TransportError::Status(code),
        ureq::Error::Io(io) => TransportError::Io(io),
        other => TransportError::Other(other.to_string()),
    }
}

impl Transport for UreqTransport {
    fn get_all(&self, url: &str, limit: u64) -> Result<Vec<u8>, TransportError> {
        let mut response = self.agent.get(url).call().map_err(map_ureq)?;
        let mut buf = Vec::new();
        // `take(limit + 1)` lets us distinguish "exactly at the limit" from
        // "over the limit" without trusting Content-Length.
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > limit {
            return Err(TransportError::TooLarge { limit });
        }
        Ok(buf)
    }

    fn get_range(&self, url: &str, offset: u64) -> Result<Download, TransportError> {
        let mut request = self.agent.get(url);
        if offset > 0 {
            request = request.header("Range", &format!("bytes={offset}-"));
        }
        let response = request.call().map_err(map_ureq)?;
        let status = response.status().as_u16();
        let resumed = match (offset, status) {
            (0, 200) => false,
            (0, _) => return Err(TransportError::Status(status)),
            (_, 206) => true,
            // 200 for a ranged request means the server sent the whole file.
            (_, 200) => false,
            (_, _) => return Err(TransportError::Status(status)),
        };
        let total_len = content_range_total(&response).or_else(|| {
            response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|len| if resumed { len + offset } else { len })
        });
        Ok(Download {
            resumed,
            total_len,
            body: Box::new(response.into_body().into_reader()),
        })
    }

    fn get_all_with_headers(
        &self,
        url: &str,
        limit: u64,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, TransportError> {
        self.get_all_headers(url, limit, headers)
    }

    fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Download, TransportError> {
        self.get_stream_headers(url, headers)
    }
}

/// The header-carrying halves, kept next to the plain ones so the two cannot
/// drift: `get_all_with_headers` is `get_all` plus headers and nothing else.
impl UreqTransport {
    fn get_all_headers(
        &self,
        url: &str,
        limit: u64,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, TransportError> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.call().map_err(map_ureq)?;
        let mut buf = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > limit {
            return Err(TransportError::TooLarge { limit });
        }
        Ok(buf)
    }

    fn get_stream_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Download, TransportError> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.call().map_err(map_ureq)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(TransportError::Status(status));
        }
        let total_len = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        Ok(Download {
            resumed: false,
            total_len,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

/// Parses the total length out of `Content-Range: bytes 100-999/1000`.
fn content_range_total<B>(response: &ureq::http::Response<B>) -> Option<u64> {
    let value = response.headers().get("content-range")?.to_str().ok()?;
    parse_content_range_total(value)
}

/// Split out from [`content_range_total`] so it can be unit tested without a
/// live `ureq` response.
fn parse_content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    total.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_total_is_parsed() {
        assert_eq!(
            parse_content_range_total("bytes 100-999/1000"),
            Some(1000u64)
        );
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("nonsense"), None);
    }
}
