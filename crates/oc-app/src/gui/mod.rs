//! The GUI (Phase 7): main window with the three areas "Konten & Server",
//! "Sync-Jobs", and "Konflikte", plus the new-job wizard and the tray icon.
//!
//! The engine is attached exclusively through the backend's event and
//! command channels (see [`backend`]); the GUI additionally reads job
//! statistics and pending conflicts from the jobs' SQLite databases
//! (its own connections, WAL mode allows concurrent readers).

mod autostart;
pub mod backend;
mod tray;
mod wizard;

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use std::time::Instant;

use eframe::App;
use eframe::egui;
use egui::CentralPanel;
use egui::ComboBox;
use egui::Context;
use egui::DragValue;
use egui::Grid;
use egui::ScrollArea;
use egui::TopBottomPanel;
use egui::ViewportCommand;
use oc_api::Space;
use oc_auth::{AuthClient, KeyringStore, TokenManager};
use oc_config::{Account, Config, ConflictPolicy, Jobs, LogLevel, Settings, SyncJob};
use oc_db::{Conflict, ConflictResolution, Stats, StatusDb, db_path};
use oc_sync::EngineEvent;
use oc_sync::JobCommand;
use oc_sync::RunSummary;

use crate::gui::backend::BackendEvent;
use crate::gui::backend::BackendHandle;
use crate::gui::backend::UiCommand;
use crate::gui::tray::Tray;
use crate::gui::tray::TrayAction;
use crate::gui::wizard::Mode;
use crate::gui::wizard::Wizard;
use crate::gui::wizard::derive_include;

/// Which main tab is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// Accounts, login, and settings.
    Accounts,
    /// The job table.
    Jobs,
    /// Pending conflicts.
    Conflicts,
}

/// UI state of one account.
#[derive(Debug, Default)]
struct AccountUi {
    /// `Some(true/false)` = logged in according to the keyring.
    logged_in: Option<bool>,
    /// A login is in flight.
    login_pending: bool,
    /// A logout is in flight.
    logout_pending: bool,
    /// Loaded spaces (for the account's quota display).
    spaces: Option<Result<Vec<Space>, String>>,
    /// A spaces request is in flight.
    spaces_pending: bool,
}

/// UI state of one job (events, statistics, conflicts).
#[derive(Debug, Default)]
struct JobUi {
    /// A sync pass is running.
    running: bool,
    /// The job was paused via the UI.
    paused: bool,
    /// What the last finished pass did.
    last_summary: Option<RunSummary>,
    /// Database statistics (refreshed once a second).
    stats: Option<Stats>,
    /// Pending conflicts (refreshed once a second).
    conflicts: Vec<Conflict>,
    /// Entries currently transferring (from the last stats refresh).
    transfers: Vec<String>,
}

/// Form state for a new account.
#[derive(Debug, Default)]
struct NewAccountForm {
    /// Chosen account id.
    id: String,
    /// Server URL.
    server_url: String,
    /// User name.
    username: String,
    /// Validation error, if any.
    error: Option<String>,
}

/// The eframe application.
struct SyncApp {
    /// Backend handle for commands.
    backend: BackendHandle,
    /// Backend events, polled each frame.
    events: Receiver<BackendEvent>,
    /// The configuration directory.
    config_dir: PathBuf,
    /// Loaded `config.json` (GUI's copy).
    config: Config,
    /// Loaded `jobs.json` (GUI's copy).
    jobs: Jobs,
    /// Editable settings (Konten tab).
    settings_edit: Settings,
    /// Per-account UI state.
    accounts: HashMap<String, AccountUi>,
    /// Per-job UI state.
    job_states: HashMap<String, JobUi>,
    /// Open status databases (GUI-thread connections).
    dbs: HashMap<String, StatusDb>,
    /// Form for adding an account.
    new_account: NewAccountForm,
    /// The active main tab.
    tab: Tab,
    /// The wizard, when open.
    wizard: Option<Wizard>,
    /// Pending job deletion confirmation.
    confirm_remove: Option<String>,
    /// Last status message (with timestamp) for the bottom line.
    message: Option<(String, Instant)>,
    /// Recent failures (ring buffer, newest last).
    errors: Vec<String>,
    /// Whether all jobs are paused (tray toggle).
    paused_all: bool,
    /// The tray icon, when the platform allows one.
    tray: Option<Tray>,
    /// Whether the OS autostart entry is currently present.
    autostart_active: bool,
    /// When the job databases were refreshed last.
    last_refresh: Instant,
}

impl SyncApp {
    /// Builds the app: loads config and jobs, checks the stored logins,
    /// and creates the tray icon.
    fn new(
        config_dir: PathBuf,
        backend: BackendHandle,
        events: Receiver<BackendEvent>,
    ) -> Result<Self, String> {
        let config = oc_config::load_config(&config_dir).map_err(|error| error.to_string())?;
        let jobs = oc_config::load_jobs(&config_dir).map_err(|error| error.to_string())?;
        oc_config::validate_references(&config, &jobs).map_err(|error| error.to_string())?;
        let mut accounts = HashMap::new();
        for account in &config.accounts {
            accounts.insert(
                account.id.clone(),
                AccountUi {
                    logged_in: stored_login(account),
                    ..AccountUi::default()
                },
            );
        }
        Ok(Self {
            backend,
            events,
            settings_edit: config.settings.clone(),
            config_dir,
            config,
            jobs,
            accounts,
            job_states: HashMap::new(),
            dbs: HashMap::new(),
            new_account: NewAccountForm::default(),
            tab: Tab::Jobs,
            wizard: None,
            confirm_remove: None,
            message: None,
            errors: Vec::new(),
            paused_all: false,
            tray: Tray::new(),
            autostart_active: crate::gui::autostart::is_enabled(),
            last_refresh: Instant::now() - Duration::from_secs(10),
        })
    }

    /// Reports a message to the bottom status line.
    fn notify(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
    }

    /// Drains all pending backend events into the UI state.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.apply_event(event);
        }
    }

    /// Applies one backend event.
    fn apply_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Engine(event) => self.apply_engine_event(event),
            BackendEvent::LoginFinished { account_id, result } => {
                if let Some(state) = self.accounts.get_mut(&account_id) {
                    state.login_pending = false;
                    state.logged_in = Some(result.is_ok());
                }
                match result {
                    Ok(()) => self.notify(format!("Konto '{account_id}' angemeldet")),
                    Err(error) => self.notify(format!("Login fehlgeschlagen: {error}")),
                }
                self.engine_restarted();
            }
            BackendEvent::LogoutFinished { account_id, result } => {
                if let Some(state) = self.accounts.get_mut(&account_id) {
                    state.logout_pending = false;
                    state.logged_in = Some(result.is_err());
                }
                match result {
                    Ok(()) => self.notify(format!("Konto '{account_id}' abgemeldet")),
                    Err(error) => self.notify(format!("Abmeldung fehlgeschlagen: {error}")),
                }
                self.engine_restarted();
            }
            BackendEvent::Spaces { account_id, result } => {
                if let Some(wizard) = &mut self.wizard {
                    if wizard.account_id.as_deref() == Some(account_id.as_str()) {
                        wizard.apply_spaces(result.clone());
                    }
                }
                if let Some(state) = self.accounts.get_mut(&account_id) {
                    state.spaces_pending = false;
                    state.spaces = Some(result);
                }
            }
            BackendEvent::Children { token, result } => {
                if let Some(wizard) = &mut self.wizard {
                    wizard.apply_children(token, result);
                }
            }
            BackendEvent::ResolveFinished {
                job_id,
                conflict_id,
                result,
            } => match result {
                Ok(true) => {
                    self.notify(format!("Konflikt #{conflict_id} in '{job_id}' gelöst"));
                    self.refresh_dbs();
                }
                Ok(false) => {
                    self.notify(format!(
                        "Konflikt #{conflict_id} in '{job_id}' bleibt offen: \
                         der Server hat sich zwischenzeitlich geändert"
                    ));
                    self.refresh_dbs();
                }
                Err(error) => self.notify(format!("Konfliktlösung fehlgeschlagen: {error}")),
            },
            BackendEvent::Message { message } => self.notify(message),
        }
    }

    /// Applies one engine event.
    fn apply_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::RunStarted { job_id } => {
                self.job_states.entry(job_id).or_default().running = true;
            }
            EngineEvent::RunFinished { job_id, summary } => {
                let state = self.job_states.entry(job_id).or_default();
                state.running = false;
                state.last_summary = Some(summary);
            }
            EngineEvent::Failed {
                job_id,
                path,
                error,
            } => {
                let line = if path.is_empty() {
                    format!("[{job_id}] {error}")
                } else {
                    format!("[{job_id}] {path}: {error}")
                };
                self.errors.push(line);
                let overflow = self.errors.len().saturating_sub(200);
                self.errors.drain(0..overflow);
            }
            EngineEvent::Conflict { job_id, path } => {
                self.notify(format!("Konflikt in '{job_id}': {path}"));
            }
            EngineEvent::ConflictResolved { job_id, path, .. } => {
                self.notify(format!("Konflikt in '{job_id}' gelöst: {path}"));
                self.refresh_dbs();
            }
            EngineEvent::Stopped { job_id } => {
                let state = self.job_states.entry(job_id).or_default();
                state.running = false;
            }
        }
    }

    /// Resets the pause states after an engine restart (login/logout,
    /// ApplyJobs): the fresh engine starts unpaused.
    fn engine_restarted(&mut self) {
        self.paused_all = false;
        if let Some(tray) = &mut self.tray {
            tray.set_paused(false);
        }
        for state in self.job_states.values_mut() {
            state.paused = false;
        }
    }

    /// Opens/refreshes the per-job status databases and pulls statistics
    /// and pending conflicts (own connections; WAL allows concurrency).
    fn refresh_dbs(&mut self) {
        self.last_refresh = Instant::now();
        let job_ids: Vec<(String, PathBuf)> = self
            .jobs
            .jobs
            .iter()
            .map(|job| (job.id.clone(), job.local_path.clone()))
            .collect();
        for (job_id, _local_root) in job_ids {
            let Ok(path) = db_path(&self.config_dir, &job_id) else {
                continue;
            };
            if !path.exists() {
                self.dbs.remove(&job_id);
                if let Some(state) = self.job_states.get_mut(&job_id) {
                    state.stats = None;
                    state.conflicts.clear();
                }
                continue;
            }
            if !self.dbs.contains_key(&job_id) {
                match StatusDb::open(&path) {
                    Ok(db) => {
                        self.dbs.insert(job_id.clone(), db);
                    }
                    Err(error) => {
                        self.notify(format!("Status-Datenbank '{job_id}': {error}"));
                        continue;
                    }
                }
            }
            let state = self.job_states.entry(job_id.clone()).or_default();
            let Some(db) = self.dbs.get(&job_id) else {
                continue;
            };
            if let Ok(stats) = db.stats() {
                state.stats = Some(stats);
            }
            if let Ok(conflicts) = db.pending_conflicts() {
                state.conflicts = conflicts;
            }
            let mut transfers = Vec::new();
            if let Ok(uploading) = db.entries_with_state(oc_db::EntryState::Uploading, 8) {
                transfers.extend(uploading.iter().map(|entry| entry.entry.path.clone()));
            }
            if let Ok(downloading) = db.entries_with_state(oc_db::EntryState::Downloading, 8) {
                transfers.extend(downloading.iter().map(|entry| entry.entry.path.clone()));
            }
            state.transfers = transfers;
        }
    }

    /// Handles tray menu actions and the close-to-tray behavior.
    fn poll_tray(&mut self, ctx: &Context) {
        // The close button hides the window instead of quitting (tray);
        // quitting goes through the tray menu.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
        let actions = self
            .tray
            .as_ref()
            .map(|tray| tray.poll())
            .unwrap_or_default();
        for action in actions {
            match action {
                TrayAction::Show => {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                TrayAction::TogglePause => {
                    self.paused_all = !self.paused_all;
                    if let Some(tray) = self.tray.as_ref() {
                        tray.set_paused(self.paused_all);
                    }
                    if self.paused_all {
                        self.backend.send(UiCommand::PauseAll);
                        self.notify("Alle Jobs pausiert");
                    } else {
                        self.backend.send(UiCommand::ResumeAll);
                        self.notify("Alle Jobs fortgesetzt");
                    }
                }
                TrayAction::Quit => {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    /// Updates the tray tooltip from the current job states.
    fn update_tray_status(&mut self) {
        let running = self.job_states.values().any(|state| state.running);
        let errors: u64 = self
            .job_states
            .values()
            .filter_map(|state| state.stats.map(|stats| stats.errors))
            .sum();
        let conflicts: u64 = self
            .job_states
            .values()
            .filter_map(|state| state.stats.map(|stats| stats.conflicts))
            .sum();
        if let Some(tray) = &mut self.tray {
            tray.set_status(running, errors, conflicts);
        }
    }

    /// The bottom status line: message, error count, pause state.
    fn show_status_line(&mut self, ctx: &Context) {
        TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let conflicts: u64 = self
                    .job_states
                    .values()
                    .map(|state| state.conflicts.len() as u64)
                    .sum();
                if let Some((message, at)) = &self.message {
                    if at.elapsed() < Duration::from_secs(10) {
                        ui.label(message);
                    } else {
                        ui.label("Bereit");
                    }
                } else {
                    ui.label("Bereit");
                }
                ui.separator();
                ui.label(format!("Jobs: {}", self.jobs.jobs.len()));
                if conflicts > 0 {
                    ui.separator();
                    ui.label(format!("Konflikte: {conflicts}"));
                }
                if !self.errors.is_empty() {
                    ui.separator();
                    ui.label(format!("Fehler (letzte Läufe): {}", self.errors.len()));
                }
                if self.paused_all {
                    ui.separator();
                    ui.label("Alle pausiert");
                }
            });
        });
    }

    /// The three main tabs; the conflicts tab is badged with their count.
    fn show_tabs(&mut self, ctx: &Context) {
        TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let conflicts: u64 = self
                    .job_states
                    .values()
                    .map(|state| state.conflicts.len() as u64)
                    .sum();
                ui.selectable_value(&mut self.tab, Tab::Accounts, "Konten & Server");
                ui.selectable_value(&mut self.tab, Tab::Jobs, "Sync-Jobs");
                let conflicts_label = if conflicts > 0 {
                    format!("Konflikte ({conflicts})")
                } else {
                    "Konflikte".to_string()
                };
                ui.selectable_value(&mut self.tab, Tab::Conflicts, conflicts_label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.paused_all {
                        if ui.button("Alle fortsetzen").clicked() {
                            self.paused_all = false;
                            if let Some(tray) = &mut self.tray {
                                tray.set_paused(false);
                            }
                            self.backend.send(UiCommand::ResumeAll);
                        }
                    } else if ui.button("Alle pausieren").clicked() {
                        self.paused_all = true;
                        if let Some(tray) = &mut self.tray {
                            tray.set_paused(true);
                        }
                        self.backend.send(UiCommand::PauseAll);
                    }
                    if self.wizard.is_none() && ui.button("Neuen Job anlegen…").clicked() {
                        self.wizard = Some(Wizard::new());
                    }
                });
            });
        });
    }

    /// The "Konten & Server" tab: accounts with login state, the spaces
    /// and quota view, the add-account form, and the settings.
    fn show_accounts(&mut self, ui: &mut egui::Ui) {
        ScrollArea::vertical().show(ui, |ui| {
            for account in self.config.accounts.clone() {
                let state = self.accounts.entry(account.id.clone()).or_default();
                ui.group(|ui| {
                    Grid::new(format!("account-{}", account.id))
                        .num_columns(2)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(&account.id).strong());
                            ui.label(format!("{} ({})", account.server_url, account.username));
                            ui.end_row();
                            match state.logged_in {
                                Some(true) => {
                                    ui.label(
                                        egui::RichText::new("Angemeldet")
                                            .color(egui::Color32::from_rgb(0, 128, 0)),
                                    );
                                }
                                Some(false) => {
                                    ui.label("Nicht angemeldet");
                                }
                                None => {
                                    ui.label("Status unbekannt");
                                }
                            }
                            ui.horizontal(|ui| {
                                if state.login_pending {
                                    ui.label("Login läuft (Browser öffnen)…");
                                } else if ui.button("Anmelden").clicked() {
                                    state.login_pending = true;
                                    self.backend.send(UiCommand::Login {
                                        account_id: account.id.clone(),
                                    });
                                }
                                if state.logout_pending {
                                    ui.label("Abmeldung…");
                                } else if state.logged_in == Some(true)
                                    && ui.button("Abmelden").clicked()
                                {
                                    state.logout_pending = true;
                                    self.backend.send(UiCommand::Logout {
                                        account_id: account.id.clone(),
                                    });
                                }
                                if state.spaces_pending {
                                    ui.label("Lade Spaces…");
                                } else if ui.button("Spaces & Quota").clicked() {
                                    state.spaces_pending = true;
                                    state.spaces = None;
                                    self.backend.send(UiCommand::ListSpaces {
                                        account_id: account.id.clone(),
                                    });
                                }
                            });
                            ui.end_row();
                            if let Some(spaces) = &state.spaces {
                                ui.label("");
                                ui.vertical(|ui| match spaces {
                                    Ok(spaces) if spaces.is_empty() => {
                                        ui.label("Keine Spaces.");
                                    }
                                    Ok(spaces) => {
                                        for space in spaces {
                                            let quota = space
                                                .quota
                                                .as_ref()
                                                .map(|quota| {
                                                    format!(
                                                        " — {} / {}",
                                                        format_bytes(quota.used.unwrap_or(0)),
                                                        quota
                                                            .total
                                                            .map(format_bytes)
                                                            .unwrap_or_else(|| "?".into())
                                                    )
                                                })
                                                .unwrap_or_default();
                                            ui.label(format!(
                                                "{} [{}]{}",
                                                space.name, space.drive_type, quota
                                            ));
                                        }
                                    }
                                    Err(error) => {
                                        ui.label(
                                            egui::RichText::new(error)
                                                .color(egui::Color32::from_rgb(180, 0, 0)),
                                        );
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });
            }

            ui.add_space(8.0);
            ui.heading("Konto hinzufügen");
            Grid::new("new-account").num_columns(2).show(ui, |ui| {
                ui.label("Konto-ID");
                ui.text_edit_singleline(&mut self.new_account.id);
                ui.end_row();
                ui.label("Server-URL");
                ui.text_edit_singleline(&mut self.new_account.server_url);
                ui.end_row();
                ui.label("Benutzername");
                ui.text_edit_singleline(&mut self.new_account.username);
                ui.end_row();
                ui.label("");
                if ui.button("Hinzufügen").clicked() {
                    self.add_account();
                }
                if let Some(error) = &self.new_account.error {
                    ui.end_row();
                    ui.label("");
                    ui.label(egui::RichText::new(error).color(egui::Color32::from_rgb(180, 0, 0)));
                }
            });

            ui.add_space(8.0);
            ui.heading("Einstellungen");
            Grid::new("settings").num_columns(2).show(ui, |ui| {
                ui.label("Sync-Intervall (Sekunden)");
                ui.add(DragValue::new(&mut self.settings_edit.sync_interval_secs).range(1..=86400));
                ui.end_row();
                ui.label("Parallele Transfers");
                ui.add(
                    DragValue::new(&mut self.settings_edit.max_parallel_transfers).range(1..=32),
                );
                ui.end_row();
                ui.label("Konflikt-Richtlinie");
                ComboBox::from_id_salt("conflict-policy")
                    .selected_text(conflict_policy_label(self.settings_edit.conflict_policy))
                    .show_ui(ui, |ui| {
                        for policy in [
                            ConflictPolicy::Ask,
                            ConflictPolicy::KeepServer,
                            ConflictPolicy::KeepLocal,
                            ConflictPolicy::ConflictCopy,
                        ] {
                            ui.selectable_value(
                                &mut self.settings_edit.conflict_policy,
                                policy,
                                conflict_policy_label(policy),
                            );
                        }
                    });
                ui.end_row();
                ui.label("Log-Level");
                ComboBox::from_id_salt("log-level")
                    .selected_text(log_level_label(self.settings_edit.log_level))
                    .show_ui(ui, |ui| {
                        for level in [
                            LogLevel::Error,
                            LogLevel::Warn,
                            LogLevel::Info,
                            LogLevel::Debug,
                            LogLevel::Trace,
                        ] {
                            ui.selectable_value(
                                &mut self.settings_edit.log_level,
                                level,
                                log_level_label(level),
                            );
                        }
                    });
                ui.end_row();
                ui.label("Bei Anmeldung starten");
                let mut autostart = self.settings_edit.autostart;
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut autostart, "").changed() {
                        let result = if autostart {
                            crate::gui::autostart::enable()
                        } else {
                            crate::gui::autostart::disable()
                        };
                        match result {
                            Ok(()) => {
                                self.settings_edit.autostart = autostart;
                                self.autostart_active = autostart;
                                self.notify(if autostart {
                                    "Autostart aktiviert"
                                } else {
                                    "Autostart deaktiviert"
                                });
                            }
                            Err(error) => self.notify(format!("Autostart: {error}")),
                        }
                    }
                    ui.weak(if self.autostart_active {
                        "(im Betriebssystem aktiv)"
                    } else {
                        "(im Betriebssystem inaktiv)"
                    });
                });
                ui.end_row();
                ui.label("");
                if ui.button("Einstellungen speichern").clicked() {
                    self.save_settings();
                }
            });
        });
    }

    /// Adds the account from the form to `config.json`.
    fn add_account(&mut self) {
        let account = Account {
            id: self.new_account.id.trim().to_owned(),
            server_url: self.new_account.server_url.trim().to_owned(),
            username: self.new_account.username.trim().to_owned(),
        };
        if let Err(error) = account.validate() {
            self.new_account.error = Some(error.to_string());
            return;
        }
        if self.config.accounts.iter().any(|a| a.id == account.id) {
            self.new_account.error = Some(format!("Konto '{}' existiert bereits", account.id));
            return;
        }
        self.new_account.error = None;
        self.accounts.insert(
            account.id.clone(),
            AccountUi {
                logged_in: Some(false),
                ..AccountUi::default()
            },
        );
        self.config.accounts.push(account);
        self.persist_config();
        self.new_account = NewAccountForm::default();
    }

    /// Persists the GUI's config copy.
    fn persist_config(&mut self) {
        match oc_config::save_config(&self.config_dir, &self.config) {
            Ok(()) => self.notify("Konfiguration gespeichert"),
            Err(error) => self.notify(format!("Speichern fehlgeschlagen: {error}")),
        }
    }

    /// Persists the GUI's jobs copy and restarts the engine.
    fn persist_jobs(&mut self) {
        match oc_config::save_jobs(&self.config_dir, &self.jobs) {
            Ok(()) => {
                self.notify("Jobs gespeichert");
                self.backend.send(UiCommand::ApplyJobs);
                self.engine_restarted();
            }
            Err(error) => self.notify(format!("Speichern fehlgeschlagen: {error}")),
        }
    }

    /// Saves the edited settings and restarts the engine (interval,
    /// parallel transfers, conflict policy).
    fn save_settings(&mut self) {
        self.config.settings = self.settings_edit.clone();
        self.persist_config();
        self.backend.send(UiCommand::ApplyJobs);
        self.engine_restarted();
    }

    /// The "Sync-Jobs" tab.
    fn show_jobs(&mut self, ui: &mut egui::Ui) {
        if self.jobs.jobs.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label("Noch keine Sync-Jobs.");
                if ui.button("Neuen Job anlegen…").clicked() {
                    self.wizard = Some(Wizard::new());
                }
            });
            return;
        }
        ScrollArea::vertical().show(ui, |ui| {
            for job in self.jobs.jobs.clone() {
                self.show_job_row(ui, &job);
            }
            self.show_error_log(ui);
        });
    }

    /// The recent failures of all jobs (newest first).
    fn show_error_log(&mut self, ui: &mut egui::Ui) {
        if self.errors.is_empty() {
            return;
        }
        ui.add_space(8.0);
        let count = self.errors.len();
        ui.collapsing(format!("Fehlerliste ({count})"), |ui| {
            for line in self.errors.iter().rev().take(100) {
                ui.label(line);
            }
        });
    }

    /// One row of the job table with its actions.
    fn show_job_row(&mut self, ui: &mut egui::Ui, job: &SyncJob) {
        let state = self.job_states.entry(job.id.clone()).or_default();
        let space = job
            .space_name
            .clone()
            .unwrap_or_else(|| job.space_id.clone());
        let status = if state.running {
            egui::RichText::new("Synchronisiert…").color(egui::Color32::from_rgb(0, 90, 200))
        } else if state.paused {
            egui::RichText::new("Pausiert").color(egui::Color32::from_rgb(180, 120, 0))
        } else if let Some(stats) = state.stats {
            if stats.conflicts > 0 {
                egui::RichText::new(format!("{} Konflikt(e)", stats.conflicts))
                    .color(egui::Color32::from_rgb(180, 0, 0))
            } else if stats.errors > 0 {
                egui::RichText::new(format!("{} Fehler", stats.errors))
                    .color(egui::Color32::from_rgb(180, 0, 0))
            } else if stats.transfers > 0 {
                egui::RichText::new(format!("{} Transfer(s)", stats.transfers))
            } else {
                egui::RichText::new("Aktuell").color(egui::Color32::from_rgb(0, 128, 0))
            }
        } else {
            egui::RichText::new("Noch nie synchronisiert")
        };
        let last_sync = state
            .stats
            .and_then(|stats| stats.last_sync)
            .map(format_unix)
            .unwrap_or_else(|| "—".into());
        let files = state
            .stats
            .map(|stats| format!("{} / {}", stats.files, stats.dirs))
            .unwrap_or_else(|| "—".into());

        ui.group(|ui| {
            Grid::new(format!("job-{}", job.id))
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(space).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Entfernen…").clicked() {
                            self.confirm_remove = Some(job.id.clone());
                        }
                        if ui.button("Ordner…").clicked() {
                            self.wizard = Some(Wizard::edit_folders(job));
                        }
                        if ui.button("Öffnen").clicked() {
                            open_in_file_manager(&job.local_path);
                        }
                        if state.paused {
                            if ui.button("Fortsetzen").clicked() {
                                state.paused = false;
                                self.backend.send(UiCommand::JobCmd {
                                    job_id: job.id.clone(),
                                    command: JobCommand::Resume,
                                });
                            }
                        } else if ui.button("Pausieren").clicked() {
                            state.paused = true;
                            self.backend.send(UiCommand::JobCmd {
                                job_id: job.id.clone(),
                                command: JobCommand::Pause,
                            });
                        }
                        if ui.button("Jetzt synchronisieren").clicked() {
                            state.paused = false;
                            self.backend.send(UiCommand::JobCmd {
                                job_id: job.id.clone(),
                                command: JobCommand::SyncNow,
                            });
                        }
                    });
                    ui.end_row();
                    ui.label(job.local_path.display().to_string());
                    ui.label(status);
                    ui.end_row();
                    ui.label(format!("Letzter Sync: {last_sync}"));
                    ui.label(format!("Dateien/Ordner: {files}"));
                    ui.end_row();
                    if !state.transfers.is_empty() {
                        ui.label(format!("Aktuell: {}", state.transfers.join(", ")));
                        ui.end_row();
                    }
                    if let Some(summary) = &state.last_summary {
                        ui.label(format!(
                            "Letzter Lauf: {summary}{}",
                            if summary.remote_scan_skipped {
                                " (Remote-Scan übersprungen)"
                            } else {
                                ""
                            }
                        ));
                        ui.end_row();
                    }
                });
        });
    }

    /// The "Konflikte" tab: all pending conflicts with both sides' state
    /// and the resolution buttons (see `PLAN.md`, Phase 6/7).
    fn show_conflicts(&mut self, ui: &mut egui::Ui) {
        let conflicts: Vec<(String, Vec<Conflict>)> = self
            .jobs
            .jobs
            .iter()
            .map(|job| {
                let state = self.job_states.entry(job.id.clone()).or_default();
                (job.id.clone(), state.conflicts.clone())
            })
            .collect();
        let total: usize = conflicts.iter().map(|(_, list)| list.len()).sum();
        if total == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label("Keine offenen Konflikte.");
            });
            return;
        }
        ScrollArea::vertical().show(ui, |ui| {
            for (job_id, list) in conflicts {
                ui.heading(format!("Job '{job_id}'"));
                if !list.is_empty() {
                    ui.horizontal(|ui| {
                        if ui.button("Alle: Server übernehmen").clicked() {
                            for conflict in &list {
                                self.backend.send(UiCommand::ResolveConflict {
                                    job_id: job_id.clone(),
                                    conflict_id: conflict.id,
                                    resolution: ConflictResolution::Server,
                                });
                            }
                        }
                        if ui.button("Alle: Client behalten").clicked() {
                            for conflict in &list {
                                self.backend.send(UiCommand::ResolveConflict {
                                    job_id: job_id.clone(),
                                    conflict_id: conflict.id,
                                    resolution: ConflictResolution::Local,
                                });
                            }
                        }
                    });
                }
                for conflict in list {
                    ui.group(|ui| {
                        Grid::new(format!("conflict-{}-{}", job_id, conflict.id))
                            .num_columns(2)
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "#{} {}",
                                        conflict.id, conflict.path
                                    ))
                                    .strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("Client behalten").clicked() {
                                            self.backend.send(UiCommand::ResolveConflict {
                                                job_id: job_id.clone(),
                                                conflict_id: conflict.id,
                                                resolution: ConflictResolution::Local,
                                            });
                                        }
                                        if ui.button("Server übernehmen").clicked() {
                                            self.backend.send(UiCommand::ResolveConflict {
                                                job_id: job_id.clone(),
                                                conflict_id: conflict.id,
                                                resolution: ConflictResolution::Server,
                                            });
                                        }
                                    },
                                );
                                ui.end_row();
                                ui.label(format!("Erkannt: {}", format_unix(conflict.detected_at)));
                                ui.label("");
                                ui.end_row();
                                ui.label(format!(
                                    "Lokal: {}",
                                    describe_side(conflict.local_size, conflict.local_mtime)
                                ));
                                ui.label(format!(
                                    "Server: {}",
                                    describe_side(conflict.remote_size, conflict.remote_mtime)
                                ));
                                ui.end_row();
                            });
                    });
                }
            }
        });
    }

    /// The wizard (full-area overlay while open).
    fn show_wizard(&mut self, ctx: &Context) {
        // The wizard is taken out of `self` for the frame: its rendering
        // helpers borrow both `self` and the wizard mutably.
        let Some(mut wizard) = self.wizard.take() else {
            return;
        };
        let mut close = false;
        let mut finished = false;
        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                let title = match &wizard.mode {
                    Mode::New => "Neuen Sync-Job anlegen",
                    Mode::EditFolders { .. } => "Ordnerauswahl bearbeiten",
                };
                ui.heading(title);
                ui.add_space(8.0);
                self.show_wizard_account(ui, &mut wizard);
                if let Some(account_id) = wizard.account_id.clone() {
                    self.show_wizard_space(ui, &mut wizard, &account_id);
                    if let Some(space_id) = wizard.space_id.clone() {
                        if matches!(wizard.mode, Mode::New) {
                            self.show_wizard_local(ui, &mut wizard);
                        }
                        self.show_wizard_tree(ui, &mut wizard, &account_id, &space_id);
                        if wizard.can_finish() {
                            ui.add_space(8.0);
                            let mut finish = false;
                            ui.horizontal(|ui| {
                                let label = match &wizard.mode {
                                    Mode::New => "Job anlegen",
                                    Mode::EditFolders { .. } => "Auswahl speichern",
                                };
                                if ui.button(label).clicked() {
                                    finish = true;
                                }
                                if ui.button("Abbrechen").clicked() {
                                    close = true;
                                }
                            });
                            if finish {
                                finished = self.finish_wizard(&mut wizard);
                            }
                        } else if ui.button("Abbrechen").clicked() {
                            close = true;
                        }
                    } else if ui.button("Abbrechen").clicked() {
                        close = true;
                    }
                } else if ui.button("Abbrechen").clicked() {
                    close = true;
                }
                if let Some(status) = &wizard.status {
                    ui.label(egui::RichText::new(status).color(egui::Color32::from_rgb(180, 0, 0)));
                }
            });
        });
        if !close && !finished {
            self.wizard = Some(wizard);
        }
    }

    /// Wizard step 1: the account.
    fn show_wizard_account(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard) {
        if !matches!(wizard.mode, Mode::New) {
            return;
        }
        ui.label("1. Konto");
        let accounts: Vec<(String, Option<bool>)> = self
            .config
            .accounts
            .iter()
            .map(|account| {
                (
                    account.id.clone(),
                    self.accounts
                        .get(&account.id)
                        .and_then(|state| state.logged_in),
                )
            })
            .collect();
        let selected = wizard
            .account_id
            .clone()
            .or_else(|| accounts.first().map(|(id, _)| id.clone()));
        let mut choice = selected.clone();
        ComboBox::from_id_salt("wizard-account")
            .selected_text(choice.clone().unwrap_or_else(|| "—".into()))
            .show_ui(ui, |ui| {
                for (id, logged_in) in &accounts {
                    let label = match logged_in {
                        Some(true) => format!("{id} (angemeldet)"),
                        _ => format!("{id} (nicht angemeldet)"),
                    };
                    ui.selectable_value(&mut choice, Some(id.clone()), label);
                }
            });
        if choice != selected {
            wizard.account_id = choice;
            wizard.reset_space(None);
        }
        if accounts.is_empty() {
            ui.label(
                egui::RichText::new("Keine Konten — zuerst im Tab 'Konten & Server' anlegen.")
                    .weak(),
            );
        }
        ui.end_row();
    }

    /// Wizard step 2: the space (spaces list from the Graph API).
    fn show_wizard_space(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard, account_id: &str) {
        if !matches!(wizard.mode, Mode::New) {
            return;
        }
        ui.add_space(8.0);
        ui.label("2. Space");
        ui.horizontal(|ui| {
            if wizard.spaces.list.is_none() && !wizard.spaces.loading {
                if ui.button("Spaces laden").clicked() {
                    wizard.spaces.loading = true;
                    self.backend.send(UiCommand::ListSpaces {
                        account_id: account_id.to_owned(),
                    });
                }
            } else if wizard.spaces.loading {
                ui.label("Lade Spaces…");
            }
        });
        let Some(spaces) = wizard.spaces.list.clone() else {
            return;
        };
        let selected_id = wizard.space_id.clone();
        let mut selected = selected_id.clone();
        for space in &spaces {
            let quota = space
                .quota
                .as_ref()
                .and_then(|quota| {
                    quota
                        .used
                        .map(|used| format!(" — {used} / {}", quota.total.unwrap_or(0)))
                })
                .unwrap_or_default();
            let label = format!("{} [{}]{}", space.name, space.drive_type, quota);
            if ui
                .add(egui::Button::selectable(
                    selected.as_deref() == Some(space.id.as_str()),
                    label,
                ))
                .clicked()
            {
                selected = Some(space.id.clone());
            }
        }
        if selected != selected_id {
            let space = spaces
                .iter()
                .find(|space| Some(&space.id) == selected.as_ref());
            wizard.reset_space(space);
        }
    }

    /// Wizard step 3: the local directory.
    fn show_wizard_local(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard) {
        ui.add_space(8.0);
        ui.label("3. Lokales Verzeichnis");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut wizard.local_path);
            if ui.button("Durchsuchen…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    wizard.local_path = path.display().to_string();
                }
            }
        });
        if !wizard.local_path.trim().is_empty() {
            let path = PathBuf::from(wizard.local_path.trim());
            if !path.is_absolute() {
                ui.label(
                    egui::RichText::new("Der Pfad muss absolut sein.")
                        .color(egui::Color32::from_rgb(180, 0, 0)),
                );
            } else {
                ui.weak("Der Ordner wird angelegt, falls er fehlt.");
            }
        }
    }

    /// Wizard step 4: the remote folder tree with checkboxes.
    fn show_wizard_tree(
        &mut self,
        ui: &mut egui::Ui,
        wizard: &mut Wizard,
        account_id: &str,
        space_id: &str,
    ) {
        ui.add_space(8.0);
        ui.label("4. Ordnerauswahl (leer = alles synchronisieren)");
        let backend = self.backend.clone();
        // Root children load automatically once the space is chosen.
        wizard
            .tree
            .fetch(account_id, space_id, "", |command| backend.send(command));
        if wizard.tree.loading.contains_key("") {
            ui.label("Lade Struktur…");
        }
        let children = wizard.tree.children.get("").cloned().unwrap_or_default();
        for child in children {
            self.show_tree_node(ui, wizard, &child, 0, account_id, space_id);
        }
        ui.weak(format!(
            "Ausgewählt: {}",
            if wizard.tree.selected.is_empty() {
                "alles".to_string()
            } else {
                wizard
                    .tree
                    .selected
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }

    /// One folder row (checkbox + expand) with its subfolders.
    fn show_tree_node(
        &mut self,
        ui: &mut egui::Ui,
        wizard: &mut Wizard,
        resource: &oc_api::DavResource,
        depth: usize,
        account_id: &str,
        space_id: &str,
    ) {
        let backend = self.backend.clone();
        let path = resource.path.clone();
        let name = resource.name.clone();
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 16.0);
            let mut checked = wizard.tree.selected.contains(&path);
            if ui.checkbox(&mut checked, &name).changed() {
                let prefix = format!("{path}/");
                let descendants: Vec<String> = wizard
                    .tree
                    .selected
                    .iter()
                    .filter(|selected| selected.starts_with(&prefix))
                    .cloned()
                    .collect();
                for descendant in descendants {
                    wizard.tree.selected.remove(&descendant);
                }
                if checked {
                    wizard.tree.selected.insert(path.clone());
                } else {
                    wizard.tree.selected.remove(&path);
                }
            }
            if wizard.tree.descendant_selected(&path) {
                ui.weak("(teilweise)");
            }
            let expanded = wizard.tree.expanded.contains(&path);
            let loaded = wizard.tree.children.contains_key(&path);
            let toggle = if wizard.tree.loading.contains_key(&path) {
                "…"
            } else if expanded {
                "▾"
            } else {
                "▸"
            };
            if ui.small_button(toggle).clicked() {
                if !loaded {
                    wizard
                        .tree
                        .fetch(account_id, space_id, &path, |command| backend.send(command));
                } else if expanded {
                    wizard.tree.expanded.remove(&path);
                } else {
                    wizard.tree.expanded.insert(path.clone());
                }
            }
        });
        if wizard.tree.expanded.contains(&path) {
            let children = wizard.tree.children.get(&path).cloned().unwrap_or_default();
            for child in children {
                self.show_tree_node(ui, wizard, &child, depth + 1, account_id, space_id);
            }
        }
    }

    /// Completes the wizard: saves `jobs.json` and restarts the engine.
    /// Returns true when the wizard is done (the caller drops it).
    fn finish_wizard(&mut self, wizard: &mut Wizard) -> bool {
        let include = derive_include(&wizard.tree.selected);
        match wizard.mode.clone() {
            Mode::New => {
                let Some(account_id) = wizard.account_id.clone() else {
                    return false;
                };
                let Some(space_id) = wizard.space_id.clone() else {
                    return false;
                };
                let Some(local_path) = wizard.local_path_buf() else {
                    wizard.status = Some("Der lokale Pfad fehlt oder ist nicht absolut.".into());
                    return false;
                };
                if let Err(error) = std::fs::create_dir_all(&local_path) {
                    wizard.status = Some(format!(
                        "Verzeichnis '{}' konnte nicht angelegt werden: {error}",
                        local_path.display()
                    ));
                    return false;
                }
                let id = unique_job_id(&self.jobs, "job-1");
                self.jobs.jobs.push(SyncJob {
                    id: id.clone(),
                    account_id,
                    space_id,
                    space_name: wizard.space_name.clone(),
                    local_path,
                    enabled: true,
                    include,
                    exclude: Vec::new(),
                });
                if let Err(error) = oc_config::save_jobs(&self.config_dir, &self.jobs) {
                    self.notify(format!("Speichern fehlgeschlagen: {error}"));
                    return false;
                }
                self.notify(format!("Job '{id}' angelegt"));
            }
            Mode::EditFolders { job_id } => {
                let Some(job) = self.jobs.jobs.iter_mut().find(|job| job.id == job_id) else {
                    return false;
                };
                job.include = include;
                if let Err(error) = oc_config::save_jobs(&self.config_dir, &self.jobs) {
                    self.notify(format!("Speichern fehlgeschlagen: {error}"));
                    return false;
                }
                self.notify(format!("Ordnerauswahl von '{job_id}' gespeichert"));
            }
        }
        self.backend.send(UiCommand::ApplyJobs);
        self.engine_restarted();
        true
    }

    /// The removal confirmation dialog for a job.
    fn show_confirm_remove(&mut self, ctx: &Context) {
        let Some(job_id) = self.confirm_remove.clone() else {
            return;
        };
        egui::Window::new("Job entfernen")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Job '{job_id}' wirklich entfernen?"));
                ui.label("Die lokale Dateien bleiben erhalten; die Statusdatenbank wird behalten.");
                ui.horizontal(|ui| {
                    if ui.button("Entfernen").clicked() {
                        self.jobs.jobs.retain(|job| job.id != job_id);
                        self.job_states.remove(&job_id);
                        self.dbs.remove(&job_id);
                        self.confirm_remove = None;
                        self.persist_jobs();
                    }
                    if ui.button("Abbrechen").clicked() {
                        self.confirm_remove = None;
                    }
                });
            });
    }
}

impl App for SyncApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.poll_tray(ctx);
        if self.last_refresh.elapsed() > Duration::from_secs(1) {
            self.refresh_dbs();
            self.update_tray_status();
        }
        self.show_status_line(ctx);
        if self.wizard.is_some() {
            self.show_wizard(ctx);
        } else {
            self.show_tabs(ctx);
            CentralPanel::default().show(ctx, |ui| match self.tab {
                Tab::Accounts => self.show_accounts(ui),
                Tab::Jobs => self.show_jobs(ui),
                Tab::Conflicts => self.show_conflicts(ui),
            });
        }
        self.show_confirm_remove(ctx);
        // Backend events and tray menu events arrive between frames.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// Runs the GUI; blocks until the user quits via the tray or the window.
pub fn run(config_dir: Option<&Path>) -> Result<(), String> {
    let config_dir = match config_dir {
        Some(dir) => dir.to_path_buf(),
        None => oc_config::default_config_dir().map_err(|error| error.to_string())?,
    };
    let (backend, events) = crate::gui::backend::spawn(config_dir.clone())?;
    let app = SyncApp::new(config_dir, backend, events)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size([880.0, 560.0])
            .with_icon(std::sync::Arc::new(egui::IconData {
                rgba: crate::assets::icon::ICON_RGBA.to_vec(),
                width: crate::assets::icon::ICON_WIDTH,
                height: crate::assets::icon::ICON_HEIGHT,
            })),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "OpenCloud Sync",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
    .map_err(|error| error.to_string())
}

/// Whether a refresh token is stored for the account (keyring).
fn stored_login(account: &Account) -> Option<bool> {
    let auth = AuthClient::new(&account.server_url).ok()?;
    let manager = TokenManager::new(auth, KeyringStore::new(), &account.id);
    Some(manager.has_stored_login())
}

/// A fresh, unused job id (`job-1`, `job-2`, ...).
fn unique_job_id(jobs: &Jobs, base: &str) -> String {
    let existing: HashSet<&str> = jobs.jobs.iter().map(|job| job.id.as_str()).collect();
    let (stem, start) = match base.rsplit_once('-') {
        Some((stem, number)) => (stem, number.parse::<u64>().unwrap_or(1)),
        None => (base, 1),
    };
    let mut number = start;
    loop {
        let id = format!("{stem}-{number}");
        if !existing.contains(id.as_str()) {
            return id;
        }
        number += 1;
    }
}

/// Formats a byte count for humans (`1.2 GB`).
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a unix timestamp as `TT.MM.JJJJ SS:MM:SS` (UTC).
fn format_unix(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-from-days (Howard Hinnant); days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{day:02}.{month:02}.{year} {hour:02}:{minute:02}:{second:02}")
}

/// Describes one side of a conflict (`1.2 KB, 01.02.2026 10:00:00`).
fn describe_side(size: Option<i64>, mtime: Option<i64>) -> String {
    let size = size
        .map(|size| format_bytes(size.max(0) as u64))
        .unwrap_or_else(|| "?".into());
    let mtime = mtime.map(format_unix).unwrap_or_else(|| "—".into());
    format!("{size}, {mtime}")
}

/// Opens a directory in the platform's file manager.
fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(windows)]
    let program = "explorer";
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// German label of a conflict policy.
fn conflict_policy_label(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Ask => "Nachfragen",
        ConflictPolicy::KeepServer => "Server übernehmen",
        ConflictPolicy::KeepLocal => "Client behalten",
        ConflictPolicy::ConflictCopy => "Konfliktkopie + Server",
    }
}

/// German label of a log level.
fn log_level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "Fehler",
        LogLevel::Warn => "Warnungen",
        LogLevel::Info => "Info",
        LogLevel::Debug => "Debug",
        LogLevel::Trace => "Trace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_epoch_is_1970() {
        assert_eq!(format_unix(0), "01.01.1970 00:00:00");
    }

    #[test]
    fn format_unix_known_date() {
        // 2026-10-22 12:34:56 UTC.
        assert_eq!(format_unix(1_792_672_496), "22.10.2026 12:34:56");
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
    }

    #[test]
    fn unique_job_ids_skip_existing() {
        let mut jobs = Jobs::default();
        jobs.jobs.push(SyncJob {
            id: "job-1".into(),
            account_id: "a".into(),
            space_id: "s".into(),
            space_name: None,
            local_path: PathBuf::from("/tmp/x"),
            enabled: true,
            include: vec![],
            exclude: vec![],
        });
        assert_eq!(unique_job_id(&jobs, "job-1"), "job-2");
        jobs.jobs[0].id = "job-2".into();
        assert_eq!(unique_job_id(&jobs, "job-1"), "job-1");
    }

    #[test]
    fn describe_side_handles_unknown() {
        assert_eq!(describe_side(None, None), "?, —");
    }
}
