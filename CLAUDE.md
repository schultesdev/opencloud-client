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
- **oc-api** — `GraphClient` (spaces via `/graph/v1.0/me/drives`) and `WebDavClient` (`/dav/spaces/{id}/…`: PROPFIND via quick-xml, GET/PUT with If-Match, MKCOL/MOVE/COPY/DELETE). Auth is decoupled through the `TokenProvider` trait (`token.rs`; `StaticToken` for tests); a 401 triggers one forced refresh + retry. `ApiError` classifies failures (`is_connectivity`, 412/423/429/5xx) and drives retry/backoff and conflict handling.
- **oc-db** — one SQLite DB per job at `<config-dir>/db/<job-id>.sqlite` (`db_path()`), tables `meta`, `entries`, `conflicts`; own migration runner, WAL. `StatusDb` is the DAO.
- **oc-sync** — the engine. `scan` (local scan + staged remote PROPFIND Depth:1 per folder with root-ETag shortcut, strictly serial per job) → `diff` (three-way compare of local/remote against the DB's last synced state → `SyncAction`s) → `transfer` (parallel queue, `.part` downloads with Range chunks and atomic rename). `conflict` implements `resolve()` and the `conflict_policy`. `engine::Engine::start` spawns one tokio task per `Job`, triggered by interval, `watch` (debounced `notify`), or commands; it handles crash recovery, offline backoff, pause/resume.
- **oc-app** — `main.rs` (clap CLI, headless mode, `--resolve` etc.) and `gui/`. The GUI talks to the engine **only** through channels: `gui/backend.rs` owns the `Engine` on the tokio side, receives `UiCommand`s (unbounded tokio mpsc) and pushes `BackendEvent`s through a `std::sync::mpsc` the egui thread polls each frame; a forwarder task relays `EngineEvent`s. No shared mutable state between GUI and engine — keep it that way. `theme.rs` holds the custom dark theme, `wizard.rs` the new-job wizard (space pick + PROPFIND folder tree → `include` list), `tray.rs`/`autostart.rs` are OS integration.

Engine ↔ UI contract: `JobCommand` down (pause/resume/sync now/stop, conflict resolution), `EngineEvent` up (run summaries, `RunProgress`, conflicts, `Offline`/`Online`). Adding UI features usually means extending these enums in `oc-sync/src/engine.rs` and handling them in `gui/backend.rs`.

## Testing

Integration tests live in `crates/*/tests/` and use `wiremock` mock servers (e.g. `oc-sync/tests/engine.rs` builds PROPFIND multistatus XML fixtures and drives a real `Engine` via `next_run`) plus `tempfile` directories/DBs. Full OIDC login can't be automated (password grant is disabled on the server); manual milestone tests use the test instance documented in `PLAN.md` ("Testinstanz für manuelle Tests").
