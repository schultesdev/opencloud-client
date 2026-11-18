//! Integration tests for the provider-neutral `RemoteStorage` view of the
//! WebDAV client: WebDAV conventions are absorbed (405 on MKCOL, 404 on
//! DELETE/stat), throttling is classified, downloads report partial
//! bodies and ETags.

// Test helpers may use unwrap/expect just like the test functions.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use oc_api::{StaticToken, WebDavClient};
use oc_storage::{ByteRange, RemoteStorage, StorageError, WriteCondition};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LISTING: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:oc="http://owncloud.org/ns">
 <D:response>
  <D:href>/dav/spaces/sid/Ordner/</D:href>
  <D:propstat><D:prop>
    <D:resourcetype><D:collection/></D:resourcetype>
    <D:getetag>&quot;dir-1&quot;</D:getetag>
  </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/Ordner/a.txt</D:href>
  <D:propstat><D:prop>
    <D:resourcetype/>
    <D:getetag>&quot;a-1&quot;</D:getetag>
    <D:getcontentlength>3</D:getcontentlength>
    <oc:checksums><oc:checksum>SHA1:ABCDEF MD5:x</oc:checksum></oc:checksums>
  </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
</D:multistatus>"#;

fn storage(uri: &str) -> Arc<dyn RemoteStorage> {
    Arc::new(WebDavClient::new(uri, "sid", StaticToken::new("tok")).expect("client"))
}

#[tokio::test]
async fn list_splits_folder_and_children() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/Ordner"))
        .and(header("Depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(LISTING))
        .mount(&server)
        .await;
    let listing = storage(&server.uri()).list("Ordner").await.expect("list");
    assert_eq!(listing.dir.path, "Ordner");
    assert!(listing.dir.is_dir);
    assert_eq!(listing.dir.etag.as_deref(), Some("\"dir-1\""));
    assert_eq!(listing.children.len(), 1);
    let file = &listing.children[0];
    assert_eq!(file.path, "Ordner/a.txt");
    assert_eq!(file.size, Some(3));
    assert_eq!(file.sha1.as_deref(), Some("abcdef"));
}

#[tokio::test]
async fn stat_of_a_missing_entry_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/weg.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert_eq!(storage(&server.uri()).stat("weg.txt").await, Ok(None));
}

#[tokio::test]
async fn existing_folder_and_missing_delete_count_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let storage = storage(&server.uri());
    assert_eq!(storage.create_dir("Ordner").await, Ok(()));
    assert_eq!(storage.delete("weg").await, Ok(()));
}

#[tokio::test]
async fn throttling_is_classified() {
    let server = MockServer::start().await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(425))
        .mount(&server)
        .await;
    let storage = WebDavClient::new(&server.uri(), "sid", StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig {
            max_attempts: 1,
            ..oc_api::RetryConfig::immediate()
        });
    let error = RemoteStorage::create_dir(&storage, "x")
        .await
        .expect_err("throttled");
    assert!(error.is_throttled());
}

#[tokio::test]
async fn ranged_download_reports_partial_body_and_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.bin"))
        .and(header("Range", "bytes=0-1"))
        .and(header("If-Match", "\"e1\""))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("ETag", "\"e1\"")
                .set_body_bytes(b"ab".to_vec()),
        )
        .mount(&server)
        .await;
    let mut download = storage(&server.uri())
        .download(
            "a.bin",
            Some(ByteRange { start: 0, end: 1 }),
            Some("\"e1\""),
        )
        .await
        .expect("download");
    assert!(download.partial);
    assert_eq!(download.etag.as_deref(), Some("\"e1\""));
    let mut body = Vec::new();
    while let Some(chunk) = download.chunk().await.expect("chunk") {
        body.extend_from_slice(&chunk);
    }
    assert_eq!(body, b"ab");
}

#[tokio::test]
async fn ignored_range_is_reported_as_whole_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"abcd".to_vec()))
        .mount(&server)
        .await;
    let download = storage(&server.uri())
        .download("a.bin", Some(ByteRange { start: 0, end: 1 }), None)
        .await
        .expect("download");
    assert!(!download.partial);
}

#[tokio::test]
async fn conditional_upload_maps_412() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(header("If-Match", "\"old\""))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("a.txt");
    std::fs::write(&source, b"abc").expect("write");
    let result = storage(&server.uri())
        .upload(
            "a.txt",
            &source,
            3,
            Some(WriteCondition::IfMatch {
                etag: "\"old\"".into(),
            }),
            Arc::new(|_| {}),
        )
        .await;
    assert_eq!(result, Err(StorageError::PreconditionFailed));
}
