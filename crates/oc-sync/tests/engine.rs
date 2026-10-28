//! Integration tests of the sync engine against a mock WebDAV server
//! (`wiremock`): initial full sync, new/changed/deleted files on either
//! side, folder renames, selective-sync filters, chunked range downloads,
//! parallel transfers, conflicts, the root-ETag shortcut, crash recovery,
//! pause/resume/stop, and the filesystem watcher (see `PLAN.md`, Phase 5) —
//! plus conflict resolution and the conflict policies (Phase 6) and the
//! offline backoff with reconnect (Phase 8).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use oc_api::{RetryConfig, StaticToken, WebDavClient};
use oc_config::ConflictPolicy;
use oc_db::{ConflictResolution, ConflictStatus, EntryState, NewConflict, NewEntry, StatusDb};
use oc_sync::{
    Engine, EngineEvent, EngineOptions, Job, JobCommand, ResolveOutcome, RunSummary, resolve,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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
/// download chunk size, no root-ETag shortcut unless a test asks for
/// it, and a fast offline backoff so connectivity tests stay quick.
fn test_options() -> EngineOptions {
    EngineOptions {
        sync_interval: Duration::from_secs(600),
        max_parallel_transfers: 2,
        watch: false,
        debounce: Duration::from_millis(300),
        download_chunk_size: 4,
        root_etag_shortcut: false,
        conflict_policy: ConflictPolicy::Ask,
        propfind_pause: Duration::ZERO,
        offline_backoff_base: Duration::from_millis(100),
        offline_backoff_cap: Duration::from_millis(400),
        // Every progress report becomes an event, so tests can assert on
        // in-flight snapshots deterministically.
        progress_interval: Duration::ZERO,
        global_exclude: Vec::new(),
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
    root_files: bool,
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
        root_files: true,
    }
}

impl Setup {
    /// Starts the engine over the job (consuming the options).
    fn engine(&self, options: EngineOptions) -> Engine {
        let dav =
            WebDavClient::new(&self.server.uri(), "sid", StaticToken::new("tok")).expect("client");
        let db = StatusDb::open(&self.db_path).expect("db");
        let job = Job::new(
            "job-1",
            &self.root,
            &self.include,
            &self.exclude,
            self.root_files,
            dav,
            db,
        )
        .expect("job");
        Engine::start(vec![job], options).expect("engine")
    }

    /// Starts the engine with a client pointing at `server_url` instead
    /// of the fixture's wiremock server (offline tests). One attempt per
    /// request: on Windows a refused loopback connect takes ~1–2 s (the
    /// stack retransmits the SYN before accepting the RST), so every
    /// retried attempt of a failing offline probe stretches the test's
    /// timing budgets several-fold.
    fn engine_at(&self, server_url: &str, options: EngineOptions) -> Engine {
        let dav = WebDavClient::new(server_url, "sid", StaticToken::new("tok"))
            .expect("client")
            .with_retry(RetryConfig {
                max_attempts: 1,
                ..RetryConfig::immediate()
            });
        let db = StatusDb::open(&self.db_path).expect("db");
        let job = Job::new(
            "job-1",
            &self.root,
            &self.include,
            &self.exclude,
            self.root_files,
            dav,
            db,
        )
        .expect("job");
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
async fn run_reports_live_transfer_progress() {
    let f = fixture(&[], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    for (name, body) in [("a.txt", "aaaa"), ("b.txt", "bb")] {
        Mock::given(method("PUT"))
            .and(path(format!("/dav/spaces/sid/{name}")))
            .and(header("If-None-Match", "*"))
            .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"x\""))
            .expect(1)
            .mount(&f.server)
            .await;
        std::fs::write(f.root.join(name), body).expect("write");
    }

    let mut engine = f.engine(test_options());
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.uploads, 2);
    assert_eq!(summary.errors, 0);

    // The run published its plan (2 files, 6 bytes), the running
    // uploads with their bytes, and the completed state.
    let progress: Vec<oc_sync::RunProgress> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Progress { progress, .. } => Some(progress.clone()),
            _ => None,
        })
        .collect();
    assert!(!progress.is_empty(), "expected progress events");
    assert_eq!(
        progress.first(),
        Some(&oc_sync::RunProgress {
            transfers_total: 2,
            bytes_total: 6,
            ..oc_sync::RunProgress::default()
        })
    );
    let active: Vec<&oc_sync::TransferProgress> =
        progress.iter().flat_map(|p| p.active.iter()).collect();
    assert!(
        active.iter().any(|t| t.path == "a.txt"
            && t.direction == oc_sync::TransferDirection::Upload
            && t.bytes_total == 4
            && t.bytes_done <= 4),
        "expected a.txt among the running uploads: {active:?}"
    );
    assert!(
        active
            .iter()
            .any(|t| t.path == "b.txt" && t.bytes_total == 2),
        "expected b.txt among the running uploads: {active:?}"
    );
    // Bytes never regress while transfers finish concurrently.
    assert!(
        progress
            .windows(2)
            .all(|w| w[0].bytes_done <= w[1].bytes_done),
        "bytes_done regressed: {progress:?}"
    );
    assert_eq!(
        progress.last(),
        Some(&oc_sync::RunProgress {
            transfers_total: 2,
            transfers_done: 2,
            transfers_failed: 0,
            bytes_total: 6,
            bytes_done: 6,
            active: vec![],
        })
    );
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
        .as_millis() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100_000, 3))
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
        .as_millis() as i64;
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
        .as_millis() as i64;
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

// --- Remote scan failures ---------------------------------------------------

#[tokio::test]
async fn server_error_on_folder_listing_freezes_it_and_syncs_the_rest() {
    let f = fixture(&[], &[]).await;

    // A synced file below the folder whose listing will fail; without
    // the freeze it would look remotely deleted and be removed.
    std::fs::create_dir_all(f.root.join("big")).expect("mkdir");
    std::fs::write(f.root.join("big/x.txt"), "x").expect("write");
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&NewEntry::new("big", true, EntryState::Synced))
            .expect("seed dir");
        db.upsert_entry(&seeded_file_entry("big/x.txt", "\"e-x\"", 1, 1))
            .expect("seed file");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"e-a\""), Some(3)),
            ("big", true, Some("\"e-big\""), None),
        ],
    )
    .await;
    // The listing of `big` fails server-side (e.g. a directory with
    // more entries than the server's internal gRPC message limit).
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/big"))
        .and(header("Depth", "1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("aaa"))
        .mount(&f.server)
        .await;

    // Immediate retries keep the run fast despite the failing listing.
    let dav = WebDavClient::new(&f.server.uri(), "sid", StaticToken::new("tok"))
        .expect("client")
        .with_retry(RetryConfig::immediate());
    let db = StatusDb::open(&f.db_path).expect("db");
    let job = Job::new(
        "job-1",
        &f.root,
        &f.include,
        &f.exclude,
        f.root_files,
        dav,
        db,
    )
    .expect("job");
    let mut engine = Engine::start(vec![job], test_options()).expect("engine");

    let (summary, events) = next_run(&mut engine).await;
    // The rest of the space syncs.
    assert_eq!(summary.downloads, 1);
    assert_eq!(summary.deletes, 0);
    // The failed folder is reported and its subtree stays untouched.
    assert_eq!(summary.errors, 1);
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Failed { path, error, .. }
            if path == "big" && error.contains("server error (500)")
    )));
    assert_eq!(
        std::fs::read_to_string(f.root.join("big/x.txt")).unwrap(),
        "x"
    );
    let db = open_db(&f.db_path);
    assert!(db.get_entry("big/x.txt").expect("db query").is_some());
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
async fn global_exclude_applies_to_jobs_without_own_excludes() {
    // The job itself has no exclude patterns; the engine options carry
    // the global ones, which must be merged in on start.
    let f = fixture(&[], &[]).await;

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
    let mut options = test_options();
    options.global_exclude = vec!["skip*".into()];
    let mut engine = f.engine(options);
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
async fn filter_change_deletes_synced_local_copies_not_the_cloud() {
    let f = fixture(&["keep"], &[]).await;

    std::fs::create_dir_all(f.root.join("old")).expect("mkdir");
    std::fs::write(f.root.join("old/x.txt"), "x").expect("write");
    let mtime = std::fs::metadata(f.root.join("old/x.txt"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .expect("mtime");

    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&NewEntry::new("old", true, EntryState::Synced))
            .expect("seed dir");
        // Baseline matches the local file: it is confirmed synced, so
        // deselecting `old` removes the local copy.
        db.upsert_entry(&seeded_file_entry("old/x.txt", "\"x1\"", mtime, 1))
            .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    // The cloud keeps deselected folders: nothing may ever be deleted
    // on the server for them.
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.deletes, 2); // file + emptied folder
    f.server.verify().await;

    let db = open_db(&f.db_path);
    assert!(db.get_entry("old").expect("db query").is_none());
    assert!(db.get_entry("old/x.txt").expect("db query").is_none());
    assert!(!f.root.join("old").exists(), "deselected folder must go");
    engine.stop().await;
}

#[tokio::test]
async fn reselected_folder_resyncs_despite_unchanged_root_etag() {
    let mut options = test_options();
    options.root_etag_shortcut = true;
    let mut f = fixture(&["keep", "workspace"], &[]).await;

    // The server never changes: the root ETag stays "root-1" through
    // all three passes.
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("keep", true, None, None),
            ("workspace", true, None, None),
        ],
    )
    .await;
    mount_propfind(
        &f.server,
        "keep",
        "1",
        &[
            ("keep", true, None, None),
            ("keep/k.txt", false, Some("\"k1\""), Some(1)),
        ],
    )
    .await;
    mount_propfind(
        &f.server,
        "workspace",
        "1",
        &[
            ("workspace", true, None, None),
            ("workspace/w.txt", false, Some("\"w1\""), Some(1)),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/keep/k.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("k"))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/workspace/w.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("w"))
        .mount(&f.server)
        .await;
    // Deselection must never delete anything on the server.
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&f.server)
        .await;

    // Pass 1: both folders selected, both download.
    let mut engine = f.engine(options.clone());
    let (first, _) = next_run(&mut engine).await;
    assert_eq!(first.errors, 0);
    assert_eq!(first.downloads, 2);
    engine.stop().await;

    // Pass 2: "workspace" deselected — its local copy goes, the DB
    // entries are dropped, the cloud keeps everything.
    f.include = vec!["keep".to_owned()];
    let mut engine = f.engine(options.clone());
    let (second, _) = next_run(&mut engine).await;
    assert_eq!(second.errors, 0);
    assert_eq!(second.deletes, 2); // file + emptied folder
    assert!(!f.root.join("workspace").exists());
    engine.stop().await;

    // Pass 3: "workspace" reselected. The root ETag is unchanged, but
    // the filter changed since it was recorded: the shortcut would
    // rebuild the remote view from the DB — which no longer knows the
    // folder — and never download it again. The pass must traverse.
    f.include = vec!["keep".to_owned(), "workspace".to_owned()];
    let mut engine = f.engine(options);
    let (third, _) = next_run(&mut engine).await;
    assert_eq!(third.errors, 0);
    assert!(!third.remote_scan_skipped);
    assert_eq!(third.downloads, 1);
    assert_eq!(
        std::fs::read_to_string(f.root.join("workspace/w.txt")).expect("downloaded"),
        "w"
    );
    f.server.verify().await;
    engine.stop().await;
}

#[tokio::test]
async fn filter_change_keeps_new_and_changed_local_content() {
    let f = fixture(&["keep"], &[]).await;

    std::fs::create_dir_all(f.root.join("old/empty")).expect("mkdir");
    std::fs::write(f.root.join("old/x.txt"), "changed").expect("write");
    // Never uploaded: the sync does not know this file at all.
    std::fs::write(f.root.join("old/new.txt"), "new").expect("write");
    let mtime = std::fs::metadata(f.root.join("old/x.txt"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .expect("mtime");

    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&NewEntry::new("old", true, EntryState::Synced))
            .expect("seed dir");
        db.upsert_entry(&NewEntry::new("old/empty", true, EntryState::Synced))
            .expect("seed dir");
        // Baseline behind the local mtime: the file was changed locally,
        // so its content only exists here — it must not be deleted.
        db.upsert_entry(&seeded_file_entry(
            "old/x.txt",
            "\"x1\"",
            mtime - 100_000,
            3,
        ))
        .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0);
    // Only the empty folder disappears; everything else stays.
    assert_eq!(summary.deletes, 1);

    let db = open_db(&f.db_path);
    for path in ["old", "old/empty", "old/x.txt"] {
        assert!(db.get_entry(path).expect("db query").is_none());
    }
    assert!(f.root.join("old/x.txt").exists(), "changed file must stay");
    assert!(
        f.root.join("old/new.txt").exists(),
        "untracked file must stay"
    );
    assert!(!f.root.join("old/empty").exists(), "empty folder must go");
    engine.stop().await;
}

#[tokio::test]
async fn root_files_toggle_off_deletes_synced_root_file_locally() {
    let mut f = fixture(&["keep"], &[]).await;
    f.root_files = false;

    std::fs::write(f.root.join("a.txt"), "a").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .expect("mtime");
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"a1\"", mtime, 1))
            .expect("seed");
    }

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.deletes, 1);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    assert!(db.get_entry("a.txt").expect("db query").is_none());
    assert!(!f.root.join("a.txt").exists(), "root file must go locally");
    engine.stop().await;
}

#[tokio::test]
async fn root_files_toggle_on_syncs_root_files() {
    let f = fixture(&["keep"], &[]).await;

    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("a.txt", false, Some("\"a1\""), Some(1)),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("a")
                .append_header("ETag", "\"a1\""),
        )
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.downloads, 1);
    f.server.verify().await;
    assert_eq!(std::fs::read_to_string(f.root.join("a.txt")).unwrap(), "a");
    assert!(
        open_db(&f.db_path)
            .get_entry("a.txt")
            .expect("db query")
            .is_some()
    );
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
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.downloads, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("big.bin")).unwrap(),
        "0123456789"
    );
    assert!(!f.root.join("big.bin.part").exists());

    // The live view saw the download in flight, with per-chunk bytes.
    let seen: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Progress { progress, .. } => Some(progress),
            _ => None,
        })
        .flat_map(|p| p.active.iter())
        .filter(|t| t.path == "big.bin" && t.direction == oc_sync::TransferDirection::Download)
        .map(|t| (t.bytes_done, t.bytes_total))
        .collect();
    assert!(
        seen.contains(&(4, 10)) || seen.contains(&(8, 10)),
        "expected a mid-download snapshot of big.bin: {seen:?}"
    );
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
        .as_millis() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100_000, 3))
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
        .as_millis() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100_000, 3))
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

#[tokio::test]
async fn upload_precondition_failure_with_indeterminate_remote_state_is_not_a_conflict() {
    let f = fixture(&[], &[]).await;

    std::fs::write(f.root.join("a.txt"), "changed").expect("write");
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .expect("meta")
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let db = open_db(&f.db_path);
        db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100_000, 3))
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
    // The lookup of the current remote state fails — the server is
    // throttling. That must not read as "deleted remotely" and freeze
    // the path behind a conflict: the next pass re-plans it.
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("Depth", "0"))
        .respond_with(ResponseTemplate::new(425))
        .expect(3)
        .mount(&f.server)
        .await;
    let mut engine = f.engine(test_options());

    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 0);
    assert_eq!(summary.errors, 1);
    f.server.verify().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Failed { path, .. } if path == "a.txt"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::Conflict { .. }))
    );

    let db = open_db(&f.db_path);
    assert!(db.pending_conflicts().expect("conflicts").is_empty());
    // The entry is back to a clean state; the next pass re-plans it.
    let entry = db.get_entry("a.txt").expect("entry").expect("present");
    assert_eq!(entry.entry.state, EntryState::Synced);
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
        .as_millis() as i64;
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

// --- Conflict resolution (Phase 6) -----------------------------------------

/// A WebDAV client plus the seeded job database of a fixture.
struct DirectJob {
    dav: WebDavClient<StaticToken>,
    db: std::sync::Mutex<StatusDb>,
}

/// Opens the fixture's database, seeds a conflicted file entry and its
/// conflict row, and returns both with a client. The local file is
/// written by the test beforehand.
fn seeded_conflict(f: &Setup, remote_etag: Option<&str>) -> (DirectJob, i64) {
    let dav = WebDavClient::new(&f.server.uri(), "sid", StaticToken::new("tok")).unwrap();
    let db = StatusDb::open(&f.db_path).unwrap();
    let mut entry = seeded_file_entry("a.txt", "\"e1\"", 100, 10);
    entry.state = EntryState::Conflict;
    entry.remote_etag = remote_etag.map(str::to_owned);
    db.upsert_entry(&entry).unwrap();
    let conflict_id = db
        .record_conflict(&NewConflict {
            path: "a.txt".to_owned(),
            detected_at: 200,
            local_mtime: Some(200),
            local_size: Some(10),
            remote_etag: remote_etag.map(str::to_owned),
            // A deleted remote is recorded without ETag and size.
            remote_size: remote_etag.map(|_| 10),
            remote_mtime: None,
        })
        .unwrap();
    (
        DirectJob {
            dav,
            db: std::sync::Mutex::new(db),
        },
        conflict_id,
    )
}

#[tokio::test]
async fn resolve_server_downloads_and_resolves() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    let (job, conflict_id) = seeded_conflict(&f, Some("\"e2\""));

    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("server edit")
                .append_header("ETag", "\"e2\""),
        )
        .expect(1)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Server,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    f.server.verify().await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "server edit"
    );

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e2\""));
    assert_eq!(
        db.get_conflict(conflict_id).unwrap().unwrap().status,
        ConflictStatus::ResolvedServer
    );
    assert!(db.pending_conflicts().unwrap().is_empty());
}

#[tokio::test]
async fn resolve_server_accepts_a_remote_deletion() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    // The conflict says the server side is gone (no ETag).
    let (job, conflict_id) = seeded_conflict(&f, None);
    // The server confirms the deletion.
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("Depth", "0"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Server,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    assert!(!f.root.join("a.txt").exists());

    let db = open_db(&f.db_path);
    assert!(db.get_entry("a.txt").unwrap().is_none());
    assert_eq!(
        db.get_conflict(conflict_id).unwrap().unwrap().status,
        ConflictStatus::ResolvedServer
    );
    f.server.verify().await;
}

#[tokio::test]
async fn resolve_server_verifies_the_server_when_the_remote_state_was_never_recorded() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    // A stale row: no recorded remote state (a failed lookup used to
    // erase it), but the file exists on the server. "Server wins" must
    // download it, not delete the local file as a "remote deletion".
    let (job, conflict_id) = seeded_conflict(&f, None);
    mount_propfind(
        &f.server,
        "a.txt",
        "0",
        &[("a.txt", false, Some("\"e9\""), Some(11))],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("server edit"))
        .expect(1)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Server,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "server edit"
    );
    let db = open_db(&f.db_path);
    assert_eq!(
        db.get_conflict(conflict_id).unwrap().unwrap().status,
        ConflictStatus::ResolvedServer
    );
    f.server.verify().await;
}

#[tokio::test]
async fn resolve_local_uploads_with_the_conflict_etag() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    let (job, conflict_id) = seeded_conflict(&f, Some("\"e2\""));

    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-Match", "\"e2\""))
        .respond_with(ResponseTemplate::new(204).append_header("ETag", "\"e3\""))
        .expect(1)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Local,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e3\""));
    assert_eq!(
        db.get_conflict(conflict_id).unwrap().unwrap().status,
        ConflictStatus::ResolvedLocal
    );
}

#[tokio::test]
async fn resolve_local_recreates_a_deleted_remote() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    // The conflict says the server side is gone: the upload recreates it.
    let (job, conflict_id) = seeded_conflict(&f, None);

    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"e1\""))
        .expect(1)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Local,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e1\""));
    assert_eq!(
        db.get_conflict(conflict_id).unwrap().unwrap().status,
        ConflictStatus::ResolvedLocal
    );
}

#[tokio::test]
async fn resolve_local_with_remote_changed_again_stays_pending() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    let (job, conflict_id) = seeded_conflict(&f, Some("\"e2\""));

    // The server changed again since detection: the guarded upload fails.
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-Match", "\"e2\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&f.server)
        .await;
    // The conflict row learns the current remote state.
    mount_propfind(
        &f.server,
        "a.txt",
        "0",
        &[("a.txt", false, Some("\"e3\""), Some(12))],
    )
    .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Local,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::RemoteChanged);
    f.server.verify().await;
    // The local file was not touched.
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "local edit"
    );

    let db = open_db(&f.db_path);
    let conflict = db.get_conflict(conflict_id).unwrap().unwrap();
    assert_eq!(conflict.status, ConflictStatus::Pending);
    assert_eq!(conflict.remote_etag.as_deref(), Some("\"e3\""));
    assert_eq!(conflict.remote_size, Some(12));
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Conflict);
}

#[tokio::test]
async fn resolve_local_keeps_the_recorded_remote_state_when_the_lookup_fails() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    let (job, conflict_id) = seeded_conflict(&f, Some("\"e2\""));

    // The guarded upload fails: the server changed again.
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-Match", "\"e2\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&f.server)
        .await;
    // The lookup of the current remote state fails (throttling): the
    // recorded state must stand — erasing it would read as "deleted
    // remotely" and make "server wins" delete the local file.
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("Depth", "0"))
        .respond_with(ResponseTemplate::new(425))
        .expect(3)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Local,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::RemoteChanged);
    f.server.verify().await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "local edit"
    );

    let db = open_db(&f.db_path);
    let conflict = db.get_conflict(conflict_id).unwrap().unwrap();
    assert_eq!(conflict.status, ConflictStatus::Pending);
    assert_eq!(conflict.remote_etag.as_deref(), Some("\"e2\""));
    assert_eq!(conflict.remote_size, Some(10));
}

#[tokio::test]
async fn resolve_local_with_local_deleted_deletes_remote() {
    let f = fixture(&[], &[]).await;
    // No local file: keeping the local side propagates the deletion.
    let (job, conflict_id) = seeded_conflict(&f, Some("\"e2\""));

    Mock::given(method("DELETE"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.server)
        .await;

    let outcome = resolve(
        &job.dav,
        &job.db,
        &f.root,
        conflict_id,
        ConflictResolution::Local,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    f.server.verify().await;

    let db = open_db(&f.db_path);
    assert!(db.get_entry("a.txt").unwrap().is_none());
    assert_eq!(
        db.get_conflict(conflict_id).unwrap().unwrap().status,
        ConflictStatus::ResolvedLocal
    );
}

#[tokio::test]
async fn unresolvable_and_unknown_conflicts_are_rejected() {
    let f = fixture(&[], &[]).await;
    let dav = WebDavClient::new(&f.server.uri(), "sid", StaticToken::new("tok")).unwrap();
    let db = StatusDb::open(&f.db_path).unwrap();
    // Folder/file mismatch: entry says folder, no automatic resolution.
    db.upsert_entry(&NewEntry::new("d", true, EntryState::Conflict))
        .unwrap();
    let folder_conflict = db
        .record_conflict(&NewConflict {
            path: "d".to_owned(),
            detected_at: 1,
            local_mtime: None,
            local_size: None,
            remote_etag: None,
            remote_size: None,
            remote_mtime: None,
        })
        .unwrap();
    let db = std::sync::Mutex::new(db);

    assert!(matches!(
        resolve(
            &dav,
            &db,
            &f.root,
            folder_conflict,
            ConflictResolution::Server,
            1024,
        )
        .await
        .unwrap_err(),
        oc_sync::SyncError::UnresolvableConflict { .. }
    ));
    assert!(matches!(
        resolve(&dav, &db, &f.root, 999, ConflictResolution::Server, 1024)
            .await
            .unwrap_err(),
        oc_sync::SyncError::UnknownConflict { id: 999, .. }
    ));
}

// --- Conflict policies (Phase 6) --------------------------------------------

/// Seeds a modify/modify conflict: the entry is synced against `e1`,
/// the local file has been edited since.
fn seed_both_changed(f: &Setup) {
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    let mtime = std::fs::metadata(f.root.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let mtime = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let db = open_db(&f.db_path);
    db.upsert_entry(&seeded_file_entry("a.txt", "\"e1\"", mtime - 100_000, 3))
        .unwrap();
}

/// Mounts the remote listing with a changed `a.txt` (ETag `e2`).
async fn mount_remote_changed(f: &Setup) {
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
}

#[tokio::test]
async fn keep_server_policy_resolves_conflicts_inline() {
    let mut options = test_options();
    options.conflict_policy = ConflictPolicy::KeepServer;
    let f = fixture(&[], &[]).await;
    seed_both_changed(&f);
    mount_remote_changed(&f).await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("server edit")
                .append_header("ETag", "\"e2\""),
        )
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(options);
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 0);
    assert_eq!(summary.auto_resolved, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "server edit"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ConflictResolved { path, resolution, .. }
        if path == "a.txt" && *resolution == ConflictResolution::Server
    )));

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e2\""));
    assert!(db.pending_conflicts().unwrap().is_empty());
    engine.stop().await;
}

#[tokio::test]
async fn keep_local_policy_resolves_conflicts_inline() {
    let mut options = test_options();
    options.conflict_policy = ConflictPolicy::KeepLocal;
    let f = fixture(&[], &[]).await;
    seed_both_changed(&f);
    mount_remote_changed(&f).await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/a.txt"))
        .and(header("If-Match", "\"e2\""))
        .respond_with(ResponseTemplate::new(204).append_header("ETag", "\"e3\""))
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(options);
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 0);
    assert_eq!(summary.auto_resolved, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "local edit"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ConflictResolved { path, resolution, .. }
        if path == "a.txt" && *resolution == ConflictResolution::Local
    )));

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"e3\""));
    assert!(db.pending_conflicts().unwrap().is_empty());
    engine.stop().await;
}

#[tokio::test]
async fn conflict_copy_policy_keeps_a_dated_local_copy() {
    let mut options = test_options();
    options.conflict_policy = ConflictPolicy::ConflictCopy;
    let f = fixture(&[], &[]).await;
    seed_both_changed(&f);
    mount_remote_changed(&f).await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("server edit")
                .append_header("ETag", "\"e2\""),
        )
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(options);
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 0);
    assert_eq!(summary.auto_resolved, 1);
    assert_eq!(summary.errors, 0);
    f.server.verify().await;

    // The server version won; the local version survives as a copy.
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "server edit"
    );
    let copies: Vec<_> = std::fs::read_dir(&f.root)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name.contains("(conflicted copy").then_some(name)
        })
        .collect();
    assert_eq!(copies.len(), 1, "exactly one conflicted copy");
    assert_eq!(
        std::fs::read_to_string(f.root.join(&copies[0])).unwrap(),
        "local edit"
    );

    let db = open_db(&f.db_path);
    let entry = db.get_entry("a.txt").unwrap().unwrap();
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert!(db.pending_conflicts().unwrap().is_empty());
    engine.stop().await;
}

// --- Offline backoff & reconnect (Phase 8) ---------------------------------

/// Binds a listener, notes its port, and drops it again — connections
/// to that port then fail with "connection refused", like a switched-off
/// server.
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address").port()
}

/// The `Content-Length` of a request whose header block is given.
fn request_body_len(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

/// Serves a minimal WebDAV on `port`: every PROPFIND answers with the
/// space root only (empty tree, ETag `"root-1"`), every other request
/// (PUT) with `201 Created` + ETag `"created"`. One request per
/// connection; connections are closed afterwards. It replaces the dead
/// port of an offline test with a "reconnected" server.
async fn serve_minimal_dav(port: u16) {
    tokio::spawn(async move {
        // The dead-port window may race with the bind; retry briefly.
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => break listener,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        let propfind_body = multistatus(&[("", true, Some("\"root-1\""), None)]);
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let body = propfind_body.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until headers and (Content-Length) body arrived.
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            if let Some(split) =
                                buffer.windows(4).position(|window| window == b"\r\n\r\n")
                            {
                                let (headers, _) = buffer.split_at(split);
                                let headers = String::from_utf8_lossy(headers);
                                let body_len = request_body_len(&headers);
                                if buffer.len() >= split + 4 + body_len {
                                    break;
                                }
                            }
                        }
                    }
                }
                let response = if buffer.starts_with(b"PROPFIND") {
                    format!(
                        "HTTP/1.1 207 Multi-Status\r\n\
                         Content-Type: application/xml; charset=utf-8\r\n\
                         Connection: close\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    "HTTP/1.1 201 Created\r\nETag: \"created\"\r\n\
                     Connection: close\r\nContent-Length: 0\r\n\r\n"
                        .to_string()
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
}

/// Collects the next `count` Offline events (their retry delays). The
/// budget must cover several failing runs against the dead port: on
/// Windows every refused loopback connect costs ~1–2 s.
async fn offline_retries(engine: &mut Engine, count: usize) -> Vec<Duration> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut retries = Vec::new();
        loop {
            match engine.events_mut().recv().await {
                Some(EngineEvent::Offline { retry_in, .. }) => {
                    retries.push(retry_in);
                    if retries.len() == count {
                        return retries;
                    }
                }
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for offline events")
}

#[tokio::test]
async fn unreachable_server_pauses_with_doubling_backoff() {
    let f = fixture(&[], &[]).await;
    let port = dead_port();
    let mut options = test_options();
    options.offline_backoff_base = Duration::from_millis(100);
    options.offline_backoff_cap = Duration::from_millis(400);
    let mut engine = f.engine_at(&format!("http://127.0.0.1:{port}"), options);

    // The first pass fails: the server is not reachable.
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 1);

    // Follow-up attempts double the delay up to the cap: 100, 200, 400, 400 ms.
    let retries = offline_retries(&mut engine, 4).await;
    assert_eq!(
        retries,
        [
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(400),
        ]
    );

    // An explicit SyncNow cancels the backoff and restarts it at the
    // base delay after the (still failing) run.
    engine
        .send("job-1", JobCommand::SyncNow)
        .await
        .expect("sync now");
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 1);
    let retries = offline_retries(&mut engine, 1).await;
    assert_eq!(retries, [Duration::from_millis(100)]);
    engine.stop().await;
}

#[tokio::test]
async fn throttling_server_aborts_the_pass_and_backs_off() {
    let f = fixture(&[], &[]).await;
    // The server answers everything with 425 (Too Early) — OpenCloud's
    // overload signal, e.g. while a build directory is being synced.
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/"))
        .respond_with(ResponseTemplate::new(425))
        .mount(&f.server)
        .await;
    let mut options = test_options();
    options.offline_backoff_base = Duration::from_millis(100);
    options.offline_backoff_cap = Duration::from_millis(400);
    let mut engine = f.engine(options);

    // The pass aborts on the throttling instead of hammering through,
    // and the job pauses with the offline backoff.
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 1);
    let retries = offline_retries(&mut engine, 1).await;
    assert_eq!(retries, [Duration::from_millis(100)]);
    engine.stop().await;
}

#[tokio::test]
async fn reconnect_emits_online_and_drains_the_queue() {
    let f = fixture(&[], &[]).await;
    let port = dead_port();
    let mut options = test_options();
    options.offline_backoff_base = Duration::from_millis(400);
    options.offline_backoff_cap = Duration::from_millis(400);
    let mut engine = f.engine_at(&format!("http://127.0.0.1:{port}"), options);

    // First pass: the server is down, the job goes into backoff.
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 1);
    let retries = offline_retries(&mut engine, 1).await;
    assert_eq!(retries, [Duration::from_millis(400)]);

    // While the job is paused by the backoff, a local change queues up
    // (nothing syncs yet: the server is still down).
    std::fs::write(f.root.join("queued.txt"), "queued while offline").expect("write");

    // The server comes back before the next retry.
    serve_minimal_dav(port).await;

    // The retry pass syncs the queued file and reports the recovery.
    let (online, summary) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut summary = None;
        loop {
            match engine.events_mut().recv().await {
                Some(EngineEvent::RunFinished { summary: s, .. }) => summary = Some(s),
                Some(EngineEvent::Online { job_id }) => return (job_id, summary),
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for reconnect");
    assert_eq!(online, "job-1");
    let summary = summary.expect("a run finished before the Online event");
    assert_eq!(
        summary.uploads, 1,
        "the queued file was synced: {summary:?}"
    );
    assert_eq!(summary.errors, 0);

    let db = open_db(&f.db_path);
    let entry = db
        .get_entry("queued.txt")
        .expect("db query")
        .expect("entry");
    assert_eq!(entry.entry.state, EntryState::Synced);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"created\""));
    engine.stop().await;
}

// --- Folder deletions vs. new content, root-ETag after failures ------------

#[tokio::test]
async fn remote_folder_deletion_keeps_a_new_local_file_inside() {
    let f = fixture(&[], &[]).await;

    // The server no longer has folder "d"; locally a new file appeared in it.
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("MKCOL"))
        .and(path("/dav/spaces/sid/d"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/spaces/sid/d/new.txt"))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"n1\""))
        .expect(1)
        .mount(&f.server)
        .await;

    std::fs::create_dir_all(f.root.join("d")).expect("mkdir");
    std::fs::write(f.root.join("d/new.txt"), "keep me").expect("write");
    open_db(&f.db_path)
        .upsert_entry(&NewEntry::new("d", true, EntryState::Synced))
        .expect("seed");

    let mut engine = f.engine(test_options());
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0, "events: {events:?}");
    assert_eq!(summary.uploads, 1);
    assert_eq!(
        std::fs::read_to_string(f.root.join("d/new.txt")).expect("kept"),
        "keep me"
    );
    f.server.verify().await;
    engine.stop().await;
}

#[tokio::test]
async fn failed_download_disables_the_root_etag_shortcut() {
    let f = fixture(&[], &[]).await;

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
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&f.server)
        .await;

    let mut options = test_options();
    options.root_etag_shortcut = true;
    let mut engine = f.engine(options);
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 1);
    // The failed path must be retried by a full traversal, so the root
    // ETag is not recorded.
    let db = open_db(&f.db_path);
    assert_eq!(db.meta("space_etag").expect("meta"), None);

    engine
        .send("job-1", JobCommand::SyncNow)
        .await
        .expect("send");
    let (summary, _) = next_run(&mut engine).await;
    assert!(!summary.remote_scan_skipped);
    engine.stop().await;
}

#[tokio::test]
async fn nfd_named_server_folders_are_addressed_by_their_server_spelling() {
    let f = fixture(&[], &[]).await;

    // "Café" stored decomposed (e + U+0301), as macOS clients upload it.
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("Cafe\u{301}", true, Some("\"d\""), None),
        ],
    )
    .await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/Cafe%CC%81"))
        .and(header("Depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&[
            ("Cafe\u{301}", true, Some("\"d\""), None),
            ("Cafe\u{301}/menu.txt", false, Some("\"m1\""), Some(4)),
        ])))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/Cafe%CC%81/menu.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("soup"))
        .expect(1)
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, events) = next_run(&mut engine).await;
    assert_eq!(summary.errors, 0, "events: {events:?}");
    assert_eq!(summary.downloads, 1);
    // Locally the name is NFC.
    assert_eq!(
        std::fs::read_to_string(f.root.join("Caf\u{e9}/menu.txt")).expect("downloaded"),
        "soup"
    );
    let db = open_db(&f.db_path);
    assert_eq!(
        db.server_names()
            .expect("names")
            .get("Caf\u{e9}")
            .map(String::as_str),
        Some("Cafe\u{301}")
    );
    f.server.verify().await;
    engine.stop().await;
}

#[tokio::test]
async fn sync_now_runs_once_while_paused_and_keeps_the_pause() {
    let f = fixture(&[], &[]).await;
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(&f.server, "", "1", &[("", true, Some("\"root-1\""), None)]).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).append_header("ETag", "\"p1\""))
        .mount(&f.server)
        .await;

    let mut options = test_options();
    options.sync_interval = Duration::from_millis(200);
    let mut engine = f.engine(options);
    next_run(&mut engine).await;
    engine
        .send("job-1", JobCommand::Pause)
        .await
        .expect("pause");
    tokio::time::sleep(Duration::from_millis(100)).await;

    std::fs::write(f.root.join("now.txt"), "x").expect("write");
    engine
        .send("job-1", JobCommand::SyncNow)
        .await
        .expect("sync now");
    let (run, _) = next_run(&mut engine).await;
    assert_eq!(run.uploads, 1);

    // Still paused: the short interval triggers no further pass.
    let more = tokio::time::timeout(Duration::from_millis(700), next_run(&mut engine)).await;
    assert!(more.is_err(), "a paused job ran again: {more:?}");
    engine.stop().await;
}

#[tokio::test]
async fn resolve_server_without_remote_etag_downloads_instead_of_deleting() {
    let f = fixture(&[], &[]).await;
    std::fs::write(f.root.join("a.txt"), "local edit").unwrap();
    let dav = WebDavClient::new(&f.server.uri(), "sid", StaticToken::new("tok")).unwrap();
    let db = StatusDb::open(&f.db_path).unwrap();
    // The server reported the file, but without an ETag.
    let conflict_id = db
        .record_conflict(&NewConflict {
            path: "a.txt".to_owned(),
            detected_at: 200,
            local_mtime: Some(200),
            local_size: Some(10),
            remote_etag: None,
            remote_size: Some(6),
            remote_mtime: None,
        })
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/dav/spaces/sid/a.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("server"))
        .expect(1)
        .mount(&f.server)
        .await;

    let db = std::sync::Mutex::new(db);
    let outcome = resolve(
        &dav,
        &db,
        &f.root,
        conflict_id,
        ConflictResolution::Server,
        1024,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ResolveOutcome::Resolved);
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.txt")).unwrap(),
        "server"
    );
    f.server.verify().await;
}

#[tokio::test]
async fn initial_size_matches_are_verified_by_checksum() {
    let f = fixture(&[], &[]).await;
    // Same size on both sides; only "same.txt" has the same content.
    std::fs::write(f.root.join("same.txt"), "abc").expect("write");
    std::fs::write(f.root.join("other.txt"), "xyz").expect("write");
    let abc = "a9993e364706816aba3e25717850c26c9cd0d89d";
    let file = |name: &str, sha1: &str| {
        format!(
            " <D:response>\n  <D:href>/dav/spaces/sid/{name}</D:href>\n  <D:propstat>\n   \
             <D:prop>\n    <D:resourcetype/>\n    <D:getetag>&quot;{name}&quot;</D:getetag>\n    \
             <D:getcontentlength>3</D:getcontentlength>\n    \
             <oc:checksums><oc:checksum>SHA1:{sha1} MD5:x</oc:checksum></oc:checksums>\n   \
             </D:prop>\n   <D:status>HTTP/1.1 200 OK</D:status>\n  </D:propstat>\n </D:response>"
        )
    };
    let body = format!(
        "<?xml version=\"1.0\"?>\n<D:multistatus xmlns:D=\"DAV:\" \
         xmlns:oc=\"http://owncloud.org/ns\">\n{}\n{}\n{}\n</D:multistatus>",
        response_xml("", true, Some("\"root-1\""), None),
        file("same.txt", abc),
        // Any SHA1 other than the one of "xyz": same size, other content.
        file("other.txt", "cb4cc28df0fdbe0ecf9d9662e294b118092a5735"),
    );
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/spaces/sid/"))
        .and(header("Depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(body))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, _) = next_run(&mut engine).await;
    assert_eq!(summary.conflicts, 1);
    let db = open_db(&f.db_path);
    let pending = db.pending_conflicts().expect("conflicts");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].path, "other.txt");
    assert_eq!(
        db.get_entry("same.txt")
            .expect("db")
            .expect("entry")
            .entry
            .state,
        EntryState::Synced
    );
    engine.stop().await;
}

#[tokio::test]
async fn case_only_name_collisions_are_frozen_on_case_folding_filesystems() {
    let f = fixture(&[], &[]).await;
    let folds_case = f.root.with_file_name("ROOT").exists();
    mount_propfind(&f.server, "", "0", ROOT_ZERO).await;
    mount_propfind(
        &f.server,
        "",
        "1",
        &[
            ("", true, Some("\"root-1\""), None),
            ("A.txt", false, Some("\"a1\""), Some(1)),
            ("a.txt", false, Some("\"a2\""), Some(1)),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .mount(&f.server)
        .await;

    let mut engine = f.engine(test_options());
    let (summary, events) = next_run(&mut engine).await;
    if folds_case {
        assert_eq!(summary.downloads, 0, "events: {events:?}");
        assert_eq!(summary.errors, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Failed { error, .. } if error.contains("upper/lower case")
        )));
    } else {
        // A case-sensitive filesystem holds both files.
        assert_eq!(summary.downloads, 2);
    }
    engine.stop().await;
}
