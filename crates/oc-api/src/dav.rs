//! WebDAV client for one space: PROPFIND (structure + metadata), GET, PUT
//! (with `If-Match`/`If-None-Match`), MKCOL, MOVE, COPY, DELETE.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, ETAG};
use reqwest::{Method, Response};
use url::Url;

use crate::error::ApiError;
use crate::http::{AuthedHttp, Replay, parse_server_url};
use crate::path::{build_url, encode_segment, href_to_server_path, normalize_nfc};
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
    /// and NFC-normalized, without trailing slash — the comparison key.
    pub path: String,
    /// The same path as the server spells it (percent-decoded, not
    /// normalized). Use it to address the resource in requests: a name
    /// stored in NFD (e.g. uploaded from macOS) is a different URL than
    /// its NFC form. Equal to `path` for NFC names.
    pub server_path: String,
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
    /// Content checksums reported by the server (`oc:checksums`), e.g.
    /// `SHA1:… MD5:… ADLER32:…`; see [`DavResource::sha1`].
    pub checksums: Option<String>,
}

impl DavResource {
    /// The server's SHA1 of the content (lowercase hex), if reported.
    pub fn sha1(&self) -> Option<String> {
        self.checksums
            .as_deref()?
            .split_whitespace()
            .find_map(|entry| {
                let (kind, value) = entry.split_once(':')?;
                kind.eq_ignore_ascii_case("sha1")
                    .then(|| value.to_ascii_lowercase())
            })
    }
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
            .send(Some(self.http.request_timeout()), Replay::Safe, |token| {
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
        self.http
            .send(None, Replay::Safe, |token| {
                self.http.http().get(url.clone()).bearer_auth(token)
            })
            .await
    }

    /// Downloads a byte range of a file (`Range: bytes=<start>-<end>`,
    /// `end` inclusive) — the basis for partial downloads. A server may
    /// ignore the header and answer with the full body (HTTP 200 instead
    /// of 206), so callers must check the response status before
    /// appending the chunk.
    ///
    /// With `if_match`, the range is only served while the file still has
    /// that ETag (`If-Match`); a file changed since the first chunk yields
    /// [`ApiError::PreconditionFailed`] instead of a chunk of another
    /// version.
    pub async fn get_range(
        &self,
        path: &str,
        start: u64,
        end: u64,
        if_match: Option<&str>,
    ) -> Result<Response, ApiError> {
        let url = build_url(&self.base, path)?;
        self.http
            .send(None, Replay::Safe, |token| {
                let request = self
                    .http
                    .http()
                    .get(url.clone())
                    .bearer_auth(token)
                    .header("Range", format!("bytes={start}-{end}"));
                match if_match {
                    Some(etag) => request.header("If-Match", etag),
                    None => request,
                }
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
        self.put_with_progress(path, content, condition, |_| {})
            .await
    }

    /// Uploads a file like [`WebDavClient::put_with`], but streams the
    /// body to the server in chunks and reports live progress: as the
    /// request body is read for sending, `progress` receives the
    /// cumulative byte count of the current attempt. A retried attempt
    /// restarts the count at 0.
    pub async fn put_with_progress(
        &self,
        path: &str,
        content: impl AsRef<[u8]>,
        condition: Option<WriteCondition>,
        progress: impl Fn(u64) + Send + Sync + 'static,
    ) -> Result<Option<String>, ApiError> {
        let url = build_url(&self.base, path)?;
        // One copy into a shared buffer; attempts and chunks slice it.
        let content = bytes::Bytes::copy_from_slice(content.as_ref());
        let progress: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(progress);
        let response = self
            .http
            .send(None, put_replay(&condition), move |token| {
                let request = self.http.http().put(url.clone()).bearer_auth(token);
                apply_condition(request, &condition)
                    .body(progress_body(content.clone(), progress.clone()))
            })
            .await?;
        Ok(etag_header(&response))
    }

    /// Uploads the local file `source` (exactly `length` bytes) to `path`,
    /// streaming it from disk in chunks — the file is never held in
    /// memory as a whole. Each attempt reopens the file; `progress`
    /// receives the cumulative byte count of the current attempt. A file
    /// that shrinks below `length` while being sent fails the attempt.
    /// Returns the new remote ETag, if the server reports one.
    pub async fn put_file(
        &self,
        path: &str,
        source: &std::path::Path,
        length: u64,
        condition: Option<WriteCondition>,
        progress: impl Fn(u64) + Send + Sync + 'static,
    ) -> Result<Option<String>, ApiError> {
        let url = build_url(&self.base, path)?;
        let progress: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(progress);
        let response = self
            .http
            .send(None, put_replay(&condition), move |token| {
                let request = self
                    .http
                    .http()
                    .put(url.clone())
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_LENGTH, length);
                apply_condition(request, &condition).body(file_body(
                    source,
                    length,
                    progress.clone(),
                ))
            })
            .await?;
        Ok(etag_header(&response))
    }

    /// Creates a collection (directory); parent collections must exist.
    pub async fn mkcol(&self, path: &str) -> Result<(), ApiError> {
        let url = build_url(&self.base, path)?;
        let mkcol = webdav_method(b"MKCOL")?;
        self.http
            .send(Some(self.http.request_timeout()), Replay::Safe, |token| {
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
            .send(Some(self.http.request_timeout()), Replay::Safe, |token| {
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
        // A repeated MOVE that already happened fails with 404.
        self.http
            .send(
                Some(self.http.request_timeout()),
                Replay::OnlyUnprocessed,
                |token| {
                    self.http
                        .http()
                        .request(method.clone(), source.clone())
                        .bearer_auth(token)
                        .header("Destination", destination.as_str())
                        .header("Overwrite", overwrite)
                },
            )
            .await?;
        Ok(())
    }

    /// Converts a raw parsed resource into a resolved [`DavResource`]
    /// (href decoded to a space-relative, NFC-normalized path).
    fn resolve(&self, raw: RawResource) -> Result<DavResource, ApiError> {
        let server_path = href_to_server_path(&self.base, &raw.href)?;
        let path = normalize_nfc(&server_path);
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        Ok(DavResource {
            path,
            server_path,
            name,
            is_dir: raw.is_dir,
            size: raw.size,
            etag: raw.etag,
            last_modified: raw.last_modified,
            quota_available_bytes: raw.quota_available_bytes,
            checksums: raw.checksums,
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

/// Adds an optimistic-write condition's header to a PUT request.
fn apply_condition(
    mut request: reqwest::RequestBuilder,
    condition: &Option<WriteCondition>,
) -> reqwest::RequestBuilder {
    match condition {
        Some(WriteCondition::IfMatch { etag }) => request = request.header("If-Match", etag),
        Some(WriteCondition::CreateNew) => request = request.header("If-None-Match", "*"),
        None => {}
    }
    request
}

/// Chunk size of streamed upload bodies.
const UPLOAD_CHUNK: usize = 64 * 1024;

/// How a PUT may be repeated: a conditional one that the server already
/// applied would fail its own precondition and look like a conflict.
fn put_replay(condition: &Option<WriteCondition>) -> Replay {
    if condition.is_some() {
        Replay::OnlyUnprocessed
    } else {
        Replay::Safe
    }
}

/// The `ETag` header of a response, if present.
fn etag_header(response: &Response) -> Option<String> {
    response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// A request body that serves `content` in chunks (zero-copy slices) and
/// reports the cumulative byte count to `progress` per chunk — the basis
/// of live upload progress ([`WebDavClient::put_with_progress`]). Hyper
/// pulls the stream with network backpressure, so the reports follow the
/// actual sending rate. One instance serves exactly one attempt; retries
/// build a fresh one.
fn progress_body(content: bytes::Bytes, progress: Arc<dyn Fn(u64) + Send + Sync>) -> reqwest::Body {
    reqwest::Body::wrap_stream(futures_util::stream::unfold(
        (content, 0usize, progress),
        |(content, offset, progress)| async move {
            if offset >= content.len() {
                None
            } else {
                let end = (offset + UPLOAD_CHUNK).min(content.len());
                let chunk = content.slice(offset..end);
                progress(end as u64);
                Some((
                    Ok::<bytes::Bytes, std::convert::Infallible>(chunk),
                    (content, end, progress),
                ))
            }
        },
    ))
}

/// A request body streaming `length` bytes of the file at `source`, in
/// chunks, with progress like [`progress_body`]. Opening or reading
/// errors (and a file shorter than `length`) fail the attempt.
fn file_body(
    source: &std::path::Path,
    length: u64,
    progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> reqwest::Body {
    use futures_util::StreamExt;
    use tokio::io::AsyncReadExt;
    let file = match std::fs::File::open(source) {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(error) => {
            return reqwest::Body::wrap_stream(futures_util::stream::once(async move {
                Err::<bytes::Bytes, std::io::Error>(error)
            }));
        }
    };
    let stream = futures_util::stream::try_unfold(
        (file, 0u64, progress),
        move |(mut file, sent, progress)| async move {
            if sent >= length {
                return Ok(None);
            }
            let want = (length - sent).min(UPLOAD_CHUNK as u64) as usize;
            let mut buffer = vec![0u8; want];
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "file got shorter while it was uploaded",
                ));
            }
            buffer.truncate(read);
            let sent = sent + read as u64;
            progress(sent);
            Ok(Some((bytes::Bytes::from(buffer), (file, sent, progress))))
        },
    );
    reqwest::Body::wrap_stream(stream.boxed())
}
