//! Integration tests for the Nextcloud provider against mock servers:
//! OCS user/quota, Basic auth with the app password, the user's WebDAV
//! tree, and chunked uploads (v2) including write conditions and cleanup.

// Test helpers may use unwrap/expect just like the test functions.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use oc_api::{ChunkingConfig, NextcloudProvider, NextcloudStorage, RetryConfig};
use oc_auth::{AppPasswordManager, InMemoryTokenStore, TokenStore};
use oc_storage::{RemoteStorage, StorageError, StorageProvider, WriteCondition};
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const ACCOUNT: &str = "nc";
/// base64("alice:app-secret")
const BASIC: &str = "Basic YWxpY2U6YXBwLXNlY3JldA==";

fn store() -> InMemoryTokenStore {
    let store = InMemoryTokenStore::default();
    store
        .save_refresh_token(
            ACCOUNT,
            r#"{"login_name":"alice","app_password":"app-secret"}"#,
        )
        .expect("seed");
    store
}

fn storage(server: &MockServer, chunking: ChunkingConfig) -> Arc<dyn RemoteStorage> {
    let manager = Arc::new(AppPasswordManager::new(&server.uri(), store(), ACCOUNT).expect("mgr"));
    Arc::new(
        NextcloudStorage::new(&server.uri(), "alice", manager)
            .expect("storage")
            .with_retry(RetryConfig::immediate())
            .with_chunking(chunking),
    )
}

/// One received chunk: its name and body.
type Chunk = (String, Vec<u8>);

/// Chunk everything from 4 bytes on, in 4-byte chunks.
const TINY_CHUNKS: ChunkingConfig = ChunkingConfig {
    threshold: 4,
    chunk_size: 4,
};

fn file_propfind(etag: &str) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response>
        <D:href>/remote.php/dav/files/alice/big.bin</D:href>
        <D:propstat><D:prop><D:resourcetype/><D:getetag>{etag}</D:getetag>
        <D:getcontentlength>3</D:getcontentlength></D:prop>
        <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}

fn source_file(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("big.bin");
    std::fs::write(&source, content).expect("write");
    (dir, source)
}

#[tokio::test]
async fn roots_come_from_the_ocs_user() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ocs/v2.php/cloud/user"))
        .and(header("OCS-APIRequest", "true"))
        .and(header("Authorization", BASIC))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ocs": {"meta": {"status": "ok"}, "data": {
                "id": "alice", "display-name": "Alice",
                "quota": {"used": 10, "total": 100, "quota": 100}
            }}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = NextcloudProvider::new(&server.uri(), store(), ACCOUNT).expect("provider");
    assert!(provider.has_stored_login());
    let roots = provider.roots().await.expect("roots");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].id, "alice");
    assert_eq!(roots[0].name, "Alice");
    let quota = roots[0].quota.clone().expect("quota");
    assert_eq!((quota.used, quota.total), (Some(10), Some(100)));
}

#[tokio::test]
async fn listing_uses_the_users_file_tree_with_basic_auth() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/remote.php/dav/files/alice/Fotos"))
        .and(header("Authorization", BASIC))
        .and(header("Depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            r#"<d:multistatus xmlns:d="DAV:">
             <d:response><d:href>/remote.php/dav/files/alice/Fotos/</d:href>
              <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>
              <d:getetag>"f1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
             </d:response>
             <d:response><d:href>/remote.php/dav/files/alice/Fotos/Urlaub%202026/</d:href>
              <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>
              </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
             </d:response></d:multistatus>"#,
        ))
        .mount(&server)
        .await;
    let listing = storage(&server, ChunkingConfig::default())
        .list("Fotos")
        .await
        .expect("list");
    assert_eq!(listing.dir.etag.as_deref(), Some("\"f1\""));
    assert_eq!(listing.children[0].path, "Fotos/Urlaub 2026");
}

#[tokio::test]
async fn small_files_use_a_single_put() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/remote.php/dav/files/alice/klein.txt"))
        .and(header("Authorization", BASIC))
        .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"k1\""))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, source) = source_file(b"abc");
    let etag = storage(&server, TINY_CHUNKS)
        .upload("klein.txt", &source, 3, None, Arc::new(|_| {}))
        .await
        .expect("upload");
    assert_eq!(etag.as_deref(), Some("\"k1\""));
}

#[tokio::test]
async fn large_files_are_uploaded_in_chunks_and_assembled_conditionally() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/remote.php/dav/files/alice/big.bin"))
        .respond_with(ResponseTemplate::new(207).set_body_string(file_propfind("\"old\"")))
        .mount(&server)
        .await;
    let destination = format!("{}/remote.php/dav/files/alice/big.bin", server.uri());
    Mock::given(method("MKCOL"))
        .and(path_regex(
            r"^/remote\.php/dav/uploads/alice/oc-sync-[0-9a-f]{16}/?$",
        ))
        .and(header("Destination", destination.as_str()))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    let chunks: Arc<Mutex<Vec<Chunk>>> = Arc::default();
    let seen = chunks.clone();
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/remote\.php/dav/uploads/alice/oc-sync-[0-9a-f]{16}/\d+$",
        ))
        .and(header("Destination", destination.as_str()))
        .and(header("OC-Total-Length", "10"))
        .respond_with(move |request: &Request| {
            let name = request.url.path().rsplit('/').next().unwrap().to_string();
            seen.lock().unwrap().push((name, request.body.clone()));
            ResponseTemplate::new(201)
        })
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("MOVE"))
        .and(path_regex(
            r"^/remote\.php/dav/uploads/alice/oc-sync-[0-9a-f]{16}/\.file$",
        ))
        .and(header("Destination", destination.as_str()))
        .and(header("OC-Total-Length", "10"))
        .and(header(
            "If",
            format!("<{destination}> ([\"old\"])").as_str(),
        ))
        .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"new\""))
        .expect(1)
        .mount(&server)
        .await;

    let (_dir, source) = source_file(b"0123456789");
    let reported: Arc<Mutex<Vec<u64>>> = Arc::default();
    let progress = reported.clone();
    let etag = storage(&server, TINY_CHUNKS)
        .upload(
            "big.bin",
            &source,
            10,
            Some(WriteCondition::IfMatch {
                etag: "\"old\"".into(),
            }),
            Arc::new(move |sent| progress.lock().unwrap().push(sent)),
        )
        .await
        .expect("upload");
    assert_eq!(etag.as_deref(), Some("\"new\""));
    let chunks = chunks.lock().unwrap().clone();
    assert_eq!(
        chunks,
        vec![
            ("1".to_string(), b"0123".to_vec()),
            ("2".to_string(), b"4567".to_vec()),
            ("3".to_string(), b"89".to_vec()),
        ]
    );
    // Progress counts across chunks.
    assert_eq!(reported.lock().unwrap().last().copied(), Some(10));
}

#[tokio::test]
async fn changed_remote_file_fails_before_any_chunk_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string(file_propfind("\"other\"")))
        .mount(&server)
        .await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&server)
        .await;
    let (_dir, source) = source_file(b"0123456789");
    let result = storage(&server, TINY_CHUNKS)
        .upload(
            "big.bin",
            &source,
            10,
            Some(WriteCondition::IfMatch {
                etag: "\"old\"".into(),
            }),
            Arc::new(|_| {}),
        )
        .await;
    assert_eq!(result, Err(StorageError::PreconditionFailed));
}

#[tokio::test]
async fn create_new_on_an_existing_file_fails_early() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string(file_propfind("\"x\"")))
        .mount(&server)
        .await;
    let (_dir, source) = source_file(b"0123456789");
    let result = storage(&server, TINY_CHUNKS)
        .upload(
            "big.bin",
            &source,
            10,
            Some(WriteCondition::CreateNew),
            Arc::new(|_| {}),
        )
        .await;
    assert_eq!(result, Err(StorageError::PreconditionFailed));
}

#[tokio::test]
async fn rejected_assembly_is_a_conflict_and_cleans_up() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    // Someone created the file between the check and the assembly.
    Mock::given(method("MOVE"))
        .and(header("Overwrite", "F"))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(
            r"^/remote\.php/dav/uploads/alice/oc-sync-[0-9a-f]{16}/?$",
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, source) = source_file(b"0123456789");
    let result = storage(&server, TINY_CHUNKS)
        .upload(
            "big.bin",
            &source,
            10,
            Some(WriteCondition::CreateNew),
            Arc::new(|_| {}),
        )
        .await;
    assert_eq!(result, Err(StorageError::PreconditionFailed));
}

#[tokio::test]
async fn revoked_app_password_is_sent_once_and_then_withheld() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let storage = storage(&server, ChunkingConfig::default());
    assert_eq!(storage.stat("a").await, Err(StorageError::Unauthorized));
    // The next request fails locally, without contacting the server.
    let second = storage.stat("a").await.unwrap_err();
    assert!(matches!(second, StorageError::Auth { .. }), "{second:?}");
}
