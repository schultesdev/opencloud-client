//! [`RemoteStorage`] for the WebDAV client: maps the provider-neutral
//! operations of the sync engine onto PROPFIND, GET, PUT, MKCOL and
//! DELETE, including the WebDAV conventions the engine must not know
//! about (405 on MKCOL = exists, 404 on DELETE = gone).

use std::path::Path;

use futures_util::{FutureExt, StreamExt, TryStreamExt};
use oc_storage::{
    BoxFuture, ByteRange, Download, Listing, ProgressFn, RemoteEntry, RemoteStorage, StorageError,
    StorageResult, WriteCondition, normalize_nfc,
};
use reqwest::StatusCode;

use crate::dav::{DavResource, Depth, WebDavClient};
use crate::error::ApiError;
use crate::token::TokenProvider;

impl From<DavResource> for RemoteEntry {
    fn from(resource: DavResource) -> Self {
        let sha1 = resource.sha1();
        Self {
            path: resource.path,
            server_path: resource.server_path,
            name: resource.name,
            is_dir: resource.is_dir,
            size: resource.size,
            etag: resource.etag,
            sha1,
        }
    }
}

/// The NFC comparison key of a request path (segments without empty
/// parts), as [`DavResource::path`] spells it.
fn path_key(path: &str) -> String {
    let joined: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    normalize_nfc(&joined.join("/"))
}

/// Splits a Depth-1 PROPFIND answer into the folder itself and its
/// children. The folder is identified by its path; servers list it first,
/// which is the fallback.
pub(crate) fn split_listing(path: &str, resources: Vec<DavResource>) -> StorageResult<Listing> {
    if resources.is_empty() {
        return Err(StorageError::protocol(format!(
            "PROPFIND answer for '{path}' lists no resource"
        )));
    }
    let key = path_key(path);
    let index = resources
        .iter()
        .position(|resource| resource.path == key)
        .unwrap_or(0);
    let mut children: Vec<RemoteEntry> = Vec::with_capacity(resources.len() - 1);
    let mut dir = None;
    for (position, resource) in resources.into_iter().enumerate() {
        if position == index {
            dir = Some(RemoteEntry::from(resource));
        } else {
            children.push(resource.into());
        }
    }
    let dir = dir.ok_or_else(|| StorageError::protocol("PROPFIND listing lost its folder"))?;
    Ok(Listing { dir, children })
}

/// The `ETag` header of a response, if present.
pub(crate) fn response_etag(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Wraps a GET response into a [`Download`].
pub(crate) fn into_download(response: reqwest::Response, ranged: bool) -> Download {
    // A server that ignores `Range` answers 200 with the whole body.
    let partial = ranged && response.status() != StatusCode::OK;
    let etag = response_etag(&response);
    let body = response
        .bytes_stream()
        .map_err(|error| StorageError::from(ApiError::from(error)))
        .boxed();
    Download::new(etag, partial, body)
}

impl<P: TokenProvider> WebDavClient<P> {
    /// [`RemoteStorage::stat`] on the plain WebDAV client.
    pub(crate) async fn stat_entry(&self, path: &str) -> StorageResult<Option<RemoteEntry>> {
        match self.propfind(path, Depth::Zero).await {
            Ok(resources) => match resources.into_iter().next() {
                Some(resource) => Ok(Some(resource.into())),
                None => Err(StorageError::protocol(format!(
                    "PROPFIND answer for '{path}' lists no resource"
                ))),
            },
            Err(ApiError::NotFound) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// [`RemoteStorage::upload`] as a single PUT.
    pub(crate) async fn upload_single(
        &self,
        path: &str,
        source: &Path,
        length: u64,
        condition: Option<WriteCondition>,
        progress: ProgressFn,
    ) -> StorageResult<Option<String>> {
        Ok(self
            .put_file(path, source, length, condition, move |sent| progress(sent))
            .await?)
    }
}

impl<P: TokenProvider> RemoteStorage for WebDavClient<P> {
    fn stat<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<Option<RemoteEntry>>> {
        self.stat_entry(path).boxed()
    }

    fn list<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<Listing>> {
        async move {
            let resources = self.propfind(path, Depth::One).await?;
            split_listing(path, resources)
        }
        .boxed()
    }

    fn download<'a>(
        &'a self,
        path: &'a str,
        range: Option<ByteRange>,
        if_match: Option<&'a str>,
    ) -> BoxFuture<'a, StorageResult<Download>> {
        async move {
            let response = self
                .fetch(path, range.map(|r| (r.start, r.end)), if_match)
                .await?;
            Ok(into_download(response, range.is_some()))
        }
        .boxed()
    }

    fn upload<'a>(
        &'a self,
        path: &'a str,
        source: &'a Path,
        length: u64,
        condition: Option<WriteCondition>,
        progress: ProgressFn,
    ) -> BoxFuture<'a, StorageResult<Option<String>>> {
        self.upload_single(path, source, length, condition, progress)
            .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<()>> {
        async move {
            match self.mkcol(path).await {
                // 405 Method Not Allowed: the collection already exists.
                Ok(()) | Err(ApiError::Status { code: 405 }) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        .boxed()
    }

    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<()>> {
        async move {
            match WebDavClient::delete(self, path).await {
                Ok(()) | Err(ApiError::NotFound) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(path: &str, is_dir: bool) -> DavResource {
        DavResource {
            path: path.into(),
            server_path: path.into(),
            name: path.rsplit('/').next().unwrap_or_default().into(),
            is_dir,
            size: None,
            etag: None,
            last_modified: None,
            quota_available_bytes: None,
            checksums: Some("SHA1:ABC MD5:x".into()),
        }
    }

    #[test]
    fn listing_identifies_the_folder_by_path() {
        let listing = split_listing(
            "/Ordner/",
            vec![resource("Ordner/a.txt", false), resource("Ordner", true)],
        )
        .expect("listing");
        assert_eq!(listing.dir.path, "Ordner");
        assert_eq!(listing.children.len(), 1);
        assert_eq!(listing.children[0].sha1.as_deref(), Some("abc"));
    }

    #[test]
    fn listing_falls_back_to_the_first_entry() {
        let listing =
            split_listing("", vec![resource("x", true), resource("x/a", false)]).expect("listing");
        assert_eq!(listing.dir.path, "x");
        assert_eq!(listing.children.len(), 1);
    }

    #[test]
    fn empty_listing_is_a_protocol_error() {
        assert!(matches!(
            split_listing("", Vec::new()),
            Err(StorageError::Protocol(_))
        ));
    }
}
