//! Integration tests for the Graph and WebDAV clients against mock
//! servers (`wiremock`): PROPFIND, GET/PUT conditions, MKCOL/MOVE/COPY/
//! DELETE, URL encoding, error mapping, retries, and 401 re-auth.

// Test helpers may use unwrap/expect just like the test functions.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};

use oc_api::{ApiError, Depth, GraphClient, StaticToken, TokenProvider, WebDavClient};
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// PROPFIND response fixture: space root, one folder, two files.
const PROPSTAT_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype><D:collection/></D:resourcetype>
    <D:getetag>&quot;root-1&quot;</D:getetag>
    <D:quota-available-bytes>1073741824</D:quota-available-bytes>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/Dateien%20&amp;%20mehr/</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype><D:collection/></D:resourcetype>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/Dateien%20&amp;%20mehr/Bericht%20Q1.pdf</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype/>
    <D:getetag>&quot;etag-1&quot;</D:getetag>
    <D:getcontentlength>2048</D:getcontentlength>
    <D:getlastmodified>Wed, 21 Oct 2026 10:00:00 GMT</D:getlastmodified>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/Cafe%CC%81.txt</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype/>
    <D:getetag>&quot;etag-2&quot;</D:getetag>
    <D:getcontentlength>3</D:getcontentlength>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#;

fn dav_client(uri: &str) -> WebDavClient<StaticToken> {
    WebDavClient::new(uri, "sid", StaticToken::new("tok")).expect("client")
}

// --- Graph ----------------------------------------------------------------

#[tokio::test]
async fn graph_lists_spaces_with_all_fields() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "value": [
            {
                "id": "storage-users-1$89ad5ad2",
                "name": "Projekt Mars",
                "driveType": "project",
                "driveAlias": "projekt-mars",
                "root": {
                    "id": "storage-users-1$89ad5ad2",
                    "webDavUrl": format!("{}/dav/spaces/storage-users-1$89ad5ad2", server.uri())
                },
                "quota": { "total": 1000, "used": 400, "remaining": 600 }
            },
            {
                "id": "personal-1",
                "name": "Admin",
                "driveType": "personal"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .and(header("Authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;

    let client = GraphClient::new(&server.uri(), StaticToken::new("tok")).expect("client");
    let spaces = client.list_spaces().await.expect("spaces");
    assert_eq!(spaces.len(), 2);

    let mars = &spaces[0];
    assert_eq!(mars.id, "storage-users-1$89ad5ad2");
    assert_eq!(mars.name, "Projekt Mars");
    assert_eq!(mars.drive_type, "project");
    assert_eq!(mars.drive_alias.as_deref(), Some("projekt-mars"));
    assert!(mars.web_dav_url().is_some());
    let quota = mars.quota.as_ref().expect("quota");
    assert_eq!(quota.total, Some(1000));
    assert_eq!(quota.used, Some(400));
    assert_eq!(quota.remaining, Some(600));

    let personal = &spaces[1];
    assert_eq!(personal.drive_type, "personal");
    assert!(personal.quota.is_none());
    assert!(personal.web_dav_url().is_none());
}

#[tokio::test]
async fn graph_maps_403_to_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    let client = GraphClient::new(&server.uri(), StaticToken::new("tok")).expect("client");
    let err = client.list_spaces().await.unwrap_err();
    assert!(matches!(err, ApiError::Forbidden));
    assert!(!err.is_retryable());
}

// --- PROPFIND ---------------------------------------------------------------

#[tokio::test]
async fn propfind_parses_and_decodes_resources() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/"))
        .and(header("Depth", "1"))
        .and(header("Authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(207).set_body_string(PROPSTAT_XML))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let resources = client.propfind("", Depth::One).await.expect("propfind");
    assert_eq!(resources.len(), 4);

    let root = &resources[0];
    assert_eq!(root.path, "");
    assert_eq!(root.name, "");
    assert!(root.is_dir);
    assert_eq!(root.etag.as_deref(), Some("\"root-1\""));
    assert_eq!(root.quota_available_bytes, Some(1_073_741_824));

    let folder = &resources[1];
    assert_eq!(folder.path, "Dateien & mehr");
    assert_eq!(folder.name, "Dateien & mehr");
    assert!(folder.is_dir);

    let file = &resources[2];
    assert_eq!(file.path, "Dateien & mehr/Bericht Q1.pdf");
    assert_eq!(file.name, "Bericht Q1.pdf");
    assert!(!file.is_dir);
    assert_eq!(file.etag.as_deref(), Some("\"etag-1\""));
    assert_eq!(file.size, Some(2048));
    assert_eq!(
        file.last_modified.as_deref(),
        Some("Wed, 21 Oct 2026 10:00:00 GMT")
    );

    // NFD href ("e" + combining acute) arrives NFC-normalized.
    let unicode = &resources[3];
    assert_eq!(unicode.path, "Caf\u{e9}.txt");
    assert_eq!(unicode.name, "Caf\u{e9}.txt");
}

#[tokio::test]
async fn propfind_percent_encodes_path_segments() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path(
            "/dav/spaces/sid/Dateien%20%26%20mehr/Unterlagen/Caf%C3%A9.txt",
        ))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            r#"<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/Dateien%20%26%20mehr/Unterlagen/Caf%C3%A9.txt</D:href>
  <D:propstat>
   <D:prop><D:resourcetype/><D:getetag>&quot;e&quot;</D:getetag></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let resources = client
        .propfind("Dateien & mehr/Unterlagen/Café.txt", Depth::Zero)
        .await
        .expect("propfind");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].path, "Dateien & mehr/Unterlagen/Caf\u{e9}.txt");
}

#[tokio::test]
async fn propfind_maps_404_to_not_found_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/fehlend.txt"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let err = client
        .propfind("fehlend.txt", Depth::Zero)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound));
    assert!(!err.is_retryable());
}

// --- GET / PUT ----------------------------------------------------------------

#[tokio::test]
async fn get_downloads_content() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/Ordner/Datei.txt"))
        .and(header("Authorization", "Bearer tok"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("ETag", "\"etag-7\"")
                .set_body_string("Inhalt"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let response = client.get("Ordner/Datei.txt").await.expect("get");
    assert_eq!(response.headers().get("etag").unwrap(), "\"etag-7\"");
    let body = response.text().await.expect("body");
    assert_eq!(body, "Inhalt");
}

#[tokio::test]
async fn put_with_if_match_sends_header_and_returns_new_etag() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/Ordner/Datei.txt"))
        .and(header("If-Match", "\"etag-alt\""))
        .and(header("Authorization", "Bearer tok"))
        .respond_with(
            ResponseTemplate::new(204)
                .append_header("ETag", "\"etag-neu\"")
                .set_body_string("neuer Inhalt"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let etag = client
        .put_with(
            "Ordner/Datei.txt",
            b"neuer Inhalt",
            Some(oc_api::WriteCondition::IfMatch {
                etag: "\"etag-alt\"".into(),
            }),
        )
        .await
        .expect("put");
    assert_eq!(etag.as_deref(), Some("\"etag-neu\""));
}

#[tokio::test]
async fn put_precondition_failed_signals_remote_change() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(header("If-Match", "\"etag-alt\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let err = client
        .put_with(
            "Datei.txt",
            b"Inhalt",
            Some(oc_api::WriteCondition::IfMatch {
                etag: "\"etag-alt\"".into(),
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::PreconditionFailed));
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn put_create_new_uses_if_none_match_star() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    client
        .put_with("Neu.txt", b"data", Some(oc_api::WriteCondition::CreateNew))
        .await
        .expect("put");
}

#[tokio::test]
async fn put_without_condition_overwrites() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/Datei.txt"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    client.put("Datei.txt", b"data").await.expect("put");
}

#[tokio::test]
async fn put_with_progress_reports_streamed_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/Gross.bin"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let reported = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = reported.clone();
    let highest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let peak = highest.clone();
    client
        .put_with_progress("Gross.bin", vec![7u8; 150_000], None, move |bytes| {
            counter.store(bytes, std::sync::atomic::Ordering::Relaxed);
            peak.fetch_max(bytes, std::sync::atomic::Ordering::Relaxed);
        })
        .await
        .expect("put");
    assert_eq!(
        reported.load(std::sync::atomic::Ordering::Relaxed),
        150_000,
        "the last chunk must report the full size"
    );
    assert_eq!(
        highest.load(std::sync::atomic::Ordering::Relaxed),
        150_000,
        "progress counts grow monotonically"
    );
}

// --- MKCOL / DELETE / MOVE / COPY ---------------------------------------------

#[tokio::test]
async fn mkcol_creates_a_collection() {
    let server = MockServer::start().await;
    Mock::given(method("MKCOL"))
        .and(path("/dav/spaces/sid/Neuer%20Ordner"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    client.mkcol("Neuer Ordner").await.expect("mkcol");
}

#[tokio::test]
async fn delete_remotes_a_file_and_maps_423() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/dav/spaces/sid/Datei.txt"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/dav/spaces/sid/Gesperrt.txt"))
        .respond_with(ResponseTemplate::new(423))
        .expect(1)
        .named("locked")
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    client.delete("Datei.txt").await.expect("delete");
    let err = client.delete("Gesperrt.txt").await.unwrap_err();
    assert!(matches!(err, ApiError::Locked));
}

#[tokio::test]
async fn move_sends_destination_and_overwrite_headers() {
    let server = MockServer::start().await;
    Mock::given(method("MOVE"))
        .and(path("/dav/spaces/sid/Alt.txt"))
        .and(header_regex("Destination", ".*/dav/spaces/sid/Neu\\.txt"))
        .and(header("Overwrite", "F"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    client.mv("Alt.txt", "Neu.txt", false).await.expect("move");
}

#[tokio::test]
async fn copy_sends_destination_header() {
    let server = MockServer::start().await;
    Mock::given(method("COPY"))
        .and(path("/dav/spaces/sid/Vorlage.txt"))
        .and(header_regex("Destination", ".*/dav/spaces/sid/Kopie\\.txt"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    client
        .copy("Vorlage.txt", "Kopie.txt", true)
        .await
        .expect("copy");
}

// --- Retries / re-auth ---------------------------------------------------------

#[tokio::test]
async fn retries_on_server_errors_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{ "id": "s1", "name": "Space", "driveType": "project" }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = GraphClient::new(&server.uri(), StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    let spaces = client.list_spaces().await.expect("spaces");
    assert_eq!(spaces.len(), 1);
}

#[tokio::test]
async fn rate_limit_retry_after_hint_is_honored() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = GraphClient::new(&server.uri(), StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    assert!(client.list_spaces().await.expect("spaces").is_empty());
}

#[tokio::test]
async fn unauthorized_triggers_forced_refresh_and_retry() {
    /// Token provider handing out a stale token, refreshing to a fresh one.
    struct Refreshing {
        refreshes: AtomicUsize,
    }
    impl TokenProvider for Refreshing {
        async fn access_token(&self) -> Result<String, ApiError> {
            Ok("stale".into())
        }
        async fn refresh(&self, rejected: &str) -> Result<String, ApiError> {
            assert_eq!(rejected, "stale");
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok("fresh".into())
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .and(header("Authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .and(header("Authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = Refreshing {
        refreshes: AtomicUsize::new(0),
    };
    let client = GraphClient::new(&server.uri(), provider)
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    assert!(client.list_spaces().await.expect("spaces").is_empty());
}

#[tokio::test]
async fn persistent_unauthorized_surfaces_after_refresh() {
    /// Token provider that always hands out the same token.
    struct AlwaysSame;
    impl TokenProvider for AlwaysSame {
        async fn access_token(&self) -> Result<String, ApiError> {
            Ok("tok".into())
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;

    let client = GraphClient::new(&server.uri(), AlwaysSame)
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    let err = client.list_spaces().await.unwrap_err();
    assert!(matches!(err, ApiError::Unauthorized));
}

// --- client construction -----------------------------------------------------

#[tokio::test]
async fn client_from_webdav_url_uses_the_given_base() {
    let server = MockServer::start().await;
    let webdav_url = format!("{}/dav/spaces/space-id", server.uri());
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/space-id/"))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            r#"<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/space-id/</D:href>
  <D:propstat>
   <D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let client =
        WebDavClient::from_webdav_url(&webdav_url, StaticToken::new("tok")).expect("client");
    let resources = client.propfind("", Depth::Zero).await.expect("propfind");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].path, "");
}

#[test]
fn invalid_server_urls_are_rejected() {
    assert!(WebDavClient::new("no url", "sid", StaticToken::new("t")).is_err());
    assert!(WebDavClient::new("ftp://example.com", "sid", StaticToken::new("t")).is_err());
    assert!(GraphClient::new("", StaticToken::new("t")).is_err());
}

#[tokio::test]
async fn invalid_paths_are_rejected_before_any_request() {
    let client = WebDavClient::new("https://cloud.example.com", "sid", StaticToken::new("t"))
        .expect("client");
    let err = client
        .propfind("../outside", Depth::Zero)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InvalidPath { .. }));
}

#[tokio::test]
async fn get_range_sends_bytes_header_and_accepts_partial_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/big.bin"))
        .and(header("Range", "bytes=0-3"))
        .respond_with(ResponseTemplate::new(206).set_body_string("abcd"))
        .expect(1)
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let response = client
        .get_range("big.bin", 0, 3, None)
        .await
        .expect("range");
    assert_eq!(response.status().as_u16(), 206);
    assert_eq!(response.bytes().await.expect("body"), "abcd");
    server.verify().await;
}

#[tokio::test]
async fn get_range_tolerates_servers_that_ignore_the_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/big.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_string("whole file"))
        .mount(&server)
        .await;

    let client = dav_client(&server.uri());
    let response = client
        .get_range("big.bin", 4, 7, None)
        .await
        .expect("range");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.bytes().await.expect("body"), "whole file");
}

// --- Retry semantics -------------------------------------------------------

/// A raw HTTP server: the first connection is dropped without an answer
/// (a broken transport), the second gets `200 ok`.
async fn flaky_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (first, _) = listener.accept().await.expect("accept");
        drop(first);
        let (mut second, _) = listener.accept().await.expect("accept");
        let mut buffer = [0u8; 4096];
        let _ = second.read(&mut buffer).await;
        let _ = second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    });
    format!("http://{address}")
}

#[tokio::test]
async fn network_errors_are_retried() {
    let server = flaky_server().await;
    let client = WebDavClient::new(&server, "sid", StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    let response = client
        .get("a.txt")
        .await
        .expect("retried after a broken connection");
    assert_eq!(response.text().await.expect("body"), "ok");
}

#[tokio::test]
async fn conditional_put_is_not_repeated_after_an_ambiguous_failure() {
    let server = MockServer::start().await;
    // 502: the proxy may have forwarded the first attempt already; a
    // repetition would fail `If-None-Match` and look like a conflict.
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount(&server)
        .await;
    let client = WebDavClient::new(&server.uri(), "sid", StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    let error = client
        .put_with("a.txt", b"x", Some(oc_api::WriteCondition::CreateNew))
        .await
        .expect_err("must fail");
    assert!(matches!(error, ApiError::Server { code: 502 }));
    server.verify().await;
}

#[tokio::test]
async fn conditional_put_is_repeated_after_503() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(503).append_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"a1\""))
        .expect(1)
        .mount(&server)
        .await;
    let client = WebDavClient::new(&server.uri(), "sid", StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    let etag = client
        .put_with("a.txt", b"x", Some(oc_api::WriteCondition::CreateNew))
        .await
        .expect("retried");
    assert_eq!(etag.as_deref(), Some("\"a1\""));
}

#[tokio::test]
async fn excessive_retry_after_is_not_waited_for() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graph/v1.0/me/drives"))
        .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "86400"))
        .expect(1)
        .mount(&server)
        .await;
    let client = GraphClient::new(&server.uri(), StaticToken::new("tok"))
        .expect("client")
        .with_retry(oc_api::RetryConfig::immediate());
    let started = std::time::Instant::now();
    let error = client.list_spaces().await.expect_err("must fail");
    assert!(matches!(error, ApiError::RateLimited { .. }));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    server.verify().await;
}

#[tokio::test]
async fn put_file_streams_the_file_with_its_length() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/big.bin"))
        .and(header("Content-Length", "200000"))
        .and(header("If-None-Match", "*"))
        .and(wiremock::matchers::body_bytes(vec![7u8; 200_000]))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"b1\""))
        .expect(1)
        .mount(&server)
        .await;
    let dir = std::env::temp_dir().join(format!("oc-api-put-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let source = dir.join("big.bin");
    std::fs::write(&source, vec![7u8; 200_000]).expect("write");

    let client = WebDavClient::new(&server.uri(), "sid", StaticToken::new("tok")).expect("client");
    let reported = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = reported.clone();
    let etag = client
        .put_file(
            "big.bin",
            &source,
            200_000,
            Some(oc_api::WriteCondition::CreateNew),
            move |bytes| seen.store(bytes as usize, Ordering::SeqCst),
        )
        .await
        .expect("upload");
    assert_eq!(etag.as_deref(), Some("\"b1\""));
    assert_eq!(reported.load(Ordering::SeqCst), 200_000);
    let _ = std::fs::remove_dir_all(&dir);
}
