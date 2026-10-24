# OpenCloud Sync Client

Ein plattformübergreifender (Windows / macOS / Linux) Synchronisations-Client
für [OpenCloud](https://opencloud.eu), geschrieben in Rust: beliebig viele
Ordner mit OpenCloud-Spaces synchronisieren — mit GUI, Tray-Icon,
Selective Sync und Konfliktlösung.

Ein unabhängiges Projekt, kein offizielles Produkt des OpenCloud-Projekts.

## Funktionen

- **Beliebig viele Sync-Jobs**: je Space und lokalem Ordner ein Job,
  beliebig kombinierbar; jeder Job hat seine eigene SQLite-Statusdatenbank.
- **Konfiguration als JSON**: `config.json` (Konten, Einstellungen) und
  `jobs.json` (Jobs) im Konfigurationsordner des Betriebssystems.
- **Selective Sync**: pro Space wählbar, welche Ordner synchronisiert
  werden (`include`-Whitelist), zusätzlich `exclude`-Muster.
- **Konfliktlösung**: beidseitig geänderte Dateien werden erkannt und
  angehalten, bis sie in der GUI (oder per CLI) entschieden sind —
  Server- oder Client-Version übernehmen, oder automatisch per Policy.
- **Robuste Transfers**: parallele Uploads/Downloads, Downloads mit
  Range-Fortsetzung über `.part`-Dateien und atomarem Umbenennen,
  ETag-basierte Änderungserkennung, Offline-Erkennung mit Backoff.
- **Tray-Icon**: Sync-Status, „Jetzt synchronisieren“-Zähler und Restzeit
  im Tooltip; Fenster schließen minimiert in den Tray.
- **Betriebssystem-Integration**: Autostart je OS, Icon in Taskleiste und
  Dateimanager; Refresh-Tokens liegen im OS-Schlüsselbund
  (Windows Credential Manager, macOS Keychain, Linux Secret Service).
- **Headless-Modus**: dieselbe Engine ohne GUI, für Server und Skripte.

## Installation

### Aus Releases

Releases werden über CI an die Tags `vX.Y.Z` gebaut und enthalten:

| Plattform | Paket | Hinweis |
| --- | --- | --- |
| Windows | `OpenCloud-Sync-<version>-setup.exe` (NSIS-Installer) | installiert nach `Programme\OpenCloud Sync`, Startmenü-Eintrag, Deinstallation über die Systemsteuerung |
| Windows | `OpenCloud-Sync-<version>-windows-portable.zip` | nur die EXE, ohne Installation |
| macOS | `OpenCloud-Sync-<version>-macOS.dmg` | Universal-Binary (Apple Silicon + Intel), App-Bundle `OpenCloud Sync.app` |
| Linux | `OpenCloud-Sync-<version>-linux-amd64.deb` | benötigt `libgtk-3-0`, `libsecret-1-0` |
| Linux | `OpenCloud-Sync-<version>-linux-amd64.rpm` | benötigt `gtk3`, `libsecret` |
| Linux | `OpenCloud-Sync-<version>-x86_64.AppImage` | gebündelte GTK-Bibliotheken, `--appimage-extract` bei Problemen |

Hinweise:

- **macOS ohne Codesigning/Notarisierung** (solange die Release-Signatur
  nicht konfiguriert ist): beim ersten Start die dmg per Rechtsklick →
  „Öffnen“ bestätigen.
- **Linux**: der Client braucht einen laufenden Secret Service
  (z. B. GNOME Keyring) für die Refresh-Tokens.

### Aus dem Source bauen

Voraussetzungen: Rust 1.85+ (rust-toolchain.toml gilt automatisch),
unter Linux `libgtk-3-dev`.

```sh
git clone <repository-url>
cd OpenCloud-Rust-Client
cargo build --release
# Binary: target/release/opencloud-sync
```

Entwicklungs-Befehle (die CI führt dieselben aus):

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Erste Schritte

1. **Konto anlegen**: GUI starten, Tab „Konten“ → „Konto hinzufügen“:
   Server-URL (z. B. `https://cloud.example.com`), Benutzername und eine
   Konto-ID vergeben.
2. **Anmelden**: „Anmelden“ öffnet den Browser für den OIDC-Login
   (Authorization Code Flow + PKCE). Das Refresh-Token landet im
   OS-Schlüsselbund — nicht in einer Datei. Abmelden widerruft es.
3. **Job anlegen**: „+ Neuer Job“ → Space auswählen → lokalen Ordner
   wählen → optional Ordner per Checkbox auswählen (Selective Sync,
   schreibt die `include`-Liste) → speichern. Der erste Lauf baut die
   lokale Kopie und die Statusdatenbank auf.
4. **Fertig**: Der Job läuft im Takt des Sync-Intervalls, reagiert auf
   lokale Dateiänderungen (Datei-Watcher) und auf Server-Änderungen
   (PROPFIND-Polling mit ETag-Shortcut). Fenster schließen minimiert
   in den Tray; „Beenden“ im Tray beendet das Programm.

## Konflikte lösen

Ändern sich eine Datei lokal *und* auf dem Server zwischen zwei Läufen,
hält der Client beide Versionen fest: die Datei wird nicht angetastet,
bis der Konflikt entschieden ist.

- **GUI**, Tab „Konflikte“: je Zeile „Server-Datei übernehmen“ (lädt die
  Serverversion herunter und überschreibt lokal) oder „Client-Datei
  behalten“ (lädt die lokale Version hoch, per ETag abgesichert).
  Sammelaktionen lösen alle Konflikte eines Space auf einmal.
- **CLI**:

  ```sh
  opencloud-sync --conflicts                     # offene Konflikte auflisten
  opencloud-sync --resolve <id> server|local     # entscheiden
  opencloud-sync --resolve <job-id>:<id> ...     # bei mehrdeutiger Id
  ```

- **Automatisch** (Einstellungen → „Bei Konflikten“): `ask` (Standard),
  `keep_server`, `keep_local` oder `conflict_copy` — letzteres legt die
  lokale Version als `name (conflicted copy <Datum> <Uhrzeit>).ext` ab
  und übernimmt dann die Serverversion; die Kopie synchronisiert beim
  nächsten Lauf als neue Datei.

Ändert sich der Server während der Lösung nochmal, bleibt der Konflikt
offen und der neue Serverzustand wird nachgetragen.

## CLI-Referenz

```text
opencloud-sync                           # GUI (Standard)
opencloud-sync --headless                # Engine ohne GUI, läuft bis Ctrl-C
opencloud-sync --login <ACCOUNT_ID>      # Browser-Login, Refresh-Token -> Keyring
opencloud-sync --logout <ACCOUNT_ID>     # Token widerrufen, Keyring leeren
opencloud-sync --status                  # Jobs, Logins, DB-Statistiken
opencloud-sync --sync-once               # ein Lauf pro aktivem Job (für cron)
opencloud-sync --list-spaces <ACCOUNT_ID> # Spaces des Kontos (Graph-API)
opencloud-sync --conflicts               # offene Konflikte auflisten
opencloud-sync --resolve <id> server|local
opencloud-sync --config-dir <DIR>        # Konfigurationsordner umstellen
```

`--headless --login <id>` verbindet beides: einloggen und die Engine
starten. Die Umgebungsvariable `OPENCLOUD_SYNC_CONFIG_DIR` wirkt wie
`--config-dir`.

## Konfigurationsreferenz

Die Dateien liegen im OS-Konfigurationsordner:

| OS | Ordner |
| --- | --- |
| Linux | `~/.config/opencloud-sync/` |
| macOS | `~/Library/Application Support/opencloud-sync/` |
| Windows | `%APPDATA%\opencloud-sync\` |

Dazu: `db/` (eine SQLite-Datei pro Job) und `logs/` (täglich rotierende
Protokolle). Beide Dateien sind atomar geschrieben und per `version`-Feld
migrationssicher; die GUI bearbeitet sie, Handbearbeitung ist aber möglich
(solange das Programm nicht läuft).

### `config.json`

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
    "propfind_pause_ms": 0,
    "conflict_policy": "ask",
    "log_level": "info",
    "autostart": false,
    "check_updates": true
  }
}
```

| Feld | Bedeutung | Grenzen / Standard |
| --- | --- | --- |
| `accounts[].id` | interne Konto-ID, Referenz der Jobs | nicht leer, eindeutig |
| `accounts[].server_url` | Basis-URL des Servers | HTTP(S) mit Host |
| `accounts[].username` | Anzeigename des Benutzers | nicht leer |
| `settings.sync_interval_secs` | Takt der periodischen Scans | 1–86400, Standard 30 |
| `settings.max_parallel_transfers` | parallele Uploads/Downloads | 1–32, Standard 4 |
| `settings.propfind_pause_ms` | Pause zwischen Ordner-PROPFINDs (Schonung des Servers) | 0–10000, Standard 0 |
| `settings.conflict_policy` | `ask` \| `keep_server` \| `keep_local` \| `conflict_copy` | Standard `ask` |
| `settings.log_level` | `error` \| `warn` \| `info` \| `debug` \| `trace` | Standard `info`; `RUST_LOG` überschreibt |
| `settings.autostart` | Programm bei Anmeldung starten (Eintrag je OS) | Standard `false` |
| `settings.check_updates` | beim Start gegen den Releases-Feed prüfen | Standard `true` |

### `jobs.json`

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

| Feld | Bedeutung |
| --- | --- |
| `id` | Job-ID; Name der Statusdatenbank, gültiger Dateiname, eindeutig |
| `account_id` | referenziert `config.json` |
| `space_id` | Space-ID (in der GUI im Wizard, per CLI mit `--list-spaces` zu sehen) |
| `local_path` | lokaler Ordner; wird angelegt, falls er fehlt. Ordner zweier Jobs dürfen sich nicht überlappen |
| `enabled` | `false` = Job wird übersprungen |
| `include` | Ordner-Whitelist (Selective Sync): leer = alles. Pfade relativ zur Space-Wurzel |
| `exclude` | Glob-Muster je Ordnersegment oder Pfad, z. B. `*.tmp`, `.git`, `Bilder/2025` |

## Updates

Der Client aktualisiert sich nicht selbst: beim Start prüft er (sofern
`check_updates` an ist) die neueste Release-Version und benachrichtigt im
Info-Tab mit Link zur Release-Seite. Die Prüfung ist aktiv, sobald das
Projekt veröffentlicht ist; aktuell zeigt der Info-Tab entsprechend einen
Platzhalter.

## Protokolle

Täglich rotierende Dateien unter `<config-dir>/logs/opencloud-sync.log.<Datum>`,
Level aus `settings.log_level`; `RUST_LOG=debug` überschreibt ihn zur
Fehlersuche (z. B. `RUST_LOG=oc_sync=trace`). Beim Melden von Fehlern
helfen die Protokolle aus dem Info-Tab („Öffnen“).

## Grenzen

- **Zwei Clients, ein Space**: es gibt kein Server-Locking; die
  Änderungserkennung ist ETag-basiert. Gleichzeitige Bearbeitung führt
  zu einem Konflikt, den der Client erkennt und festhält.
- **Icon-Overlays im Dateimanager** (Häkchen auf Dateien) sind eine
  spätere Ausbaustufe (plattformabhängige Shell-Integration).
- **Große Uploads** laufen als ein PUT-Request; TUS (stükeweises
  Hochladen) ist für sehr große Dateien als Ausbaustufe notiert.

## Entwicklung

- Workspace: `oc-config` (JSON-Konfiguration), `oc-auth` (OIDC + Keyring),
  `oc-api` (Graph + WebDAV), `oc-db` (SQLite je Job), `oc-sync` (Engine),
  `oc-app` (GUI/CLI).
- `PLAN.md` ist das maßgebliche Design-Dokument, `CHANGELOG.md` hält die
  Änderungen fest, `CLAUDE.md` beschreibt Konventionen und Architektur.
- Tests: Unit- und Integrationstests mit Mock-Servern (`wiremock`);
  Release-Builds entstehen über `.github/workflows/release.yml` an Tags
  `vX.Y.Z`; die Installations-Assets liegen unter `packaging/`.

## Lizenz

[Apache License 2.0](LICENSE). Der Client nutzt die Graph- und WebDAV-APIs
von OpenCloud und ist kein offizielles Produkt des OpenCloud-Projekts.
