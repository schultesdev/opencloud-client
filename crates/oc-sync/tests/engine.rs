//! Integration tests of the sync engine against a mock WebDAV server
//! (`wiremock`): initial full sync, new/changed/deleted files on either
//! side, folder renames, selective-sync filters, chunked range downloads,
//! parallel transfers, conflicts, the root-ETag shortcut, crash recovery,
//! pause/resume/stop, and the filesystem watcher (see `PLAN.md`, Phase 5).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use oc_api::{StaticToken, WebDavClient};
use oc_db::{EntryState, NewEntry, StatusDb};
use oc_sync::{Engine, EngineEvent, EngineOptions, Job, JobCommand, RunSummary};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// --- Mock-WebDAV helpers --------------------------------------------------

/// One `<D:response>` of a PROPFIND multistatus body.
fn response_xml(path: &str, is_dir: bool, etag: Option<&str>, size: Option<u64>) -> String {
    let href = if path.is_empty() {
        "/dav/spaces/sid/".to_owned()
    } else {
        format!("/dav/spaces/sid/{path}")
    };
    let resourcetype = if is_dir {
        "<D:resourcetype><D:collection/></D:resourcetype>"
    } else {
        "<D:resourcetype/>"
    };
    let etag = etag
        .map(|etag| format!("<D:getetag>{}</D:getetag>", etag.replace('"', "&quot;")))
        .unwrap_or_default();
    let size = size
        .map(|size| format!("<D:getcontentlength>{size}</D:getcontentlength>"))
        .unwrap_or_default();
    format!(
        " <D:response>\n  <D:href>{href}</D:href>\n  <D:propstat>\n   \
         <D:prop>\n    {resourcetype}\n    {etag}\n    {size}\n   \
         </D:prop>\n   <D:status>HTTP/1.1 200 OK</D:status>\n  </D:propstat>\n </D:response>"
    )
}

/// A full multistatus body; `(path, is_dir, etag, size)` per response.
fn multistatus(responses: &[(&str, bool, Option<&str>, Option<u64>)]) -> String {
    let body = responses
        .iter()
        .map(|(path, is_dir, etag, size)| response_xml(path, *is_dir, *etag, *size))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <D:multistatus xmlns:D=\"DAV:\">\n{body}\n</D:multistatus>"
    )
}

/// Mounts a PROPFIND mock for `dav_path` (empty = space root) answering
/// with the given multistatus responses.
async fn mount_propfind(
    server: &MockServer,
    dav_path: &str,
    depth: &str,
    responses: &[(&str, bool, Option<&str>, Option<u64>)],
) {
    let url = if dav_path.is_empty() {
        "/dav/spaces/sid/".to_owned()
    } else {
        format!("/dav/spaces/sid/{dav_path}")
    };
    Mock::given(method("PROPFIND"))
        .and(path(url))
        .and(header("Depth", depth))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(responses)))
        .mount(server)
        .await;
}

/// The space root as a Depth:0 listing, ETag `"root-1"`.
const ROOT_ZERO: &[(&str, bool, Option<&str>, Option<u64>)] =
    &[("", true, Some("\"root-1\""), None)];

// --- Engine helpers --------------------------------------------------------

/// Options for deterministic tests: no watcher, a long period, a small
/// download chunk size, and no root-ETag shortcut unless a test asks
/// for it.
fn test_options() -> EngineOptions {
    EngineOptions {
        sync_interval: Duration::from_secs(600),
        max_parallel_transfers: 2,
        watch: false,
        debounce: Duration::from_millis(300),
        download_chunk_size: 4,
        root_etag_shortcut: false,
    }
}

/// Test setup: mock server, local root, and database path for one job.
/// The engine starts only via [`Setup::engine`] — after the test has
/// mounted its mocks and seeded state, keeping runs deterministic.
struct Setup {
    /// Holds the temporary directory (with the local root and the
    /// database file) alive until the test ends; never read.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    server: MockServer,
    root: PathBuf,
    db_path: PathBuf,
    include: Vec<String>,
    exclude: Vec<String>,
}

async fn fixture(include: &[&str], exclude: &[&str]) -> Setup {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("root dir");
    let db_path = dir.path().join("job.sqlite");
    Setup {
        dir,
        server,
        root,
        db_path,
        include: include.iter().map(|pattern| pattern.to_string()).collect(),
        exclude: exclude.iter().map(|pattern| pattern.to_string()).collect(),
    }
}

impl Setup {
    /// Starts the engine over the job (consuming the options).
    fn engine(&self, options: EngineOptions) -> Engine {
        let dav =
            WebDavClient::new(&self.server.uri(), "sid", StaticToken::new("tok")).expect("client");
        let db = StatusDb::open(&self.db_path).expect("db");
        let job =
            Job::new("job-1", &self.root, &self.include, &self.exclude, dav, db).expect("job");
        Engine::start(vec![job], options).expect("engine")
    }
}

/// Waits for the next finished run and returns its summary plus all
/// events seen in between (conflicts, failures).
async fn next_run(engine: &mut Engine) -> (RunSummary, Vec<EngineEvent>) {
    let (summary, events) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            match engine.events_mut().recv().await {
                Some(EngineEvent::RunFinished { summary, .. }) => return (summary, events),
                Some(event) => events.push(event),
                None => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for a run");
    (summary, events)
}

/// Opens the job's database for assertions (WAL allows concurrent
/// readers while the engine holds its connection).
fn open_db(db_path: &Path) -> StatusDb {
    StatusDb::open(db_path).expect("db")
}

/// A synced database entry for a file.
fn seeded_file_entry(path: &str, etag: &str, mtime: i64, size: i64) -> NewEntry {
    NewEntry {
        path: path.to_owned(),
        is_dir: false,
        local_mtime: Some(mtime),
        local_size: Some(size),
        remote_etag: Some(etag.to_owned()),
        remote_size: Some(size),
        checksum: None,
        last_synced_at: Some(1),
        state: EntryState::Synced,
    }
}

// --- Initial full sync ----------------------------------------------------

#[tokio::test]
async fn initial_sync_downloads_remote_files_and_folders() {
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e1\""), Some(5)),
            ("dir", true, None, None),
            ("dir/b.txt", false, Some("\"e2\""), Some(3)),
        ],
    )
    .await;
    mount_propfind(
        &f.server,
        "dir",
        "1",
        &[
            ("dir", true, None, None),
            ("dir/b.txt", false, Some("\"e2\""), Some(3)),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("aaaaa"))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/dir/b.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("bbb"))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.downloads, 2);
    assert_eq!(summary.errors, 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "aaaaa"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("dir/b.txt")).unwrap(),
        "bbb"
    );

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").expect("db query").expect("entry");
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e1\""));
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert!(db.get_entry("dir").expect("db query").is_some());
    assert!(db.get_entry("dir/b.txt").expect("db query").is_some());
    engine.stop().await;
}

// --- New and changed files ------------------------------------------------

#[tokio::test]
async fn new_local_file_is_uploaded_with_create_condition() {
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/c.txt"))
        .and(header("If-None-Match", "*"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_string("")
                .append_header("ETag", "\"c1\""),
        )
        .expect(1)
        .mount(&f.server)
        .await;

    std::fs::write(f.root.join("c.txt"), "new file").expect("write");
    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.uploads, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    let entry = db.get_entry("c.txt").expect("db query").expect("entry");
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"c1\""));
    assert_eq!(entry.entry.state, EntryState::Synced);
    engine.stop().await;
}

#[tokio::test]
async fn local_change_uploads_with_if_match() {
    let mut options = test_options();
    // Keep the chunked path out of this test; the change is about PUT.
    options.download_chunk_size = 1024;
    let f = fixture(&[], &[]).await;

    // Seed the database with the last synced state, then modify the file.
    std::fs::write(f.root.join("a.txt"), "old").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100, 3))
            .expect("seed");
    }
    std::fs::write(f.root.join("a.txt"), "changed").expect("write");

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e1\""), Some(3)),
        ],
    )
    .await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-Match", "\"e1\""))
        .respond_with(ResponseTemplate::new(204).append_header("ETag", "\"e2\""))
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(options);
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.uploads, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").expect("db query").expect("entry");
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e2\""));
    engine.stop().await;
}

#[tokio::test]
async fn remote_change_is_downloaded() {
    let f = fixture(&[], &[]).await;

    std::fs::write(f.root.join("a.txt"), "old a").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime, 5))
            .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e2\""), Some(5)),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("new a"))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.downloads, 1);
    assert_eq!(summary.errors, 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "new a"
    );

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").expect("db query").expect("entry");
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e2\""));
    engine.stop().await;
}

// --- Deletions ------------------------------------------------------------

#[tokio::test]
async fn local_deletion_propagates_to_the_server() {
    let f = fixture(&[], &[]).await;

    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", 100, 5))
            .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e1\""), Some(5)),
        ],
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.deletes, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    assert!(db.get_entry("a.txt").expect("db query").is_none());
    engine.stop().await;
}

#[tokio::test]
async fn remote_deletion_removes_the_local_file() {
    let f = fixture(&[], &[]).await;

    std::fs::write(f.root.join("a.txt"), "local").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime, 5))
            .expect("seed");
    }

    // The server no longer lists a.txt.
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.deletes, 1);
    assert_eq!(summary.errors, 0);
    assert!(!f.root.join("a.txt").exists());

    let db = open_db(&f.db_path);
    assert!(db.get_entry("a.txt").expect("db query").is_none());
    engine.stop().await;
}

// --- Folder rename (delete + re-create) ------------------------------------

#[tokio::test]
async fn local_folder_rename_moves_content_to_the_server() {
    let f = fixture(&[], &[]).await;

    // Last synced state: old/ with old/x.txt; locally renamed to new/.
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&NewEntry {
            path: "old".to_owned(),
            is_dir: true,
            local_mtime: Some(1),
            local_size: None,
            remote_etag: None,
            remote_size: None,
            checksum: None,
            last_synced_at: Some(1),
            state: EntryState::Synced,
        })
        .expect("seed");
        db.upsert_entry(&seeded_file_entry("old/x.txt", "\"x1\"", 100, 1))
            .expect("seed");
    }
    std::fs::create_dir_all(f.root.join("new")).expect("mkdir");
    std::fs::write(f.root.join("new/x.txt"), "x").expect("write");

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("old", true, None, None),
            ("old/x.txt", false, Some("\"x1\""), Some(1)),
        ],
    )
    .await;
    Mock::given(method("MKCOL"))
        .and(path("/dav/spaces/sid/new"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/new/x.txt"))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"x2\""))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/dav/spaces/sid/old"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/dav/spaces/sid/old/x.txt"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.uploads, 1);
    assert_eq!(summary.folders_created, 1);
    assert_eq!(summary.deletes, 2);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    assert!(db.get_entry("new").expect("db query").is_some());
    assert!(db.get_entry("new/x.txt").expect("db query").is_some());
    assert!(db.get_entry("old").expect("db query").is_none());
    assert!(db.get_entry("old/x.txt").expect("db query").is_none());
    engine.stop().await;
}

// --- Selective sync --------------------------------------------------------

#[tokio::test]
async fn exclude_filter_keeps_paths_out_of_the_sync() {
    let f = fixture(&[], &["skip*"]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/keep.txt"))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"k1\""))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/skip.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&f.server)
        .await;

    std::fs::write(f.root.join("skip.txt"), "nope").expect("write");
    std::fs::write(f.root.join("keep.txt"), "yes").expect("write");
    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.uploads, 1);
    assert_eq!(summary.errors, 0);

    let db = open_db(&f.db_path);
    assert!(db.get_entry("skip.txt").expect("db query").is_none());
    assert!(db.get_entry("keep.txt").expect("db query").is_some());
    engine.stop().await;
}

#[tokio::test]
async fn include_whitelist_limits_remote_traversal() {
    let f = fixture(&["keep"], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("keep", true, None, None),
            ("keep/k.txt", false, Some("\"kk\""), Some(1)),
            ("other", true, None, None),
        ],
    )
    .await;
    mount_propfind(
        &f.server,
        "keep",
        "1",
        &[
            ("keep", true, None, None),
            ("keep/k.txt", false, Some("\"kk\""), Some(1)),
        ],
    )
    .await;
    // "other" must not be traversed: a PROPFIND on it would 404 and fail
    // the run.
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/keep/k.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("k"))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.downloads, 1);
    assert_eq!(summary.errors, 0);
    assert!(f.root.join("keep/k.txt").exists());
    assert!(!f.root.join("other").exists());
    engine.stop().await;
}

#[tokio::test]
async fn filter_change_drops_database_entries_but_keeps_files() {
    let f = fixture(&["keep"], &[]).await;

    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("old/x.txt", "\"x1\"", 100, 1))
            .expect("seed");
    }
    std::fs::create_dir_all(f.root.join("old")).expect("mkdir");
    std::fs::write(f.root.join("old/x.txt"), "x").expect("write");

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0);

    let db = open_db(&f.db_path);
    assert!(db.get_entry("old/x.txt").expect("db query").is_none());
    assert!(f.root.join("old/x.txt").exists(), "local file must stay");
    engine.stop().await;
}

// --- Chunked range downloads ----------------------------------------------

#[tokio::test]
async fn large_downloads_arrive_in_range_chunks() {
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("big.bin", false, Some("\"b1\""), Some(10)),
        ],
    )
    .await;
    // chunk_size = 4: ranges bytes=0-3, bytes=4-7, bytes=8-9.
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/big.bin"))
        .and(header("Range", "bytes=0-3"))
        .respond_with(
            ResponseTemplate::new(206)
                .set_body_string("0123")
                .append_header("ETag", "\"b1\""),
        )
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/big.bin"))
        .and(header("Range", "bytes=4-7"))
        .respond_with(ResponseTemplate::new(206).set_body_string("4567"))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/big.bin"))
        .and(header("Range", "bytes=8-9"))
        .respond_with(ResponseTemplate::new(206).set_body_string("89"))
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.downloads, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("big.bin")).unwrap(),
        "0123456789"
    );
    assert!(!f.root.join("big.bin.part").exists());
    engine.stop().await;
}

// --- Conflicts ------------------------------------------------------------

#[tokio::test]
async fn both_sides_changed_records_a_conflict() {
    let f = fixture(&[], &[]).await;

    std::fs::write(f.root.join("a.txt"), "local edit").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100, 3))
            .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e2\""), Some(10)),
        ],
    )
    .await;

    let mut engine = f.engine(test_options());
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 1);
    assert_eq!(summary.errors, 0);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Conflict { path, .. } if path == "a.txt"))
    );

    // The local file is untouched until Phase 6 resolves the conflict.
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "local edit"
    );

    let db = open_db(&f.db_path);
    let conflicts = db.pending_conflicts().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "a.txt");
    let entry = db.get_entry("a.txt").expect("db query").expect("entry");
    assert_eq!(entry.entry.state, EntryState::Conflict);
    engine.stop().await;
}

#[tokio::test]
async fn upload_precondition_failure_becomes_a_conflict() {
    let f = fixture(&[], &[]).await;

    std::fs::write(f.root.join("a.txt"), "changed").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100, 3))
            .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e1\""), Some(3)),
        ],
    )
    .await;
    // Someone else changed the file between scan and PUT: 412.
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-Match", "\"e1\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&f.server)
        .await;
    let mut engine = f.engine(test_options());
    // The engine looks up the current remote state for the conflict row.
    mount_propfind(
        &f.server,
        "a.txt",
        "0",
        &[("a.txt", false, Some("\"e3\""), Some(12))],
    )
    .await;

    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Conflict { path, .. } if path == "a.txt"))
    );

    let db = open_db(&f.db_path);
    let conflicts = db.pending_conflicts().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].remote_etag.as_deref(), Some("\"e3\""));
    engine.stop().await;
}

// --- Root-ETag shortcut ----------------------------------------------------

#[tokio::test]
async fn unchanged_root_etag_skips_the_remote_scan() {
    let mut options = test_options();
    options.root_etag_shortcut = true;
    let f = fixture(&[], &[]).await;

    // First run: full scan, nothing to do (both sides empty).
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    let mut engine = f.engine(options);
    let (first, _) = next_run(&mut engine).await;
    assert!(!first.remote_scan_skipped);
    assert_eq!(first.errors, 0);

    // Second run: only a Depth:0 mock with the same root ETag is mounted.
    // A traversal would 404 and fail the run.
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"l1\""))
        .expect(1)
        .mount(&f.server)
        .await;
    std::fs::write(f.root.join("later.txt"), "later").expect("write");
    engine
        .send("job-1", JobCommand::SyncNow)
        .await
        .expect("sync now");
    let (second, _) = next_run(&mut engine).await;
    assert!(second.remote_scan_skipped);
    assert_eq!(second.uploads, 1);
    assert_eq!(second.errors, 0);
    engine.stop().await;
}

// --- Crash recovery -------------------------------------------------------

#[tokio::test]
async fn interrupted_download_state_is_replanned() {
    let f = fixture(&[], &[]).await;

    // A crash left the entry in `downloading` with the old baseline.
    std::fs::write(f.root.join("a.txt"), "old a").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let db = open_db(&f.db_path);
        let mut entry = seeded_file_entry("a.txt", "\"e1\"", mtime, 5);
        entry.state = EntryState::Downloading;
        db.upsert_entry(&entry).expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e2\""), Some(5)),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("new a"))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.downloads, 1);
    assert_eq!(summary.errors, 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "new a"
    );

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").expect("db query").expect("entry");
    assert_eq!(entry.entry.state, EntryState::Synced);
    engine.stop().await;
}

// --- Parallel transfers ---------------------------------------------------

#[tokio::test]
async fn transfers_run_in_parallel_up_to_the_limit() {
    let mut options = test_options();
    options.max_parallel_transfers = 2;
    options.download_chunk_size = 1024;
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_delay(Duration::from_millis(300))
                .append_header("ETag", "\"p1\""),
        )
        .expect(4)
        .mount(&f.server)
        .await;

    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        std::fs::write(f.root.join(name), "content").expect("write");
    }
    let started = std::time::Instant::now();
    let mut engine = f.engine(options);
    let (summary, _) = next_run(&mut engine).await;
    let elapsed = started.elapsed();
    assert_eq!(summary.uploads, 4);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;
    // Two waves of 300 ms each — a serial run would take 1200 ms.
    assert!(
        elapsed < Duration::from_millis(1100),
        "transfers did not overlap: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(500),
        "limit not respected: {elapsed:?}"
    );
    engine.stop().await;
}

// --- Commands: pause / resume / stop --------------------------------------

#[tokio::test]
async fn pause_holds_runs_and_resume_catches_up() {
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"p1\""))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (first, _) = next_run(&mut engine).await;
    assert_eq!(first.errors, 0);

    engine
        .send("job-1", JobCommand::Pause)
        .await
        .expect("send pause");
    // Give the command time to be processed.
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(f.root.join("paused.txt"), "while paused").expect("write");

    // No watcher, long interval: nothing may trigger a run while paused.
    tokio::time::sleep(Duration::from_millis(500)).await;

    engine
        .send("job-1", JobCommand::Resume)
        .await
        .expect("send resume");
    let (second, _) = next_run(&mut engine).await;
    assert_eq!(second.uploads, 1);
    assert_eq!(second.errors, 0);

    engine.send("job-1", JobCommand::Stop).await.expect("stop");
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match engine.events_mut().recv().await {
                Some(EngineEvent::Stopped { job_id }) => return job_id,
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for stop");
    assert_eq!(stopped, "job-1");
}

// --- Watcher ---------------------------------------------------------------

#[tokio::test]
async fn local_changes_trigger_a_sync_run() {
    let mut options = test_options();
    options.watch = true;
    options.debounce = Duration::from_millis(300);
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"p1\""))
        .expect(1)
        .mount(&f.server)
        .await;

    // Initial run on the empty tree.
    let mut engine = f.engine(options);
    let (first, _) = next_run(&mut engine).await;
    assert_eq!(first.errors, 0);

    std::fs::write(f.root.join("watched.txt"), "watched").expect("write");

    // The watcher must pick the change up after the debounce window; the
    // periodic interval is far too long to interfere.
    let (second, _) = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match engine.events_mut().recv().await {
                Some(EngineEvent::RunFinished { summary, .. }) => {
                    return (summary, Vec::<EngineEvent>::new());
                }
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("watcher did not trigger a run");
    assert_eq!(second.uploads, 1);
    assert_eq!(second.errors, 0);
    f.server.verify().await;
    engine.stop().await;
}

// --- Misc ------------------------------------------------------------------

#[tokio::test]
async fn stop_command_stops_a_job() {
    let f = fixture(&[], &[]).await;
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0);

    engine.send("job-1", JobCommand::Stop).await.expect("stop");
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match engine.events_mut().recv().await {
                Some(EngineEvent::Stopped { job_id }) => return job_id,
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for stop");
    assert_eq!(stopped, "job-1");
}

#[tokio::test]
async fn unknown_job_command_is_rejected() {
    let f = fixture(&[], &[]).await;
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    let mut engine = f.engine(test_options());
    let _ = next_run(&mut engine).await;
    assert!(engine.send("nope", JobCommand::Stop).await.is_err());
    engine.stop().await;
}

/// Guards the events' Display implementation used by the headless runner.
#[test]
fn events_render_readably() {
    let event = EngineEvent::RunFinished {
        job_id: "job-1".into(),
        summary: RunSummary {
            uploads: 2,
            downloads: 1,
            ..RunSummary::default()
        },
    };
    let text = event.to_string();
    assert!(text.contains("[job-1]"), "{text}");
    assert!(text.contains("2 up"), "{text}");
    assert!(text.contains("1 down"), "{text}");
}
