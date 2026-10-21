# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning follows [Semantic Versioning](https://semver.org/).

## Release-Strategie

- Versionierung: SemVer (`MAJOR.MINOR.PATCH`), gepflegt in
  `[workspace.package] version` in `Cargo.toml`.
- Tags: `vX.Y.Z` auf `main`; CI-Builds (Phase 9) hängen an Tags.
- Eine neue Version bekommt einen Abschnitt `[X.Y.Z] - YYYY-MM-DD` mit einem
  Link zum Tag und wird aus `[Unreleased]` herausgezogen.

## [Unreleased]

### Added (Phase 0 — Projekt-Setup)

- Cargo-Workspace mit den sechs Crates `oc-config`, `oc-auth`, `oc-api`,
  `oc-db`, `oc-sync`, `oc-app` (letztere fünf als Platzhalter für die
  Phasen 2–7).
- GitHub-Actions-CI: Build + `cargo test` auf `ubuntu-latest`,
  `windows-latest`, `macos-latest`; `cargo clippy` und Kompilierung mit
  `deny(warnings)`; `cargo fmt --check`.
- `--headless`-Flag in `oc-app` vorgesehen (Engine ohne GUI startbar).
- Versions-/Releasestrategie: SemVer + Tags, Changelog in dieser Datei.

### Added (Phase 1 — Konfiguration, `oc-config`)

- OS-spezifisches Konfigurationsverzeichnis (`dirs::config_dir()` +
  `opencloud-sync`), überschreibbar über `OPENCLOUD_SYNC_CONFIG_DIR`.
- `config.json`: globale Einstellungen + Konten (`Account`, `Settings` mit
  `sync_interval_secs`, `max_parallel_transfers`, `conflict_policy`,
  `log_level`).
- `jobs.json`: beliebig viele Sync-Jobs (`SyncJob` mit `include`/`exclude`
  Ordnerfiltern).
- Atomares Schreiben (Temp-Datei + Rename), Validierung (Pfade, IDs,
  Duplikate, Konto-Referenzen), Schema-Migration über das `version`-Feld.
- Unit-/Integrationstests: Roundtrips, ungültige Pfade, Version-Migration.
