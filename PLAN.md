# OpenCloud Sync Client (Rust) — Umsetzungsplan

Ein plattformübergreifender (Windows / macOS / Linux) Synchronisations-Client für
[OpenCloud](https://opencloud.eu), geschrieben in Rust, mit JSON-Konfiguration,
SQLite-Statusdatenbank pro Verzeichnis und einer GUI für Konfiguration,
Statusanzeige und Konfliktlösung.

---

## 1. Anforderungen (Zusammenfassung)

| Anforderung | Umsetzung im Plan |
| --- | --- |
| Beliebig viele Verzeichnisse mit Spaces synchronisieren | N "Sync-Jobs": Space + lokales Verzeichnis + Ordnerfilter |
| Konfiguration als JSON-Textdateien | `config.json` + `jobs.json`, siehe Phase 1 |
| GUI zur Konfiguration | Phase 7 |
| Sync-Status pro Verzeichnis in SQLite | Eine Status-DB pro Sync-Job, Phase 4 |
| Konflikte in der GUI anzeigen und lösen (Server-Datei übernehmen / Client-Datei behalten) | Konflikt-Queue in DB + GUI, Phasen 5–7 |
| Pro Space wählbar, welche Ordner synchronisiert werden | Ordnerfilter (Selective Sync) pro Job, Phase 5 |
| Alle gängigen Betriebssysteme | Rust + egui + rusqlite (bundled) + tokio, Phase 9 Packaging |

---

## 2. Architektur

### 2.1 Prozessmodell

Ein einzelner Prozess:

```
┌─────────────────────────────────────────────────────┐
│                    Hauptprozess                     │
│                                                     │
│  ┌──────────────┐   mpsc/Event-Bus   ┌───────────┐ │
│  │ Sync-Engine │ ──────────────────> │  GUI      │ │
│  │ (tokio      │ <────────────────── │ (egui)    │ │
│  │  Tasks)     │   Commands (Pause,  │           │ │
│  │             │   Konfliktlösung)   │ Tray-Icon │ │
│  └──────┬───────┘                    └───────────┘ │
│         │                                            │
│  ┌──────┴───────┐  ┌──────────────┐  ┌───────────┐  │
│  │ API-Client   │  │ Status-DBs   │  │ JSON-     │  │
│  │ (OIDC, Graph,│  │ (SQLite pro  │  │ Config    │  │
│  │  WebDAV)     │  │ Sync-Job)    │  │           │  │
│  └──────────────┘  └──────────────┘  └───────────┘  │
└─────────────────────────────────────────────────────┘
```

- Sync-Engine läuft als Hintergrund-Tasks (tokio), auch wenn das Fenster
  geschlossen ist (Tray-Icon; Fenster nur bei Bedarf offen).
- Kommunikation Engine <-> GUI über `tokio::sync::mpsc` (Events nach oben,
  Commands nach unten). Dadurch ist die Engine ohne GUI testbar
  (`--headless` Modus für CLI und Tests).

### 2.2 Cargo-Workspace

```
opencloud-sync/
├── Cargo.toml            # Workspace
├── crates/
│   ├── oc-config/        # JSON-Konfiguration laden/speichern/validieren
│   ├── oc-auth/          # OIDC-Login (Code Flow + PKCE), Token-Refresh, Keyring
│   ├── oc-api/           # HTTP-Client: Graph-API (Spaces) + WebDAV
│   ├── oc-db/            # SQLite-Schema, Migrationen, Statusabfragen
│   ├── oc-sync/          # Sync-Engine: Scannen, Diff, Transfers, Konflikte
│   └── oc-app/           # GUI (egui) + Tray + main()
└── PLAN.md
```

### 2.3 Technologie-Entscheidungen

| Bereich | Wahl | Begründung / Alternativen |
| --- | --- | --- |
| Sprache/Runtime | Rust, tokio | ökosystemtragend, cross-platform |
| HTTP | `reqwest` (rustls) | rustls vermeidet OpenSSL-Abhängigkeiten auf Win/mac |
| WebDAV | eigenständig auf reqwest-Basis + `quick-xml` | Volle Kontrolle über PROPFIND, ETags, Depth-Header; `reqwest_dav` als Referenz prüfen |
| GUI | `egui`/`eframe` | pure Rust, keine Systemabhängigkeiten (im Gegensatz zu Tauri/webkit2gtk auf Linux), sofortiges Rendering, gute Tree/Listen-Widgets. Alternative: `iced` (Elm-Architektur, aufwendiger für komplexen State) |
| Tray | `tray-icon` + `winit` | Win/mac/Linux |
| SQLite | `rusqlite` mit `bundled` Feature | SQLite wird mitkompiliert → identisches Verhalten auf allen OS, keine System-libs nötig |
| Datei-Watching | `notify` | FS-Events auf allen Plattformen; Polling-Fallback für Netzlaufwerke |
| Config | `serde` + `serde_json` | menschenlesbar, kommentierbar über `_comment`-Felder oder separate `config.example.json` |
| Secrets | `keyring` (Win Credential Manager / macOS Keychain / Linux Secret Service) | Refresh-Token gehört nicht in die JSON-Datei |
| CSUM | `xxhash` (schnell) + optionale starke Prüfsumme bei Konfliktverdacht | ETag + Größe + mtime als Primärindikatoren |

### 2.4 API-Grundlagen (aus docs.opencloud.eu)

- **Auth (OIDC)**: Authorization Code Flow mit Redirect auf
  `http://127.0.0.1:<port>` (Desktop `client_id=OpenCloudDesktop`).
  - Authorize: `https://<server>/signin/v1/identifier/_/authorize?client_id=...&scope=openid+profile+email+offline_access&response_type=code&redirect_uri=http://127.0.0.1:...`
  - Token: `POST https://<server>/konnect/v1/token` (`grant_type=authorization_code`, später `refresh_token`)
  - `offline_access`-Scope liefert Refresh-Token.
- **Graph-API (LibreGraph, Subset MS Graph Drives)**:
  - `GET /graph/v1.0/me/drives` — Spaces, in denen der Nutzer Mitglied ist
  - `GET /graph/v1.0/drives` — alle Spaces (nur mit erweiterten Rechten)
  - Relevante Felder pro Space: `id`, `name`, `driveType` (personal/project/…), `driveAlias`, `root.webDavUrl`, `quota`
- **WebDAV**: `https://<server>/dav/spaces/{spaceID}/{path}`
  - `PROPFIND` (Depth 0/1) → Struktur + ETag, Größe, mtime, Typ
  - `GET`/`HEAD` (Download), `PUT` (Upload), `MKCOL` (Ordner anlegen), `MOVE`, `COPY`, `DELETE`
  - ETag-basierte Änderungserkennung; `If-Match` für optimistisches Schreiben
  - Optional später: TUS-Upload (`/tus-upload`) für große Dateien

---

## 3. Phasenplan

Jede Phase endet mit lauffähigem, testbarem Code (Commits pro Phase).

### Phase 0 — Projekt-Setup (ca. 1–2 Tage)

- [ ] Cargo-Workspace mit den sechs Crates anlegen, CI (GitHub Actions) mit
      Build + `cargo test` auf `ubuntu-latest`, `windows-latest`, `macos-latest`.
- [ ] Clippy + rustfmt konfigurieren, `deny(warnings)` in CI.
- [ ] `--headless`-Flag in `oc-app` vorsehen (Engine ohne GUI startbar).
- [ ] Versions-/Releasestrategie festlegen (semverutag, Changelog).

### Phase 1 — Konfiguration (`oc-config`, ca. 2–3 Tage)

- [ ] Config-Verzeichnis pro OS via `dirs`:
      Linux `~/.config/opencloud-sync/`, macOS `~/Library/Application Support/opencloud-sync/`, Windows `%APPDATA%\opencloud-sync\`
- [ ] `config.json` (globale Einstellungen + Konten):

```json
{
  "version": 1,
  "accounts": [
    {
      "id": "konto-1",
      "server_url": "https://cloud.example.com",
      "username": "max@example.com"
    }
  ],
  "settings": {
    "sync_interval_secs": 30,
    "max_parallel_transfers": 4,
    "conflict_policy": "ask",
    "log_level": "info"
  }
}
```

- [ ] `jobs.json` (Sync-Jobs, beliebig viele):

```json
{
  "version": 1,
  "jobs": [
    {
      "id": "job-1",
      "account_id": "konto-1",
      "space_id": "storage-users-1$89ad5ad2-...",
      "local_path": "/home/max/OpenCloud/projekt-mars",
      "enabled": true,
      "include": ["Dokumente", "Bilder/2026"],
      "exclude": ["tmp", "*.tmp", ".git"]
    }
  ]
}
```

  - `include` = Ordner-Whitelist pro Space (Selective Sync, Requirement 6);
    leere Liste = alles synchronisieren. `exclude` = Exclude-Muster.
- [ ] Atomares Schreiben (temp file + rename), Validierung mit`serde`,
      Schema-Migration über `version`-Feld.
- [ ] Unit-Tests: runden laden/speichern, ungültige Pfade, Version-Migration.

### Phase 2 — Authentifizierung (`oc-auth`, ca. 3–4 Tage)

- [ ] OIDC Discovery über `https://<server>/.well-known/openid-configuration`.
- [ ] Authorization Code Flow + PKCE: lokaler Loopback-HTTP-Listener auf
      `127.0.0.1:<freier Port>`, Browser öffnen (`open`/`xdg-open`/`start`), Code
      abfangen, Token holen.
- [ ] Access-Token im Speicher; Refresh-Token im OS-Keyring (`keyring`-Crate)
      speichern, **nicht** in JSON.
- [ ] Automatischer Refresh bei 401, Expiry-Tracking, parallele Requests
      teilen sich einen Refresh (Single-Flight).
- [ ] Logout (Refresh-Token invalidieren, Keyring leeren).
- [ ] Tests: Token-Refresh-Logik mit Mock-Server, PKCE-Helper, Expiry-Handling.
      Hinweis: volles Login nur manuell testbar (Browser nötig).

### Phase 3 — API-Client (`oc-api`, ca. 4–6 Tage)

- [ ] `GraphClient`: `list_spaces()` (`GET /graph/v1.0/me/drives`), Space-Typ
      mit `id`, `name`, `driveType`, `driveAlias`, `webDavUrl`, `quota`.
- [ ] `WebDavClient`:
  - [ ] `propfind(path, depth)` → geparste Ressourcen-Liste (Name, Typ, Größe,
        ETag, Last-Modified, Quota-available für Ordner)
  - [ ] `get()`, `put()` (mit `If-Match`/`If-None-Match`), `mkcol()`, `delete()`,
        `move()`, `copy()`
  - [ ] URL-Encoding für Pfade, Unicode-Normalisierung (NFC) beim Vergleich
        lokaler/remote-Namen
- [ ] Fehlermodell: `ApiError` mit Differenzierung (Netzwerk, 401, 403, 404,
      412 Precondition Failed, 423 Locked, 5xx) → Grundlage für Retry/Conflict.
- [ ] Retries mit Exponential-Backoff (`backoff`-Logik), Timeout-Konfiguration.
- [ ] Tests: PROPFIND-Parser gegen XML-Fixtures, URL-Encoding, Retry-Logik mit
      `wiremock`/`httpmock`.

### Phase 4 — SQLite-Statusdatenbank (`oc-db`, ca. 3–4 Tage)

Eine Datenbankdatei **pro Sync-Job** (Anforderung: Status pro Verzeichnis):
`<config-dir>/db/<job-id>.sqlite`.

- [ ] Schema (v1):

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);       -- schema_version, space_etag, last_sync
CREATE TABLE entries (
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,          -- relativer Pfad im Space
  is_dir INTEGER NOT NULL,
  local_mtime INTEGER,                -- beim letzten Sync gesehene lokale mtime
  local_size INTEGER,
  remote_etag TEXT,                   -- beim letzten Sync gesehener Server-ETag
  remote_size INTEGER,
  checksum TEXT,
  last_synced_at INTEGER,
  state TEXT NOT NULL                 -- synced | uploading | downloading | conflict | error
);
CREATE TABLE conflicts (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL,
  detected_at INTEGER NOT NULL,
  local_mtime INTEGER, local_size INTEGER,
  remote_etag TEXT, remote_size INTEGER, remote_mtime INTEGER,
  status TEXT NOT NULL,               -- pending | resolved_server | resolved_local
  resolved_at INTEGER
);
```

- [ ] Migrationen (`rusqlite`-Migrationsrunner), WAL-Modus.
- [ ] Dao-Layer: `upsert_entry`, `get_entry`, `delete_subtree`, `record_conflict`,
      `resolve_conflict`, Statistik-Abfragen (für UI: Anzahl Dateien,
      Konflikte, Fehler, letzter Sync).
- [ ] Tests: In-Memory-SQLite (tempfile), Migration von v0 → v1.

### Phase 5 — Sync-Engine (`oc-sync`, ca. 1–2 Wochen, Kernstück)

- [ ] **Drei-Stufen-Vergleich** pro Datei (Basis: letzter synchronisierter
      Zustand aus DB):

```
Lokal geändert?  = mtime/size != DB-Werte
Remote geändert? = ETag != DB-Werte
→ beide ruhig: nichts
→ nur lokal: Upload (PUT mit If-Match=alter ETag)
→ nur remote: Download
→ beide geändert: KONFLIKT (Phase 6)
→ lokal gelöscht / remote gelöscht: DELETE/MOVE-Behandlung (Delete-Detection:
   Pfad weg + kein Eintrag in DB-Delete-Log = gelöscht)
```

- [ ] **Initialer Vollsync**: rekursives PROPFIND (Depth:1 je Ebene,
      ressourcenschonender als Depth:infinity), lokaler Scanner, DB aufbauen.
- [ ] **Selective Sync**: Traversal bricht unterhalb nicht-`include`ter Ordner
      ab; `exclude`-Muster beim lokalen Scan und remote-Traversal filtern.
      Wird der Filter geändert → betroffene Pfade aus DB entfernen und lokale
      Dateien optional bereinigen (GUI-Nachfrage).
- [ ] **Lokale Änderungserkennung**: `notify`-Watcher pro Job + Debouncing;
      zusätzlicher Periodik-Scan (`sync_interval_secs`) als Fallback und für
      remote-Änderungen.
- [ ] **Remote-Änderungserkennung**: PROPFIND Depth:1 pro beobachtetem Ordner,
      gestaffelt; Root-ETag als Shortcut. (Später Ausbaustufe: Server-Events
      via Notifications-API statt Polling.)
- [ ] **Transfers**: Queue mit `max_parallel_transfers`, partielle Downloads
      (`Range`) mit `.part`-Tempdateien, atomares Umbenennen nach Abschluss,
      Read-Only-Attribute/Permissions pro OS behandeln (Windows: hidden,
      System-Attribute; Unix: Mode-Bits).
- [ ] Idempotenz: Absturz-sicher (`.part`-Dateien aufräumen, DB-Status
      `uploading`/`downloading` beim Start prüfen und Transfers neu anstoßen).
- [ ] Pause/Resume pro Job, globales Stop.
- [ ] Tests: Engine-Szenarien als Integrationstests mit Mock-WebDAV
      (`httpmock`): neuer File lokal/remote, Änderung lokal/remote, Löschung,
      Ordnerumbenennung, Filter, parallele Transfers.

### Phase 6 — Konflikthandhabung (in `oc-sync`, ca. 3–5 Tage)

- [ ] Erkennung: beidseitig geändert (siehe Phase 5) → statt blind zu
      überschreiben Eintrag in `conflicts`-Tabelle, Status `conflict` in
      `entries`, betroffene Datei wird **nicht** mehr angetastet, bis gelöst.
- [ ] Optional (Einstellung `conflict_policy`): statt Anfrage automatisch
      `keep_server` / `keep_local`, oder Konfliktkopie
      `name (conflicted copy <user> <date>).ext` lokal anlegen und Remote
      übernehmen.
- [ ] **Lösungsaktionen** (Commands von der GUI):
  - `resolve_server`: Download überschreibt lokal, DB aktualisieren, Konflikt `resolved_server`
  - `resolve_local`: Upload mit `If-Match=<Konflikt-ETag>` überschreibt Remote;
    scheitert das (ETag inzwischen wieder geändert) → Konflikt bleibt/neuer Konflikt
- [ ] GUI-freier Pfad: dieselben Aktionen per CLI (`oc-app --resolve <id> server|local`) → testbar.
- [ ] Tests: Konflikt entsteht, Aktionen, Remote-ändert-sich-währenddessen.

### Phase 7 — GUI (`oc-app`, ca. 1–2 Wochen)

- [ ] **Hauptfenster** mit drei Bereichen:
  1. **Konten & Server**: Server-URL, Login-Button (öffnet Browser, siehe
     Phase 2), Status (Token gültig, Space-Quota), Logout.
  2. **Sync-Jobs**: Tabelle aller Jobs (Space-Name, lokaler Pfad, Status,
     letzter Sync, Fortschritt). Pro Job: Pause/Resume, "Jetzt synchronisieren",
     Ordnerauswahl (s. u.), Verzeichnis in Dateimanager öffnen.
  3. **Konflikte**: Liste aller offenen Konflikte aller Jobs mit Zeitstempel,
     lokaler/remote mtime+size, Buttons je Zeile: "Server-Datei übernehmen",
     "Client-Datei behalten", "Alle Server / Alle lokal" als Sammelaktion.
- [ ] **Neuen Job anlegen** (Wizard): Space aus `GET /me/drives` wählen →
  lokales Zielverzeichnis wählen → **Ordnerauswahl pro Space**: Remote-Baum via
  PROPFIND laden, Checkboxen je Ordner → schreibt `include`-Liste → Job speichern.
- [ ] **Statusanzeige pro Verzeichnis**: Fortschritt während Sync
  (aktuelle Datei, Warteschlange), Fehlerliste, Konfliktanzahl als Badge im
  Tray-Tooltip.
- [ ] **Tray-Icon**: Status (Sync läuft / Fehler / Konflikt), Fenster
  zeigen/beenden, Pause alles. Fenster-Schließen minimiert in Tray (nicht
  beenden).
- [ ] Autostart je OS (Registry-Eintrag Windows, LaunchAgent macOS,
  `~/.config/autostart` Linux) als Einstellung.
- [ ] Engine-Anbindung ausschließlich über den Event-/Command-Kanal (kein
  gemeinsamer State) → GUI-Code dünn und testbar.

### Phase 8 — Robustheit & Reife (kontinuierlich, ca. 1 Woche)

- [ ] Offline-Verhalten: Netzwerk weg → Engine pausiert Jobs automatisch,
  Backoff beim Wiederverbinden; Queue bleibt erhalten.
- [ ] Große Dateien: TUS-Upload-Endpoint evaluieren, Download per Range-Resume.
- [ ] Ratenlimit / Serverlast: serielle PROPFINDs, konfigurierbare Intervalle.
- [ ] Logging: `tracing` + rotierende Logfiles im Config-Verzeichnis, Level aus
  `config.json`; Crash-Reporting-Hook optional.
- [ ] Icon-Overlays (Status pro Verzeichnis im Dateimanager) als spätere
  Ausbaustufe dokumentieren — nicht Teil von v1.

### Phase 9 — Packaging & Distribution (ca. 3–5 Tage)

- [ ] Release-Builds über CI:
  - Windows: NSIS/MSI-Installer, Codesigning optional
  - macOS: Universal-Binary (aarch64+x86_64), `.dmg`, Notarisierung
  - Linux: `.deb`, `.rpm`, AppImage
- [ ] Update-Benachrichtigung (Versionscheck gegen Releases-Feed), kein
  Auto-Update in v1.
- [ ] Handbuch/README: Installation, erste Schritte, Konfliktlösung,
  Konfigurationsreferenz.

---

## 4. Meilensteine & Reihenfolge

| Meilenstein | Inhalt | Ergebnis |
| --- | --- | --- |
| M1 | Phasen 0–1 | Workspace, CI, Konfiguration gelesen/geschrieben |
| M2 | Phase 2 | Login gegen echten OpenCloud-Server funktioniert, Token-Refresh |
| M3 | Phase 3 | Spaces auflisten, PROPFIND/PUT/GET gegen echten Server (CLI-Testprogramm) |
| M4 | Phase 4–5 | Headless-Sync eines Verzeichnisses mit einem Space funktioniert vollständig |
| M5 | Phase 6 | Konflikte werden erkannt und per CLI gelöst |
| M6 | Phase 7 | GUI komplett: Konfiguration, Status, Konfliktlösung |
| M7 | Phasen 8–9 | Stabilität, Installer für alle drei Plattformen |

Nach jedem Meilenstein: manuelles Testen gegen eine echte OpenCloud-Instanz
(docker-compose opencloud) mit Testfällen (Dual-Client-Konflikt erzeugen,
Ordnerfilter ändern, Netzwerk ziehen).

## 5. Risiken & offene Punkte

- **Events statt Polling**: Remote-Erkennung zunächst per PROPFind-Polling;
  Notifications-/Events-API für Push-Sync später prüfen (Doku:
  "notifications"-Service existiert).
- **Dateinamen**: plattform-illegale Zeichen (Windows `:?*`), Groß-/Kleinschreibung
  (case-insensitive FS) → Normalisierungs- und Sanitizing-Regeln in Phase 5
  festlegen und testen.
- **Gleichzeitige Clients**: Sync desselben Spaces durch zwei Instanzen → nur
  ETag-basiert, kein Locking in v1; Verhalten dokumentieren.
- **Keychain auf headless Linux**: Fallback auf `pass`-ähnlichen verschlüsselten
  Datei-Store, wenn kein Secret Service läuft.
- **GUI-Framework**: egui gewählt; falls Anforderungen (z. B. native File-Dialoge,
  komplexe Virtualisierung) wachsen, ist der GUI-Code isoliert genug, um auf
  iced/Tauri zu wechseln.
