//! WebDAV client for one space: PROPFIND (structure + metadata), GET, PUT
//! (with `If-Match`/`If-None-Match`), MKCOL, MOVE, COPY, DELETE.

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, ETAG};
use reqwest::{Method, Response};
use url::Url;

use crate::error::ApiError;
use crate::http::{AuthedHttp, parse_server_url};
use crate::path::{build_url, encode_segment, href_to_path};
use crate::propfind::{PROPFIND_BODY, RawResource, parse_multistatus};
use crate::retry::RetryConfig;
use crate::token::TokenProvider;

/// PROPFIND `Depth` header values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Only the resource itself.
    Zero,
    /// The resource and its direct children.
    One,
}

impl Depth {
    /// The header value sent to the server.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::One => "1",
        }
    }
}

/// Optimistic-write condition for [`WebDavClient::put_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteCondition {
    /// Only write when the remote ETag still matches the recorded one
    /// (`If-Match`); a mismatch yields [`ApiError::PreconditionFailed`].
    IfMatch {
        /// The recorded remote ETag (quoted form, e.g. `"abc"`).
        etag: String,
    },
    /// Only create the file when it does not yet exist remotely
    /// (`If-None-Match: *`).
    CreateNew,
}

/// One WebDAV resource as returned by [`WebDavClient::propfind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResource {
    /// Path relative to the space root (`""` is the root), percent-decoded
    /// and NFC-normalized, without trailing slash.
    pub path: String,
    /// Last path segment; empty for the space root.
    pub name: String,
    /// Whether the resource is a collection (directory).
    pub is_dir: bool,
    /// Size in bytes (files only, if reported).
    pub size: Option<u64>,
    /// ETag in quoted form, as far as the server provides one.
    pub etag: Option<String>,
    /// `Last-Modified` as HTTP-date string (RFC 7231 IMF-fixdate).
    pub last_modified: Option<String>,
    /// Free space in bytes below the collection, if reported.
    pub quota_available_bytes: Option<u64>,
}

/// WebDAV client scoped to one space.
///
/// Paths are given relative to the space root (`"Ordner/Datei.txt"`), are
/// percent-encoded per segment, and may not contain `.`/`..` segments.
/// Transfers (GET/PUT) use the transfer timeout, metadata requests the
/// request timeout; both are configurable.
#[derive(Debug)]
pub struct WebDavClient<P: TokenProvider> {
    http: AuthedHttp<P>,
    base: Url,
}

impl<P: TokenProvider> WebDavClient<P> {
    /// Creates a client for `https://<server>/dav/spaces/<space_id>/`.
    pub fn new(server_url: &str, space_id: &str, token: P) -> Result<Self, ApiError> {
        let server = parse_server_url(server_url)?;
        let url = format!(
            "{}/dav/spaces/{}/",
            server.as_str().trim_end_matches('/'),
            encode_segment(space_id)
        );
        let base = Url::parse(&url).map_err(|err| ApiError::invalid_url(&url, err.to_string()))?;
        Ok(Self {
            http: AuthedHttp::new(token)?,
            base,
        })
    }

    /// Creates a client from a `root.webDavUrl` of a
    /// [`crate::Space`] — the preferred construction, since the
    /// server URL may differ from the API host.
    pub fn from_webdav_url(webdav_url: &str, token: P) -> Result<Self, ApiError> {
        let mut normalized = webdav_url.trim_end_matches('/').to_string();
        if !normalized.contains("://") {
            return Err(ApiError::invalid_url(
                webdav_url,
                "must be an http(s) URL with a host",
            ));
        }
        normalized.push('/');
        let base = Url::parse(&normalized)
            .map_err(|err| ApiError::invalid_url(webdav_url, err.to_string()))?;
        Ok(Self {
            http: AuthedHttp::new(token)?,
            base,
        })
    }

    /// Overrides the retry policy (default: [`RetryConfig::default`]).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.http = self.http.with_retry(retry);
        self
    }

    /// Overrides the metadata-request and transfer timeouts.
    pub fn with_timeouts(mut self, request: Duration, transfer: Duration) -> Self {
        self.http = self.http.with_timeouts(request, transfer);
        self
    }

    /// The WebDAV base URL of the space.
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// Lists the resource itself and, with [`Depth::One`], its direct
    /// children, with ETag, size, mtime, and collection flag. The first
    /// entry is the requested resource; for the root its `path` is `""`.
    ///
    /// Directory structures are traversed level by level with `Depth::One`
    /// (resource-friendly) rather than `Depth: infinity`.
    pub async fn propfind(&self, path: &str, depth: Depth) -> Result<Vec<DavResource>, ApiError> {
        let url = build_url(&self.base, path)?;
        let propfind = self.propfind_method()?;
        let response = self
            .http
            .send(self.http.request_timeout(), |token| {
                self.http
                    .http()
                    .request(propfind.clone(), url.clone())
                    .bearer_auth(token)
                    .header("Depth", depth.as_str())
                    .header(CONTENT_TYPE, "application/xml")
                    .body(PROPFIND_BODY)
            })
            .await?;
        let body = response.text().await?;
        let raw = parse_multistatus(&body)?;
        raw.into_iter()
            .map(|resource| self.resolve(resource))
            .collect()
    }

    /// Downloads a file; the returned [`Response`] streams the body. The
    /// ETag is available in its `ETag` header.
    pub async fn get(&self, path: &str) -> Result<Response, ApiError> {
        let url = build_url(&self.base, path)?;
        let timeout = self.http.transfer_timeout();
        self.http
            .send(timeout, |token| {
                self.http.http().get(url.clone()).bearer_auth(token)
            })
            .await
    }

    /// Uploads a file without a write condition (overwrite).
    /// Returns the new remote ETag, if the server reports one.
    pub async fn put(
        &self,
        path: &str,
        content: impl AsRef<[u8]>,
    ) -> Result<Option<String>, ApiError> {
        self.put_with(path, content, None).await
    }

    /// Uploads a file with an optional optimistic-write condition
    /// ([`WriteCondition`]). Returns the new remote ETag, if the server
    /// reports one. With [`WriteCondition::IfMatch`], a remotely changed
    /// file yields [`ApiError::PreconditionFailed`] instead of being
    /// overwritten.
    pub async fn put_with(
        &self,
        path: &str,
        content: impl AsRef<[u8]>,
        condition: Option<WriteCondition>,
    ) -> Result<Option<String>, ApiError> {
        let url = build_url(&self.base, path)?;
        let timeout = self.http.transfer_timeout();
        let response = self
            .http
            .send(timeout, |token| {
                let mut request = self.http.http().put(url.clone()).bearer_auth(token);
                match &condition {
                    Some(WriteCondition::IfMatch { etag }) => {
                        request = request.header("If-Match", etag);
                    }
                    Some(WriteCondition::CreateNew) => {
                        request = request.header("If-None-Match", "*");
                    }
                    None => {}
                }
                request.body(content.as_ref().to_vec())
            })
            .await?;
        Ok(response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string))
    }

    /// Creates a collection (directory); parent collections must exist.
    pub async fn mkcol(&self, path: &str) -> Result<(), ApiError> {
        let url = build_url(&self.base, path)?;
        let mkcol = webdav_method(b"MKCOL")?;
        self.http
            .send(self.http.request_timeout(), |token| {
                self.http
                    .http()
                    .request(mkcol.clone(), url.clone())
                    .bearer_auth(token)
            })
            .await?;
        Ok(())
    }

    /// Deletes a file or (recursively) a collection.
    pub async fn delete(&self, path: &str) -> Result<(), ApiError> {
        let url = build_url(&self.base, path)?;
        self.http
            .send(self.http.request_timeout(), |token| {
                self.http.http().delete(url.clone()).bearer_auth(token)
            })
            .await?;
        Ok(())
    }

    /// Moves (renames) a resource to `to` within the same space.
    /// With `overwrite = false` an existing target yields
    /// [`ApiError::Status`] 412.
    pub async fn mv(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ApiError> {
        self.transfer(webdav_method(b"MOVE")?, from, to, overwrite)
            .await
    }

    /// Copies a resource to `to` within the same space.
    /// With `overwrite = false` an existing target yields
    /// [`ApiError::Status`] 412.
    pub async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ApiError> {
        self.transfer(webdav_method(b"COPY")?, from, to, overwrite)
            .await
    }

    /// Shared MOVE/COPY implementation: `Destination` header plus
    /// `Overwrite: T/F`.
    async fn transfer(
        &self,
        method: Method,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), ApiError> {
        let source = build_url(&self.base, from)?;
        let destination = build_url(&self.base, to)?;
        let overwrite = if overwrite { "T" } else { "F" };
        self.http
            .send(self.http.request_timeout(), |token| {
                self.http
                    .http()
                    .request(method.clone(), source.clone())
                    .bearer_auth(token)
                    .header("Destination", destination.as_str())
                    .header("Overwrite", overwrite)
            })
            .await?;
        Ok(())
    }

    /// Converts a raw parsed resource into a resolved [`DavResource`]
    /// (href decoded to a space-relative, NFC-normalized path).
    fn resolve(&self, raw: RawResource) -> Result<DavResource, ApiError> {
        let path = href_to_path(&self.base, &raw.href)?;
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        Ok(DavResource {
            path,
            name,
            is_dir: raw.is_dir,
            size: raw.size,
            etag: raw.etag,
            last_modified: raw.last_modified,
            quota_available_bytes: raw.quota_available_bytes,
        })
    }

    /// Builds the PROPFIND method.
    fn propfind_method(&self) -> Result<Method, ApiError> {
        webdav_method(b"PROPFIND")
    }
}

/// Builds a WebDAV-specific HTTP method (`PROPFIND`, `MKCOL`, ...).
fn webdav_method(name: &[u8]) -> Result<Method, ApiError> {
    Method::from_bytes(name).map_err(|_| {
        ApiError::protocol(format!(
            "invalid WebDAV method '{}'",
            String::from_utf8_lossy(name)
        ))
    })
}
