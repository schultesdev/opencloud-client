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

### Added (Phase 2 — Authentifizierung, `oc-auth`)

- OIDC Discovery über `<server>/.well-known/openid-configuration` mit den
  OpenCloud-Standardendpunkten (`/signin/v1/identifier/_/authorize`,
  `/konnect/v1/token`) als Fallback.
- Authorization Code Flow + PKCE (S256): Loopback-HTTP-Listener auf
  `127.0.0.1:<freier Port>`, `state`-Validierung, Systembrowser via
  `webbrowser`, Code-Austausch gegen den Token-Endpunkt
  (Desktop-`client_id=OpenCloudDesktop`, Scope `openid profile email
  offline_access`).
- Access-Token nur im Speicher (Expiry-Tracking mit 30 s Vorlauf);
  Refresh-Token im OS-Keyring (`keyring`: Windows Credential Manager,
  macOS Keychain, Linux Secret Service), nie in JSON.
- Single-Flight-Refresh: parallele Aufrufe teilen sich eine
  Refresh-Anfrage; `force_refresh()` für den 401-Pfad;
  Refresh-Token-Rotation wird im Store nachgezogen; `invalid_grant`
  räumt den Login ab.
- Logout: Revocation (RFC 7009, best effort, nur wenn Discovery einen
  Endpunkt liefert) + Keyring leeren + Speicherzustand verwerfen.
- Tests (33 neue): PKCE-Testvektor (RFC 7636), Callback-Listener (Code,
  State, Favicon-Störungen, Timeout), Discovery inkl. Fallback, kompletter
  Login-Flow gegen Mock-Server, Refresh/Expiry/Single-Flight,
  `invalid_grant`, Logout — jeweils mit `wiremock`.
