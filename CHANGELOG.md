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

### Added

- **Schalter „Dateien im Hauptordner“**: Sync-Jobs mit Ordnerauswahl
  (Include-Liste) können jetzt getrennt steuern, ob Dateien direkt im
  Hauptordner des Spaces synchronisiert werden (`sync_root_files` in
  `jobs.json`, Standard: an). Vorher fielen Hauptordner-Dateien bei
  gesetzter Ordnerauswahl aus dem Sync heraus — und wurden durch das neue
  Aufräumverhalten lokal gelöscht, ohne sie je wieder einsyncen zu
  können. Der Schalter steht im Job-Wizard (Schritt „Ordnerauswahl“,
  erscheint sobald Ordner ausgewählt sind, auch beim Bearbeiten) und
  wird in der Job-Liste angezeigt. Ist der Schalter aus, werden
  synchronisierte Hauptordner-Dateien beim nächsten Sync lokal gelöscht —
  in der Cloud bleiben sie erhalten.

- **Ausschlüsse nach Mustern, pro Job und global**: Sync-Jobs können
  Dateien und Ordner per Glob-Muster (`*`, `?`) vom Sync ausschließen —
  Muster ohne `/` treffen jeden Ordner- oder Dateinamen (`*.tmp`, `.git`),
  Muster mit `/` ganze Pfade (`build/out`). Neu ist ein Editor dafür im
  Job-Wizard (Schritt „Ausschlüsse“, auch beim Bearbeiten bestehender
  Jobs) und — neu `settings.global_exclude` — eine globale Liste in den
  Einstellungen, die für alle Jobs zusätzlich zu den Job-Mustern gilt.
  Die Engine mischt die globalen Muster beim Start in jeden Job-Filter;
  GUI und Headless/CLI werten sie gleichermaßen aus.

### Changed

- **Abgewählte Ordner lokal löschen, Cloud behält sie**: Wird ein zuvor
  synchronisierter Ordner abgewählt (Include-Liste oder Ausschlussmuster),
  entfernt die Engine beim nächsten Sync die lokalen Kopien — auf dem
  Server bleibt alles erhalten. Gelöscht wird nur, was nachweislich
  synchronisiert war (lokale Datei stimmt noch mit der letzten gesicherten
  Baseline überein); neue oder lokal geänderte Dateien im abgewählten
  Ordner bleiben liegen, ebenso Ordner, die danach nicht leer sind
  (gleiche Sicherheitsregel wie bei Lösch-Propagation). Der Assistent
  „Ordner & Ausschlüsse bearbeiten“ weist auf das Verhalten hin.

### Fixed

- **GTK-Warnung beim Tray-Start unter Linux unterdrückt**: Beim Start
  schrieb das Systempaket libayatana-appindicator (die Basis des
  Tray-Icons unter Linux) jedes Mal die Meldung
  `Gtk-CRITICAL **: gtk_widget_get_scale_factor: assertion 'GTK_IS_WIDGET
  (widget)' failed` auf stderr — der Bibliotheks-Fallback auf ein
  GtkStatusIcon-Widget ist auf modernem GTK immer leer. Die Meldung ist
  harmlos (das Tray-Icon erscheint, sobald die Statusleiste den Dienst
  anmeldet); sie wird jetzt per Gtk-Log-Handler gezielt gefiltert und nur
  noch mit `RUST_LOG=debug` ins normale Log geschrieben. Alle anderen
  GTK-Meldungen bleiben unverändert sichtbar.

- **Konflikt-Kaskade bei gedrosseltem Server (HTTP 425)**: Antwortet
  OpenCloud auf die Sync-Flut mit `425 Too Early` (z. B. wenn versehentlich
  ein Build-Verzeichnis synchronisiert wird), wurde jeder einzelne
  Request als dauerhafter Fehler verbucht — im Extremfall
  sechsstellige Fehlerzahlen pro Lauf. Jetzt gilt 425 wie 429 als
  vorübergehend (HTTP-Retry mit Backoff, Wiederholung bedingter
  Schreiboperationen), und der erste Drossel-Fehler bricht den Lauf
  ab: Der Job geht in den Offline-Backoff mit exponentieller
  Verzögerung, statt im 30-Sekunden-Takt weiterzuhämmern. Halbfertige
  Läufe können dadurch keine veralteten Baselines mehr hinterlassen,
  aus denen bei späteren Uploads Schein-Konflikte (412) entstehen.

- **Keine Konflikte mehr mit erfundenem Remote-Zustand**: Schlug nach
  einem abgewiesenen Upload (`412`) das nachgeschaltete `PROPFIND` fehl
  (Drosselung, Netzwerk), wurde das als „remote gelöscht“ in den
  Konflikt geschrieben — es entstanden unauflösbare Konflikte ohne
  lokale und ohne Remote-Metadaten, deren Auflösung mit „Server
  übernehmen“ die lokale Datei gelöscht hätte. Jetzt ist ein
  unbestimmbarer Remote-Zustand ein Fehler (der Pfad wird im nächsten
  Lauf neu geplant), ein auf beiden Seiten verschwundener Pfad wird
  gar nicht mehr als Konflikt vermerkt, und die Konflikt-Auflösung
  bewahrt den aufgezeichneten Remote-Zustand, statt ihn bei
  fehlgeschlagener Nachfrage zu löschen. „Server übernehmen“
  verifiziert zudem per `PROPFIND`, bevor es aufgrund fehlender
  Remote-Metadaten lokale Daten als „remote gelöscht“ entfernt.

- **GUI — Absturz beim Hit-Test (NaN-Widget-Rects)**: egui 0.32.3 stürzte
  mit `called Option::unwrap() on a None value` (`hit_test.rs:365`) ab,
  wenn ein Widget-Rect eine NaN-Koordinate enthielt — z. B. während das
  Fenster in den Tray versteckt war und die GTK-Oberfläche bereits
  abgebaut war. Behoben durch einen lokalen Backport des Upstream-Fixes
  ([emilk/egui#8479](https://github.com/emilk/egui/pull/8479), upstream
  ab 0.36.2): egui wird per `[patch.crates-io]` aus `vendor/egui`
  eingebunden; der Hit-Test verwirft Widgets mit NaN-Rects und vergleicht
  Treffer nach Id statt nach Wert.

- **GUI — Ordnerauswahl im Job-Wizard**: Jede Zeile des Ordnerbaums nahm
  die gesamte verbleibende Höhe ein und streckte das Fenster „Ordner &
  Ausschlüsse bearbeiten“; die Zeilen sind jetzt einzeilig, große Bäume
  scrollen innerhalb der Karte (max. 320 px). Beim Bearbeiten sind die
  Schritte als 1/2 statt 4/5 nummeriert.

- **Serverfehler beim Auflisten eines Ordners friert nur diesen Ordner ein**:
  Scheitert das `PROPFIND Depth: 1` eines Unterordners mit einem 5xx-Fehler
  (z. B. `ResourceExhausted`, wenn ein Verzeichnis mehr Einträge hat als das
  gRPC-Nachrichtenlimit des Servers erlaubt), bricht der Sync-Lauf nicht mehr
  komplett ab. Der betroffene Ordner wird als Fehler gemeldet und zusammen mit
  seinem Unterbaum für den Lauf eingefroren; der Rest des Spaces wird normal
  synchronisiert und beim nächsten Lauf erneut versucht. Netzwerk-, Auth- und
  Protokollfehler brechen den Lauf weiterhin ab.

### Changed

- **GitHub-Projekt hinterlegt**: `repository` ist nun im Manifest gesetzt
  (`https://github.com/schultesdev/opencloud-client`). Der Info-Tab verlinkt
  Quellcode und Issue-Tracker statt der Platzhalter, und die Update-Prüfung
  nutzt den Releases-Feed des Repositories.

### Added (Phase 9 — Packaging & Distribution)

- **Release-Builds über CI** (`.github/workflows/release.yml`, getriggert
  an Tags `vX.Y.Z`): der Release-Job stellt alle Pakete als Draft-Release
  bereit —
  - Windows: NSIS-Installer (Startmenü/Desktop-Verknüpfung, Deinstallation
    mit Eintrag in „Apps & Features“) und portables ZIP; das
    Anwendungssymbol ist per `winresource` in der EXE eingebettet
    (`build.rs`).
  - macOS: Universal-Binary (aarch64+x86_64) als `OpenCloud Sync.app`
    mit ICNS, verpackt als `.dmg`; Codesigning und Notarisierung laufen
    automatisch, sobald die Secrets konfiguriert sind
    (`MACOS_SIGNING_IDENTITY`, Zertifikat, notarytool-Zugang), sonst
    bleibt das Bundle ad-hoc signiert.
  - Linux: `.deb` und `.rpm` via nfpm (`packaging/nfpm.yaml`, Binary,
    Desktop-Datei, Hicolor-Icons; Abhängigkeiten `gtk3`/`libsecret`)
    und AppImage via linuxdeploy mit GTK-Plugin — der AppImage-Schritt
    darf ausfallen, ohne die anderen Artefakte aufzuhalten.
  - Alle Paket-Icons werden aus dem eingebetteten Anwendungsicon
    erzeugt: `packaging/icons/generate.py` liest
    `oc-app/assets/icon.rs` und schreibt PNG (64/128/256), ICO
    (16–256) und ICNS (32–512) — eine Quelle, kein Auseinanderdriften.
- **Update-Benachrichtigung** (`oc-app/src/updates.rs`): beim Start und
  per „Jetzt prüfen“ im Info-Tab wird die neueste Release-Version gegen
  die laufende Version geprüft (SemVer, auch mit `v`-Präfix und
  Vorabversionen); bei einer neueren Version zeigt der Info-Tab Badge,
  Link zur Release-Seite und einen Hinweis in der Statuszeile —
  **kein Auto-Update**. Der Feed ist die GitHub-Releases-API des in
  `[workspace.package] repository` eingetragenen Projekts; solange
  `repository` (noch) nicht gesetzt ist, meldet die Prüfung „folgt mit
  der ersten Veröffentlichung“, ohne das Netz zu bemühen — dieselbe
  Platzhalter-Logik wie die Projekt-Links im Info-Tab. Neue Einstellung
  `settings.check_updates` (Standard: an) schaltet die Prüfung beim
  Start ab. Prüfung läuft als eigener Task im Backend
  (`UiCommand::CheckUpdates`/`BackendEvent::UpdateChecked`), Tests mit
  Mock-Feed decken neueren/aktuellen/Fehler-Fall ab.
- **README.md**: Handbuch mit Installation (alle Pakete, Bauen aus dem
  Source), ersten Schritten, Konfliktlösung (GUI, CLI, Policies),
  CLI-Referenz und vollständiger Konfigurationsreferenz zu
  `config.json`/`jobs.json` inklusive Feldgrenzen, Ordnerorten je OS
  und Protokolldateien.

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

### Added (Phase 6 — Konflikthandhabung, `oc-sync`/`oc-db`/`oc-app`)

- **Konflikterkennung vervollständigt**: beidseitig geänderte Dateien
  (auch Löschung-vs.-Änderung und 412 beim Upload) landen in
  `conflicts` (Status `pending`) und `entries.state = conflict`; die
  betroffene Datei wird nicht mehr angetastet, bis der Konflikt
  gelöst ist (eingefrorene Pfade, inkl. schützender Ordner).
- **`conflict_policy`** (`config.json`, `settings`): `ask` (Standard)
  hält Konflikte pending für GUI/CLI; `keep_server`, `keep_local` und
  `conflict_copy` lösen Konflikte sofort im Sync-Lauf auf.
  `conflict_copy` legt die lokale Version als
  `name (conflicted copy YYYY-MM-DD HH-MM-SS).ext` ab (Kollisionen
  bekommen einen Zähler) und übernimmt dann die Serverversion; die
  Kopie synchronisiert sich beim nächsten Lauf als neue Datei.
- **Lösungsaktionen** (`oc-sync::conflict::resolve`): `resolve_server`
  lädt die Serverversion herunter und überschreibt lokal (bei
  serverseitiger Löschung wird die lokale Datei entfernt);
  `resolve_local` lädt mit `If-Match=<Konflikt-ETag>` hoch — ändert
  sich der Server in der Zwischenzeit (412), bleibt der Konflikt
  pending und der aktuelle Remote-Zustand wird nachgetragen
  (`oc-db`: `update_conflict_remote`, `get_conflict`); lokal
  gelöschte Dateien propagieren die Löschung zum Server.
  Ordner-/Datei-Mismatches werden abgelehnt
  (`SyncError::UnresolvableConflict`) und müssen manuell bereinigt
  werden.
- **CLI (GUI-freier Pfad, `oc-app`)**: `--conflicts` listet offene
  Konflikte aller Jobs (Id, Pfad, lokale/remote-Größe und ETag);
  `--resolve <id> server|local` löst einen Konflikt (bei mehrdeutigen
  Ids über mehrere Jobs hinweg: `--resolve <job-id>:<id> ...`).
  Der Headless-Modus wendet die konfigurierte Policy an.
- **Events/Summary**: `EngineEvent::ConflictResolved` (Policy wie auch
  CLI), `RunSummary.auto_resolved` (Anzeige „N auto-resolved“).
- Tests (12 neue): Resolve-Aktionen gegen Mock-WebDAV — Server gewinnt
  (Download + Remote-Löschung), Lokal gewinnt (`If-Match`,
  `If-None-Match`-Neuanlage, lokale Löschung → DELETE), 412 mit
  nachgetragenem Remote-Zustand, unbekannte/Ordner-Konflikte,
  Policies `keep_server`/`keep_local`/`conflict_copy` im Engine-Lauf;
  `oc-db`-DAO-Tests für `get_conflict`/`update_conflict_remote`.

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

### Changed (GUI-Überarbeitung, `oc-app`)

- Inhalte aller Tabs und des Wizards laufen in einer zentrierten Spalte
  (max. 960 px); Karten wirken auf breiten Fenstern nicht mehr leer.
- Konten-Tab: kompakte Kontokarten (Avatar, Server, Status-Pille,
  An-/Abmelden, Spaces aktualisieren); Spaces mit Typ-Chip und
  Quota-Balken. „Konto hinzufügen“ als aufklappbares Formular.
- Neuer Tab „Einstellungen“ (Synchronisation / Programm) mit Hinweis
  auf ungespeicherte Änderungen und „Verwerfen“.
- Job-Karten: Status-Pille, „Jetzt synchronisieren“, Pause-Icon und
  ein Menü für Öffnen, Ordnerauswahl und Entfernen statt fünf
  gleichrangiger Buttons; Details und Fortschritt darunter.
- Konflikte gruppiert nach Space-Name mit Anzahl, Lokal/Server als
  Tabelle.
- Wizard lädt die Spaces automatisch (bzw. übernimmt bereits geladene)
  und warnt bei nicht angemeldetem Konto.
- Statuszeile meldet „Server nicht erreichbar“, wenn ein Job offline ist.
- Minimale Fenstergröße auf 760×520 gesenkt; der Titel im Kopf entfällt
  bei schmalen Fenstern.

### Fixed (GUI)

- Quota ohne bekanntes Limit zeigt „8.2 GB belegt“ statt
  „8.2 GB von 0 B belegt“.
- Status-Punkte werden gezeichnet statt als Glyphe gesetzt (die
  eingebauten Schriften enthalten kein „●“, es erschien ein leeres
  Kästchen).

### Fixed (Review, Block 1 — Datenverlust-Risiken)

- `oc-sync`: Eine Ordner-Löschung auf einer Seite löscht keine neuen oder
  geänderten Dateien mehr, die auf der anderen Seite in diesem Ordner
  liegen. Der Ordner wird stattdessen dort neu angelegt; unveränderte
  Kinder folgen weiterhin der Löschung.
- `oc-sync`: Offene Konflikte frieren den ganzen Teilbaum ein, nicht nur
  den exakten Pfad.
- `oc-sync`: Ein Download überschreibt keine lokale Datei mehr, die seit
  dem Scan bearbeitet wurde (`LocalGuard`); ein Upload erkennt Dateien,
  die während des Lesens geschrieben wurden.
- `oc-sync`: Der Root-ETag wird nur nach vollständig fehlerfreien Läufen
  gespeichert, sonst würde der Shortcut fehlgeschlagene Pfade bis zur
  nächsten Server-Änderung verdecken.
- `oc-db`: `delete_subtree` nutzt eine exakte Bereichsabfrage statt
  `LIKE`; `_`, `%` und Groß-/Kleinschreibung trafen bisher fremde Pfade.
- `oc-db`/`oc-app`: Die Status-DB ist an Konto, Space und lokalen Ordner
  ihres Jobs gebunden (`ensure_identity`) und wird bei Abweichung
  zurückgesetzt. Verwaiste DBs gelöschter Jobs werden entfernt, ihre IDs
  nicht wieder vergeben.
- `oc-config`: Atomares Schreiben ohne Lösch-Fallback, mit `fsync` und
  eindeutigem Temp-Namen; eine unlesbare Datei liefert einen Fehler statt
  stiller Standardwerte.
- `oc-config`/`oc-app`: Überlappende lokale Ordner zweier Jobs werden beim
  Speichern abgelehnt und von der Engine übersprungen; Job-IDs müssen
  gültige Dateinamen sein.
- `oc-auth`/`oc-app`: Ein Token-Manager pro Konto statt pro Job; ein
  wegen gleichzeitiger Rotation abgelehnter Refresh-Token meldet das
  Konto nicht mehr ab.
- `oc-app`: `--login` mit `--headless` startet nach dem Login die Engine.

### Fixed (Review, Block 2 — Netzwerk- und Engine-Robustheit)

- `oc-api`: Netzwerkfehler werden jetzt wie dokumentiert wiederholt.
  Bedingte PUTs und MOVE/COPY werden nur wiederholt, wenn der Server den
  ersten Versuch sicher nicht verarbeitet hat (keine Verbindung, 429,
  503); sonst entstünden falsche Konflikte. `Retry-After` gilt auch bei
  503 und wird auf 60 s gedeckelt.
- `oc-api`/`oc-sync`: Offline schlägt oft zuerst der Token-Refresh fehl;
  das zählt jetzt als Verbindungsfehler und löst den Offline-Backoff aus.
- `oc-api`/`oc-sync`: Server-Namen in anderer Unicode-Form (NFD, etwa von
  macOS hochgeladen) bleiben adressierbar. `DavResource::server_path`
  trägt die Server-Schreibweise, die Engine merkt sie sich in der
  Status-DB (`server_names`) und nutzt sie für alle Requests.
- `oc-api`: hrefs müssen auf einer Segmentgrenze unter dem Space liegen;
  `<response>`-Einträge mit eigenem Fehlerstatus werden verworfen,
  Statuszeilen werden geparst.
- `oc-sync`: Range-Downloads senden `If-Match` und prüfen das ETag jedes
  Stücks, damit keine Datei aus zwei Versionen entsteht.
- `oc-sync`: Ein unlesbarer lokaler Eintrag bricht den Scan nicht mehr ab.
  Er wird gemeldet und für den Lauf eingefroren, damit ein unlesbarer
  Ordner nie als gelöscht gilt.
- `oc-db`: Schema v2 mit passenden Indizes, höchstens einem offenen
  Konflikt pro Pfad und der Tabelle `server_names`. Migrationen laufen in
  einer `IMMEDIATE`-Transaktion nach dem Umschalten auf WAL, so dass
  parallele Öffnungen nicht doppelt migrieren; kaputte Versionsangaben
  sind ein Fehler statt einer erneuten Migration.
- `oc-auth`: Refresh und Widerruf nutzen die per Discovery gefundenen
  Endpunkte; Server-URLs mit Unterpfad funktionieren; mehrere gleichzeitige
  401 lösen nur einen Refresh aus; Token-Antworten ohne `expires_in`
  werden akzeptiert.

### Fixed (Review, Block 3 — GUI-Korrektheit)

- `oc-app`: „Beenden“ im Tray beendet die App; ohne Tray schließt das
  Fensterkreuz die App, statt das Fenster unerreichbar zu verstecken.
- `oc-app`: Das Tray-Icon entsteht erst bei laufender Event-Loop (macOS)
  und läuft unter Linux auf einem eigenen gtk-Thread, wie `tray-icon`
  es verlangt.
- `oc-app`: Der Wizard sendet fehlgeschlagene Ordner- und
  Space-Anfragen nicht mehr in jedem Frame neu; stattdessen gibt es
  „Erneut versuchen“.
- `oc-app`: Konto-, Job- und Einstellungsänderungen werden erst nach
  erfolgreichem Speichern übernommen; ein Speicherfehler hinterlässt
  keine Phantom-Jobs mehr.
- `oc-app`: Der Engine-Neustart prüft die Konfiguration, bevor die
  laufende Engine gestoppt wird.
- `oc-app`: Login, Spaces, Ordnerbaum und Konfliktlösung laufen als
  eigene Tasks; ein offener Browser-Login blockiert nichts mehr.
  „Alle: …“ löst die Konflikte eines Jobs mit einer Verbindung; laufende
  Lösungen sperren ihre Buttons.
- `oc-app`: Autostart-Einträge maskieren den Programmpfad je Plattform,
  „deaktivieren“ meldet echte Fehler, „aktiv“ heißt „zeigt auf dieses
  Programm“; die Einstellung wird beim Umschalten sofort gespeichert.
- `oc-sync`/`oc-app`: „Jetzt synchronisieren“ läuft auch bei pausiertem
  Job einmal und hebt die Pause nicht mehr auf.
- `oc-sync`: Ein nicht installierbarer Dateiwächter verhindert den Start
  der Engine nicht mehr; der Job nutzt dann den periodischen Scan.

### Changed (Review, Block 4 — Performance)

- `oc-db`/`oc-sync`: `StatusDb::transaction`; reine DB-Schritte eines
  Laufs (`RecordSynced`, `DropEntry`) und die Filter-Bereinigung laufen in
  einer Transaktion statt einem Commit pro Pfad; Recovery
  (`reset_transfer_states`) und Statuswechsel (`set_state`) sind je ein
  Statement; die Einträge werden einmal pro Lauf gelesen. Konflikt und
  eingefrorener Eintrag werden atomar geschrieben.
- `oc-db`/`oc-sync`: Schema v3 speichert lokale Änderungszeiten in
  Millisekunden; zwei gleich große Änderungen in derselben Sekunde gehen
  nicht mehr verloren. Alte Sekundenwerte werden migriert und tolerant
  verglichen.
- `oc-api`/`oc-sync`: Uploads streamen aus der Datei
  (`WebDavClient::put_file`), Downloads in die `.part`-Datei; keine Datei
  liegt mehr komplett im Speicher. Transfers haben einen Leerlauf-Timeout
  (120 s ohne Daten) statt 10 Minuten Gesamtzeit. Alle API-Clients teilen
  einen Connection-Pool.
- `oc-app`: Die GUI liest die Status-DBs nicht mehr im UI-Thread; das
  Backend liefert Snapshots aus einem Blocking-Thread. Backend-Ereignisse
  wecken die GUI sofort; im Leerlauf zeichnet sie einmal pro Sekunde.
- `oc-db`: `stats()` ist eine einzige, konsistente Abfrage.
- `oc-auth`: Schlüsselbund-Zugriffe beim Refresh laufen in einem
  Blocking-Thread.
- `oc-app`: Der Wizard-Ordnerbaum kopiert seine Einträge nicht mehr pro
  Frame; `derive_include` ist linear in der Tiefe.

### Fixed (Review, Block 5 — Aufräumen und Härtung)

- `oc-sync`: Eine Server-Datei ohne ETag gilt bei der Konfliktlösung
  nicht mehr als gelöscht; „Server übernehmen“ lädt sie herunter, statt
  die lokale Datei zu löschen.
- `oc-auth`: Der Login-Listener bedient Verbindungen parallel (eine
  stumme Vorab-Verbindung des Browsers bremst nichts mehr), akzeptiert
  nur den Pfad `/` und den eigenen `state` und zeigt bei Ablehnung eine
  passende Seite.
- `oc-config`: Obergrenzen für Intervall, parallele Transfers und
  PROPFIND-Pause, gleich denen der GUI.
- `oc-app`: Neue CLI-Modi `--logout`, `--status` und `--sync-once`;
  Modus-Flags schließen sich gegenseitig aus. `--resolve` mit bloßer ID
  meldet DB-Fehler statt sie zu verschlucken.
- `oc-app`: Ein geänderter Log-Level gilt sofort, ohne Neustart.
- `oc-api`: Die ungenutzte Hilfsfunktion `with_retry` entfällt; die
  Retry-Logik liegt allein im HTTP-Kern.
- `oc-api`/`oc-sync`: Der PROPFIND fragt `oc:checksums` ab. Gleich große
  Dateien ohne bisherige Basis gelten beim Initial-Sync nur noch als
  identisch, wenn die SHA1 übereinstimmt; sonst entsteht ein Konflikt.
- `oc-sync`: Auf Dateisystemen ohne Unterscheidung der Groß-/
  Kleinschreibung werden Server-Namen, die sich nur darin unterscheiden,
  erkannt, gemeldet und eingefroren, statt sich lokal zu überschreiben
  und danach auf dem Server gelöscht zu werden.
- `oc-app`: „Alle pausieren“ markiert auch die einzelnen Job-Karten als
  pausiert, egal ob über die Kopfleiste oder den Tray ausgelöst.

### Changed (Review, Block 5)

- `oc-app`: `gui/mod.rs` ist aufgeteilt in `accounts.rs`, `jobs.rs`,
  `conflicts.rs`, `settings.rs`, `wizard_view.rs` und `format.rs`; der
  Leerzustand der Tabs ist ein gemeinsamer Baustein (`theme::empty_state`).

### Added (GUI — Transferdetails)

- Jobkarte während eines Laufs: unter dem Fortschrittsbalken eine Zeile je
  laufendem Transfer mit Richtung (Pfeil hoch/runter), Pfad, Mini-Balken,
  „übertragen / Größe“ und der Geschwindigkeit dieser Datei; darunter
  „noch N wartend“. Die Zeilen zeigen nur wirklich laufende Transfers
  (höchstens `max_parallel_transfers`), nicht mehr die eingereihten.
- `RunProgress` führt die laufenden Transfers (`active`: Pfad, Richtung,
  Bytes, Größe) und die abgebrochenen (`transfers_failed`) mit; neue Typen
  `TransferProgress`/`TransferDirection` in `oc-sync`.
- Tray-Tooltip nennt bei laufender Synchronisation Dateizähler und
  Restzeit („12/40 Dateien · noch 2 min“, auf Minuten gerundet).
- `EngineOptions::progress_interval` (Standard 200 ms) steuert die
  Drosselung der Fortschritts-Events; Tests setzen 0 und prüfen Snapshots
  laufender Transfers deterministisch.

### Fixed (Transferfortschritt)

- „X von Y Dateien“ zählt einen Transfer, sobald er fertig ist — nicht erst,
  wenn die Engine in Planreihenfolge bei ihm ankommt.
- Die Byte-Anzeige kann beim Abschluss paralleler Transfers nicht mehr kurz
  zurückspringen (Abschluss und Zählung unter demselben Lock).
- Chunked Downloads melden den Fortschritt je Netzwerk-Paket statt einmal
  pro 8-MiB-Range; die Ranges werden direkt in die `.part`-Datei gestreamt
  statt im Speicher gepuffert.
- Nach einem fehlgeschlagenen Transfer geht „noch N wartend“ auf 0.
- Fortschritts-Events werden nur noch auf `debug` geloggt; die
  einmal pro Sekunde aus der Datenbank gelesene Liste „Aktuell: …“ entfällt.

### Added (GUI — Info-Tab)

- Neuer Tab „Info“: Version, Autor, Website (www.schultes.dev), Lizenz
  (Apache-2.0) und Link zu OpenCloud; dazu Plattform/Build sowie
  Konfigurations- und Protokollordner mit „Öffnen“-Button.
- Die Angaben stammen aus `[workspace.package]` (`authors`, `homepage`
  neu); für das GitHub-Projekt und den Issue-Tracker steht ein Platzhalter
  („Link folgt“), der automatisch zu Links wird, sobald `repository`
  gesetzt ist.
- `LICENSE` im Repository-Wurzelverzeichnis: unveränderter Text der
  Apache License 2.0.
