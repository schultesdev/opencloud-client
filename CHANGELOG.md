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

### Added (Phase 4 — SQLite-Statusdatenbank, `oc-db`)

- Eine Statusdatenbank pro Sync-Job unter `<config-dir>/db/<job-id>.sqlite`
  (`db_path()` mit Validierung der Job-ID gegen Pfad-Separatoren/`..`).
- Schema v1: `meta` (schema_version, space_etag, last_sync), `entries`
  (Pfad, is_dir, lokale mtime/size, Remote-ETag/size, Checksumme,
  State `synced|uploading|downloading|conflict|error` mit CHECK-Constraints),
  `conflicts` (lokale/Remote-Metadaten, Status
  `pending|resolved_server|resolved_local`, resolved_at).
- Migrationsrunner: Version in `meta`, schrittweise Migration in
  Transaktionen, Neuöffnen idempotent, neuere Schema-Versionen werden
  abgelehnt; WAL-Modus + `synchronous=NORMAL` für Datei-Datenbanken.
- Dao-Layer auf `StatusDb`: `upsert_entry`, `get_entry`, `delete_entry`,
  `delete_subtree` (Präfix-Baum, berührt keine gleich benannten Geschwister),
  `entries_with_state`, `record_conflict`, `resolve_conflict` (nur `pending`,
  Server/lokal), `pending_conflicts`, `conflicts_with_status`, `stats()`
  (Datei-/Ordnerzahl, aktive Transfers, Konflikte, Fehler, letzter Sync),
  `meta`/`set_meta`/`set_last_sync`.
- Tests: In-Memory- und tempfile-Datenbanken, Migration v0 → v1 und
  Idempotenz, Roundtrips, Subtree-Löschung, Konflikt-Lebenszyklus,
  Statistiken, WAL-Modus.

### Added (Phase 5 — Sync-Engine, `oc-sync`)

- **Drei-Stufen-Vergleich** (`diff`): lokaler Scan vs. Remote-Scan vs.
  letzter synchronisierter Zustand aus der Statusdatenbank → Aktionsplan
  (`SyncAction`): Upload mit `If-Match`, Download, Löschung auf die
  jeweils andere Seite, Löschung-vs.-Änderung als Konflikt, beidseitig
  neu mit gleicher Größe als `RecordSynced` (Initial-Sync-Heuristik);
  Ordner werden nur auf Existenz verglichen; Ordner mit konfliktbehafteten
  Inhalten werden nicht gelöscht.
- **Initialer Vollsync**: gestaffeltes Remote-Traversal per PROPFIND
  `Depth: 1` je Ebene (ressourcenschonender als `Depth: infinity`),
  lokaler Scanner (überspringt Symlinks und `.part`-Tempdateien,
  legt fehlende Wurzeln an), Aufbau der Datenbank.
- **Selective Sync** (`Filter`): `include`-Whitelist (Traversal bricht
  unterhalb nicht enthaltener Ordner ab), `exclude`-Globs je Segment oder
  Pfad; Filterwechsel entfernt nicht mehr abgedeckte DB-Einträge
  (lokale Dateien bleiben, GUI-Nachfrage folgt in Phase 7).
- **Lokale Änderungserkennung** (`watch`): `notify`-Watcher pro Job mit
  Debouncing (Standard 800 ms), `.part`-Events werden ignoriert;
  Periodik-Scan (`sync_interval_secs`, Standard 30 s) als Fallback und
  für Remote-Änderungen.
- **Remote-Änderungserkennung**: Root-ETag als Shortcut — unveränderter
  Space-ETag überspringt das Traversal, der Datenbankstand dient als
  Remote-Snapshot; 404 auf Ordner-PROPFINDs während des Traversals wird
  toleriert (zwischenzeitlich gelöschte Ordner).
- **Transfers** (`transfer`): Queue mit `max_parallel_transfers`
  (Tokio-Semaphore), partielle Downloads per `Range` in Chunks
  (Standard 8 MiB) mit `.part`-Tempdatei und atomarem Rename;
  Read-only-Attribute werden beim Überschreiben pro OS behandelt
  (Unix: nur Owner-Write-Bit, Windows: Read-only-Attribut); PUT ohne
  ETag in der Antwort löst einen PROPFIND zum Nachlesen aus.
- **Konflikterkennung** (Auflösung folgt in Phase 6): beidseitig
  geändert oder Upload mit `If-Match` auf 412 gelaufen → Eintrag in
  `conflicts` (Status `pending`) + `entries.state = conflict`,
  betroffene Pfade werden eingefroren und nicht mehr angetastet.
- **Idempotenz/Absturzsicherheit**: `.part`-Dateien werden zu Beginn
  jedes Laufs aufgeräumt; DB-Einträge in `uploading`/`downloading`
  werden beim Start zurückgesetzt und vom Diff neu geplant
  (Basiswerte bleiben intakt); 404/NotFound bei Löschungen wird als
  Erfolg gewertet (Elterner bereits gelöscht).
- **Engine** (`engine`): ein Tokio-Task pro Job, Läufe werden
  serialisiert; Befehle `Pause`/`Resume` (mit Catch-up-Lauf)/`SyncNow`/
  `Stop` pro Job, globales `pause_all`/`resume_all`/`stop`; Events
  (`RunStarted`, `RunFinished` mit Zusammenfassung, `Failed`,
  `Conflict`, `Stopped`) für GUI und Headless-Modus; StatusDb pro Job
  hinter `std::sync::Mutex` (Guard nie über `await` gehalten).
- **Headless-Modus in `oc-app`**: `--headless` lädt `config.json`/
  `jobs.json`, startet pro aktivem Job die Engine (Token aus Phase 2
  via Keyring-Refresh), gibt Events auf stdout aus, beendet mit Ctrl-C.
- `oc-api`: `get_range()` für `Range`-Downloads (inkl. Toleranz für
  Server, die den Header ignorieren und 200 liefern).
- Tests (43 neue in `oc-sync`): Engine-Szenarien gegen Mock-WebDAV
  (`wiremock`) — Initialsync, neue/geänderte/gelöschte Dateien auf
  beiden Seiten, Ordnerumbenennung, Include-/Exclude-Filter,
  Filterwechsel, Range-Chunk-Downloads, parallele Transfers,
  Konflikte (beidseitig + 412), Root-ETag-Shortcut, Absturz-Recovery,
  Pause/Resume/Stop, Watcher; dazu Unit-Tests für Diff, Scan, Filter,
  Transfers und zwei `oc-api`-Tests für `get_range`.

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

### Added (Phase 3 — API-Client, `oc-api`)

- `GraphClient`: `list_spaces()` (`GET /graph/v1.0/me/drives`) mit
  `Space` (`id`, `name`, `driveType`, `driveAlias`, `root.webDavUrl`,
  `quota`) — die Basis für die Space-Auswahl in Sync-Jobs.
- `WebDavClient` pro Space (`https://<server>/dav/spaces/{id}/` oder aus
  `root.webDavUrl`): `propfind(path, depth)` mit eigenem
  Multistatus-Parser (`quick-xml`, namespace-agnostisch, nur
  `propstat`-Status 200), `get()`, `put()`/`put_with()` mit
  `If-Match`/`If-None-Match` (`WriteCondition`), `mkcol()`, `delete()`,
  `move()`, `copy()` (Destination-/Overwrite-Header).
- Pfade: Prozent-Encoding je Segment (inkl. `&`, Leerzeichen, Unicode),
  Hrefs werden dekodiert, NFD→NFC-normalisiert und relativ zum Space
  aufgelöst; `.`/`..`-Segmente werden abgelehnt.
- Fehlermodell `ApiError`: Netzwerk, 401, 403, 404, 412 Precondition
  Failed (Konflikt-Grundlage), 423 Locked, 429 mit `Retry-After`, 5xx,
  Protokollfehler — mit `is_retryable()`/`retry_after()`.
- Retries mit Exponential-Backoff und Jitter (`RetryConfig`,
  konfigurierbare Timeouts für Metadaten vs. Transfers); nach 401 wird
  der Token einmal per `TokenProvider::refresh` erzwungen erneuert.
- `TokenProvider`-Trait inkl. `StaticToken` (Tests/CLI) und Implementierung
  für `oc_auth::TokenManager` (Single-Flight-Refresh aus Phase 2).
- Tests (45 neue): PROPFIND-Parser gegen XML-Fixtures (Entities,
  Propstat-404, Namensraumvarianten, malformed), URL-Encoding, NFC,
  Graph-/WebDAV-Operationen, Fehlermapping, Retry-/Retry-After-Logik und
  401-Neuanmeldung — jeweils mit `wiremock`.
