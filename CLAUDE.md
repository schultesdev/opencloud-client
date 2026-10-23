# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Cross-platform (Windows/macOS/Linux) sync client for [OpenCloud](https://opencloud.eu), written in Rust: JSON configuration, one SQLite status DB per sync job, egui GUI with tray, conflict resolution. `PLAN.md` is the authoritative design document and phase plan (German); phases 0–8 are done, Phase 9 (packaging/distribution) is open. `CHANGELOG.md` gets an entry under `[Unreleased]` for each phase/feature.

## Commands

CI (`.github/workflows/ci.yml`) runs these on Linux/Windows/macOS and fails on any warning:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
RUSTFLAGS="-D warnings" cargo build --workspace --locked
cargo test --workspace --locked
```

Single crate / single test:

```sh
cargo test -p oc-sync                                  # one crate
cargo test -p oc-sync --test engine <test_name>        # one integration test (tests/engine.rs)
cargo test -p oc-db <name_substring>                   # filter by name
```

Run the app (binary `opencloud-sync` in crate `oc-app`):

```sh
cargo run -p oc-app                                   # GUI
cargo run -p oc-app -- --headless                     # engine only, all enabled jobs
cargo run -p oc-app -- --login <ACCOUNT_ID>           # browser OIDC login, refresh token -> OS keyring
cargo run -p oc-app -- --logout <ACCOUNT_ID>          # revoke + remove the refresh token
cargo run -p oc-app -- --status                       # jobs with login state and DB stats
cargo run -p oc-app -- --sync-once                    # one pass per enabled job, non-zero exit on errors
cargo run -p oc-app -- --list-spaces <ACCOUNT_ID>
cargo run -p oc-app -- --conflicts                    # list pending conflicts
cargo run -p oc-app -- --resolve <[job-id:]conflict-id> server|local
cargo run -p oc-app -- --config-dir <DIR> ...         # or env OPENCLOUD_SYNC_CONFIG_DIR
```

`RUST_LOG` overrides `settings.log_level`; logs rotate daily under `<config-dir>/logs/`.

## Lints and conventions

- Workspace lints: `unsafe_code = forbid`, `missing_docs = warn` (→ error in CI, so every public item needs a doc comment), clippy `unwrap_used`/`expect_used` warn. `clippy.toml` allows unwrap/expect in tests only — production code must propagate errors.
- Edition 2024, MSRV 1.85. Shared dependencies are declared in root `[workspace.dependencies]`; crates use `dep.workspace = true`.
- Code, doc comments and identifiers are English; user-facing GUI strings, `PLAN.md`, `CHANGELOG.md` and commit messages are German (commit style: `Phase N: <crate> — <summary>`).
- Linux GUI builds need `libgtk-3-dev` (rfd/egui).

## Architecture

Single process; crates layered bottom-up:

- **oc-config** — `config.json` (accounts + settings) and `jobs.json` (sync jobs: account, space id, local path, `include`/`exclude` filters). Atomic writes (temp + rename), validation, schema migration via `version` field. Config dir from `dirs::config_dir()/opencloud-sync`, overridable.
- **oc-auth** — OIDC discovery, Authorization Code + PKCE with a loopback listener on `127.0.0.1`, `TokenManager` with single-flight refresh; refresh token lives only in the OS keyring (`KeyringStore`), never in JSON.
- **oc-api** — `GraphClient` (spaces via `/graph/v1.0/me/drives`) and `WebDavClient` (`/dav/spaces/{id}/…`: PROPFIND via quick-xml, GET/PUT with If-Match, MKCOL/MOVE/COPY/DELETE). Auth is decoupled through the `TokenProvider` trait (`token.rs`; `StaticToken` for tests; implemented for `Arc<P>` to share one manager); a 401 triggers one refresh + retry. `ApiError` classifies failures (`is_connectivity`, `is_retryable`, `is_unprocessed`, 412/423/429/5xx) and drives retry/backoff and conflict handling; conditional writes and MOVE/COPY are only retried when the server provably did not process them (`http::Replay`). `DavResource::path` is NFC (comparison key), `server_path` the server's spelling (use it for requests). Uploads stream from disk (`put_file`); transfers use an idle timeout, not a total one.
- **oc-db** — one SQLite DB per job at `<config-dir>/db/<job-id>.sqlite` (`db_path()`), tables `meta`, `entries`, `conflicts`, `server_names`; own migration runner (IMMEDIATE transaction, WAL first), `local_mtime` in milliseconds since schema v3. `StatusDb` is the DAO; `transaction()` batches writes, `ensure_identity()` binds a DB to account/space/local root.
- **oc-sync** — the engine. `scan` (local scan + staged remote PROPFIND Depth:1 per folder with root-ETag shortcut, strictly serial per job) → `diff` (three-way compare of local/remote against the DB's last synced state → `SyncAction`s) → `transfer` (parallel queue, `.part` downloads with Range chunks and atomic rename). `conflict` implements `resolve()` and the `conflict_policy`. Safety rules worth keeping: folder deletions never remove new/changed children (`keep_folders_with_content`), frozen paths freeze their subtree, downloads check `LocalGuard` before the rename, the root ETag is only stored after error-free passes, unreadable local paths and case-only name collisions are frozen. `engine::Engine::start` spawns one tokio task per `Job`, triggered by interval, `watch` (debounced `notify`), or commands; it handles crash recovery, offline backoff, pause/resume.
- **oc-app** — `main.rs` (clap CLI, headless mode, `--resolve` etc.) and `gui/`. The GUI talks to the engine **only** through channels: `gui/backend.rs` owns the `Engine` on the tokio side, receives `UiCommand`s (unbounded tokio mpsc) and pushes `BackendEvent`s through a `std::sync::mpsc` the egui thread polls each frame (each send also wakes the GUI via `request_repaint`); a forwarder task relays `EngineEvent`s. Network commands (login, spaces, folder tree, conflict resolution) run as their own tasks; one `TokenManager` per account is shared by all clients (refresh tokens rotate). The GUI never opens the status DBs itself — it requests `RefreshSnapshots` once a second and the backend reads them on a blocking thread. No shared mutable state between GUI and engine — keep it that way. `gui/mod.rs` holds app state, event handling, tray, and the frame loop; each tab renders in its own module (`accounts.rs`, `jobs.rs`, `conflicts.rs`, `settings.rs`, `info.rs` — version/author/license/links come from `[workspace.package]` via `env!`; setting `repository` there activates the GitHub/issue links), the wizard view in `wizard_view.rs`, formatting in `format.rs`. `theme.rs` holds the custom dark theme and the UI building blocks (`content_column` for the centered max-width column every tab renders in, `card`, `pill`, `quota_bar`, `icon_button`, painted `dot` — the bundled fonts lack glyphs like ● and ⋯, so don't use them in labels), `wizard.rs` the new-job wizard state (space pick + PROPFIND folder tree → `include` list), `tray.rs`/`autostart.rs` are OS integration (on Linux the tray runs on its own gtk thread).

Engine ↔ UI contract: `JobCommand` down (pause/resume/sync now/stop, conflict resolution), `EngineEvent` up (run summaries, `RunProgress`, conflicts, `Offline`/`Online`). Adding UI features usually means extending these enums in `oc-sync/src/engine.rs` and handling them in `gui/backend.rs`.

## Testing

Integration tests live in `crates/*/tests/` and use `wiremock` mock servers (e.g. `oc-sync/tests/engine.rs` builds PROPFIND multistatus XML fixtures and drives a real `Engine` via `next_run`) plus `tempfile` directories/DBs. Full OIDC login can't be automated (password grant is disabled on the server); manual milestone tests use the test instance documented in `PLAN.md` ("Testinstanz für manuelle Tests").
