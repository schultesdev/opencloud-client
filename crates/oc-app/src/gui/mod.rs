//! The GUI (Phase 7): main window with the three areas "Konten & Server",
//! "Sync-Jobs", and "Konflikte", plus the new-job wizard and the tray icon.
//!
//! The engine is attached exclusively through the backend's event and
//! command channels (see [`backend`]); the GUI additionally reads job
//! statistics and pending conflicts from the jobs' SQLite databases
//! (its own connections, WAL mode allows concurrent readers).

mod autostart;
pub mod backend;
mod theme;
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
use oc_sync::RunProgress;
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
    /// App settings (interval, transfers, policy, autostart, logging).
    Settings,
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
    /// The server is unreachable; passes back off until it returns.
    offline: bool,
    /// What the last finished pass did.
    last_summary: Option<RunSummary>,
    /// Database statistics (refreshed once a second).
    stats: Option<Stats>,
    /// Pending conflicts (refreshed once a second).
    conflicts: Vec<Conflict>,
    /// Entries currently transferring (from the last stats refresh).
    transfers: Vec<String>,
    /// Live progress of the running pass (progress events).
    progress: Option<RunProgress>,
    /// Bytes and arrival time of the last progress event — the basis
    /// of the speed estimate.
    progress_at: Option<(u64, Instant)>,
    /// Smoothed transfer speed in bytes/s (exponential average).
    speed_bps: f64,
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
    /// Whether the add-account form is expanded.
    show_new_account: bool,
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
    /// Whether the theme has been applied to the context (once).
    styled: bool,
    /// The header icon (loaded from the embedded asset once).
    icon: Option<egui::TextureHandle>,
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
            show_new_account: false,
            tab: Tab::Jobs,
            wizard: None,
            confirm_remove: None,
            message: None,
            errors: Vec::new(),
            paused_all: false,
            tray: Tray::new(),
            autostart_active: crate::gui::autostart::is_enabled(),
            styled: false,
            icon: None,
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
                    Ok(()) => {
                        self.notify(format!("Konto '{account_id}' angemeldet"));
                        // Space names for display resolve via the Graph API.
                        if let Some(state) = self.accounts.get_mut(&account_id) {
                            state.spaces_pending = true;
                        }
                        self.backend.send(UiCommand::ListSpaces {
                            account_id: account_id.clone(),
                        });
                    }
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
                let state = self.job_states.entry(job_id).or_default();
                state.running = true;
                state.progress = None;
                state.progress_at = None;
                state.speed_bps = 0.0;
            }
            EngineEvent::Offline { job_id, retry_in } => {
                let state = self.job_states.entry(job_id.clone()).or_default();
                state.offline = true;
                state.running = false;
                state.progress = None;
                state.speed_bps = 0.0;
                self.notify(format!(
                    "Server nicht erreichbar ('{job_id}'): neuer Versuch in {} s",
                    retry_in.as_secs()
                ));
            }
            EngineEvent::Online { job_id } => {
                let state = self.job_states.entry(job_id.clone()).or_default();
                state.offline = false;
                self.notify(format!("Verbindung wiederhergestellt ('{job_id}')"));
            }
            EngineEvent::Progress { job_id, progress } => {
                let state = self.job_states.entry(job_id).or_default();
                let now = Instant::now();
                if let Some((previous, at)) = state.progress_at {
                    // Bytes can briefly regress on a retried transfer;
                    // only rising samples feed the speed estimate.
                    let gain = progress.bytes_done.saturating_sub(previous);
                    let elapsed = now.duration_since(at).as_secs_f64();
                    if gain > 0 && elapsed > 0.05 {
                        let rate = gain as f64 / elapsed;
                        state.speed_bps = if state.speed_bps == 0.0 {
                            rate
                        } else {
                            // Smoothed average: new samples shift it
                            // by 40%.
                            state.speed_bps * 0.6 + rate * 0.4
                        };
                    }
                }
                state.progress_at = Some((progress.bytes_done, now));
                state.progress = Some(progress);
            }
            EngineEvent::RunFinished { job_id, summary } => {
                let state = self.job_states.entry(job_id).or_default();
                state.running = false;
                state.progress = None;
                state.progress_at = None;
                state.speed_bps = 0.0;
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
            state.offline = false;
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
        let theme = theme::palette();
        let conflicts: u64 = self
            .job_states
            .values()
            .map(|state| state.conflicts.len() as u64)
            .sum();
        let message = match &self.message {
            Some((message, at)) if at.elapsed() < Duration::from_secs(10) => Some(message.clone()),
            _ => None,
        };
        TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::NONE
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::symmetric(18, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    let offline = self.job_states.values().any(|state| state.offline);
                    if self.paused_all {
                        theme::status(ui, theme.warning, "Alle Jobs pausiert");
                    } else {
                        if offline {
                            theme::status(ui, theme.error, "Server nicht erreichbar");
                        }
                        match &message {
                            Some(message) => {
                                ui.label(egui::RichText::new(message).small().color(theme.text));
                            }
                            None if !offline => theme::status(ui, theme.success, "Bereit"),
                            None => {}
                        }
                    }
                    ui.label(
                        egui::RichText::new(format!("{} Jobs", self.jobs.jobs.len()))
                            .small()
                            .color(theme.text_dim),
                    );
                    if conflicts > 0 {
                        ui.label(
                            egui::RichText::new(format!("{conflicts} Konflikte"))
                                .small()
                                .color(theme.error),
                        );
                    }
                    if !self.errors.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("{} Fehler", self.errors.len()))
                                .small()
                                .color(theme.text_dim),
                        );
                    }
                });
            });
    }

    /// The three main tabs; the conflicts tab is badged with their count.
    fn show_tabs(&mut self, ctx: &Context) {
        let theme = theme::palette();
        let conflicts: u64 = self
            .job_states
            .values()
            .map(|state| state.conflicts.len() as u64)
            .sum();
        TopBottomPanel::top("header")
            .frame(
                egui::Frame::NONE
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(icon) = &self.icon {
                        ui.add(
                            egui::Image::from_texture(icon)
                                .fit_to_exact_size(egui::Vec2::splat(28.0)),
                        );
                    }
                    // Narrow windows drop the title so the tabs and the
                    // right-hand buttons never overlap.
                    if ui.available_width() > 900.0 {
                        ui.label(
                            egui::RichText::new("OpenCloud Sync")
                                .size(17.0)
                                .strong()
                                .color(theme.text),
                        );
                        ui.add_space(24.0);
                    } else {
                        ui.add_space(8.0);
                    }
                    let mut tab = self.tab;
                    for (candidate, label) in [
                        (Tab::Accounts, "Konten"),
                        (Tab::Jobs, "Sync-Jobs"),
                        (Tab::Conflicts, "Konflikte"),
                        (Tab::Settings, "Einstellungen"),
                    ] {
                        if tab_button(ui, candidate == self.tab, label) {
                            tab = candidate;
                        }
                        if candidate == Tab::Conflicts && conflicts > 0 {
                            theme::badge(ui, conflicts, theme.error);
                        }
                        ui.add_space(6.0);
                    }
                    self.tab = tab;
                    // The button block is the last element: a
                    // right-to-left layout claims the remaining width,
                    // so anything after it would land outside the
                    // panel. Placed last, its buttons pin to the right
                    // edge.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::primary(ui, "+ Neuer Job").clicked() && self.wizard.is_none() {
                            self.wizard = Some(Wizard::new());
                        }
                        if self.paused_all {
                            if theme::quiet(ui, "Alle fortsetzen").clicked() {
                                self.paused_all = false;
                                if let Some(tray) = &mut self.tray {
                                    tray.set_paused(false);
                                }
                                self.backend.send(UiCommand::ResumeAll);
                            }
                        } else if theme::quiet(ui, "Alle pausieren").clicked() {
                            self.paused_all = true;
                            if let Some(tray) = &mut self.tray {
                                tray.set_paused(true);
                            }
                            self.backend.send(UiCommand::PauseAll);
                        }
                    });
                });
            });
    }

    /// The "Konten" tab: one card per account (login state, spaces and
    /// quota), followed by the collapsible add-account form.
    fn show_accounts(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        let accounts = self.config.accounts.clone();
        if accounts.is_empty() && !self.show_new_account {
            ui.vertical_centered(|ui| {
                ui.add_space(70.0);
                ui.label(
                    egui::RichText::new("Noch kein Konto")
                        .size(20.0)
                        .strong()
                        .color(theme.text),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Verbinde einen OpenCloud-Server, um Spaces synchronisieren zu können.",
                    )
                    .color(theme.text_dim),
                );
                ui.add_space(14.0);
                if theme::primary(ui, "Konto hinzufügen…").clicked() {
                    self.show_new_account = true;
                }
            });
            return;
        }
        ScrollArea::vertical().show(ui, |ui| {
            theme::content_column(ui, |ui| {
                ui.add_space(4.0);
                for account in &accounts {
                    self.show_account_card(ui, account);
                    ui.add_space(8.0);
                }
                if self.show_new_account {
                    self.show_new_account_form(ui);
                } else if theme::quiet(ui, "+ Konto hinzufügen").clicked() {
                    self.show_new_account = true;
                }
                ui.add_space(4.0);
            });
        });
    }

    /// One account card: avatar, name and server on the left, login state
    /// and actions on the right; the loaded spaces with quota bars below.
    fn show_account_card(&mut self, ui: &mut egui::Ui, account: &Account) {
        let theme = theme::palette();
        let state = self.accounts.entry(account.id.clone()).or_default();
        let logged_in = state.logged_in == Some(true);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::avatar(ui, &initial_of(&account.username), theme.accent, 34.0);
                // Right side first (fixed width), the text block takes
                // what is left and ellipsizes.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if state.spaces_pending {
                        ui.spinner();
                    } else if logged_in
                        && theme::icon_button(ui, "\u{21bb}", "Spaces & Quota aktualisieren")
                            .clicked()
                    {
                        state.spaces_pending = true;
                        self.backend.send(UiCommand::ListSpaces {
                            account_id: account.id.clone(),
                        });
                    }
                    if state.login_pending {
                        ui.label(
                            egui::RichText::new("Im Browser anmelden…")
                                .small()
                                .color(theme.accent),
                        );
                    } else if state.logout_pending {
                        ui.label(
                            egui::RichText::new("Abmeldung…")
                                .small()
                                .color(theme.text_dim),
                        );
                    } else if logged_in {
                        if theme::danger(ui, "Abmelden").clicked() {
                            state.logout_pending = true;
                            self.backend.send(UiCommand::Logout {
                                account_id: account.id.clone(),
                            });
                        }
                    } else if theme::primary(ui, "Anmelden").clicked() {
                        state.login_pending = true;
                        self.backend.send(UiCommand::Login {
                            account_id: account.id.clone(),
                        });
                    }
                    if logged_in {
                        theme::pill(ui, theme.success, "Angemeldet");
                    } else {
                        theme::pill(ui, theme.text_dim, "Nicht angemeldet");
                    }
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        theme::ellipsis(
                            ui,
                            egui::RichText::new(&account.id)
                                .strong()
                                .size(15.0)
                                .color(theme.text),
                        );
                        theme::ellipsis(
                            ui,
                            egui::RichText::new(format!(
                                "{} · {}",
                                account.server_url, account.username
                            ))
                            .small()
                            .color(theme.text_dim),
                        );
                    });
                });
            });
            let Some(spaces) = &state.spaces else {
                return;
            };
            theme::divider(ui);
            theme::section_label(ui, "Spaces");
            ui.add_space(2.0);
            match spaces {
                Ok(spaces) if spaces.is_empty() => {
                    ui.label(
                        egui::RichText::new("Keine Spaces.")
                            .small()
                            .color(theme.text_dim),
                    );
                }
                Ok(spaces) => {
                    Grid::new(("spaces", &account.id))
                        .num_columns(3)
                        .spacing(egui::Vec2::new(14.0, 6.0))
                        .show(ui, |ui| {
                            for space in spaces {
                                space_row(ui, space);
                                ui.end_row();
                            }
                        });
                }
                Err(error) => {
                    ui.label(egui::RichText::new(error).small().color(theme.error));
                }
            }
        });
    }

    /// The expanded add-account form (card with the three fields).
    fn show_new_account_form(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::heading(ui, "Konto hinzufügen");
            ui.add_space(6.0);
            Grid::new("new-account")
                .num_columns(2)
                .spacing(egui::Vec2::new(14.0, 8.0))
                .show(ui, |ui| {
                    ui.label("Konto-ID");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_account.id)
                            .hint_text("z. B. arbeit")
                            .desired_width(320.0),
                    );
                    ui.end_row();
                    ui.label("Server-URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_account.server_url)
                            .hint_text("https://cloud.example.org")
                            .desired_width(320.0),
                    );
                    ui.end_row();
                    ui.label("Benutzername");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_account.username)
                            .desired_width(320.0),
                    );
                    ui.end_row();
                });
            if let Some(error) = &self.new_account.error {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(error).small().color(theme.error));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, "Hinzufügen").clicked() && self.add_account() {
                    self.show_new_account = false;
                }
                if theme::quiet(ui, "Abbrechen").clicked() {
                    self.new_account = NewAccountForm::default();
                    self.show_new_account = false;
                }
            });
        });
    }

    /// The "Einstellungen" tab: sync behavior and program options.
    fn show_settings(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        ScrollArea::vertical().show(ui, |ui| {
            theme::content_column(ui, |ui| {
                ui.add_space(4.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::heading(ui, "Synchronisation");
                    ui.add_space(6.0);
                    Grid::new("settings-sync")
                        .num_columns(2)
                        .spacing(egui::Vec2::new(24.0, 10.0))
                        .show(ui, |ui| {
                            ui.label("Sync-Intervall");
                            ui.add(
                                DragValue::new(&mut self.settings_edit.sync_interval_secs)
                                    .range(1..=86400)
                                    .suffix(" s"),
                            );
                            ui.end_row();
                            ui.label("Parallele Transfers");
                            ui.add(
                                DragValue::new(&mut self.settings_edit.max_parallel_transfers)
                                    .range(1..=32),
                            );
                            ui.end_row();
                            ui.label("Bei Konflikten");
                            ComboBox::from_id_salt("conflict-policy")
                                .selected_text(conflict_policy_label(
                                    self.settings_edit.conflict_policy,
                                ))
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
                        });
                });
                ui.add_space(8.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::heading(ui, "Programm");
                    ui.add_space(6.0);
                    Grid::new("settings-app")
                        .num_columns(2)
                        .spacing(egui::Vec2::new(24.0, 10.0))
                        .show(ui, |ui| {
                            ui.label("Bei Anmeldung starten");
                            let mut autostart = self.settings_edit.autostart;
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut autostart, "").changed() {
                                    self.toggle_autostart(autostart);
                                }
                                ui.label(
                                    egui::RichText::new(if self.autostart_active {
                                        "im Betriebssystem aktiv"
                                    } else {
                                        "im Betriebssystem inaktiv"
                                    })
                                    .small()
                                    .color(theme.text_dim),
                                );
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
                        });
                });
                ui.add_space(12.0);
                let dirty = self.settings_edit != self.config.settings;
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(dirty, |ui| {
                        if theme::primary(ui, "Einstellungen speichern").clicked() {
                            self.save_settings();
                        }
                    });
                    if dirty {
                        if theme::quiet(ui, "Verwerfen").clicked() {
                            self.settings_edit = self.config.settings.clone();
                        }
                        ui.label(
                            egui::RichText::new("Ungespeicherte Änderungen")
                                .small()
                                .color(theme.warning),
                        );
                    }
                });
                ui.add_space(4.0);
            });
        });
    }

    /// Enables/disables the OS autostart entry and reports the outcome.
    fn toggle_autostart(&mut self, autostart: bool) {
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

    /// Adds the account from the form to `config.json`; returns whether
    /// it was added (false: validation error shown in the form).
    fn add_account(&mut self) -> bool {
        let account = Account {
            id: self.new_account.id.trim().to_owned(),
            server_url: self.new_account.server_url.trim().to_owned(),
            username: self.new_account.username.trim().to_owned(),
        };
        if let Err(error) = account.validate() {
            self.new_account.error = Some(error.to_string());
            return false;
        }
        if self.config.accounts.iter().any(|a| a.id == account.id) {
            self.new_account.error = Some(format!("Konto '{}' existiert bereits", account.id));
            return false;
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
        true
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
        let theme = theme::palette();
        if self.jobs.jobs.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(70.0);
                ui.label(
                    egui::RichText::new("Noch keine Sync-Jobs")
                        .size(20.0)
                        .strong()
                        .color(theme.text),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Lege den ersten Job an, um einen Space mit einem lokalen \
                         Ordner zu synchronisieren.",
                    )
                    .color(theme.text_dim),
                );
                ui.add_space(14.0);
                if theme::primary(ui, "Neuen Job anlegen…").clicked() {
                    self.wizard = Some(Wizard::new());
                }
            });
            return;
        }
        ScrollArea::vertical().show(ui, |ui| {
            theme::content_column(ui, |ui| {
                ui.add_space(4.0);
                for job in self.jobs.jobs.clone() {
                    self.show_job_row(ui, &job);
                    ui.add_space(8.0);
                }
                self.show_error_log(ui);
                ui.add_space(4.0);
            });
        });
    }

    /// The recent failures of all jobs (newest first).
    fn show_error_log(&mut self, ui: &mut egui::Ui) {
        if self.errors.is_empty() {
            return;
        }
        let theme = theme::palette();
        let count = self.errors.len();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.collapsing(
                egui::RichText::new(format!("Fehlerliste ({count})"))
                    .small()
                    .color(theme.text_dim),
                |ui| {
                    for line in self.errors.iter().rev().take(100) {
                        ui.label(egui::RichText::new(line).small().color(theme.text_dim));
                    }
                },
            );
        });
    }

    /// The display name of the job's space: the stored name, or the name
    /// resolved from the account's spaces list (Graph API). A freshly
    /// resolved name is written back into `jobs.json` once, so the id
    /// never surfaces again (also benefits CLI/headless).
    fn display_space_name(&mut self, job: &SyncJob) -> String {
        if let Some(name) = &job.space_name {
            return name.clone();
        }
        let Some(name) = self.lookup_space_name(&job.account_id, &job.space_id) else {
            return job.space_id.clone();
        };
        if let Some(stored) = self.jobs.jobs.iter_mut().find(|stored| stored.id == job.id) {
            stored.space_name = Some(name.clone());
            // Informational only — the engine does not read it, so no
            // restart is triggered.
            if let Err(error) = oc_config::save_jobs(&self.config_dir, &self.jobs) {
                self.notify(format!("Speichern fehlgeschlagen: {error}"));
            }
        }
        name
    }

    /// Looks a space name up in the account's loaded spaces list.
    fn lookup_space_name(&self, account_id: &str, space_id: &str) -> Option<String> {
        self.accounts
            .get(account_id)?
            .spaces
            .as_ref()?
            .as_ref()
            .ok()?
            .iter()
            .find(|space| space.id == space_id)
            .map(|space| space.name.clone())
    }

    /// Loads the spaces of every logged-in account (Graph API) — the
    /// source of the space display names and the quota view.
    fn bootstrap_space_names(&mut self) {
        let logged_in: Vec<String> = self
            .accounts
            .iter()
            .filter(|(_, state)| state.logged_in == Some(true))
            .map(|(account_id, _)| account_id.clone())
            .collect();
        for account_id in logged_in {
            if let Some(state) = self.accounts.get_mut(&account_id) {
                state.spaces_pending = true;
            }
            self.backend.send(UiCommand::ListSpaces {
                account_id: account_id.clone(),
            });
        }
    }

    /// One job card: identity and the main actions in the header (status
    /// pill, sync now, pause, overflow menu), details and live progress
    /// below.
    fn show_job_row(&mut self, ui: &mut egui::Ui, job: &SyncJob) {
        let theme = theme::palette();
        let space = self.display_space_name(job);
        let state = self.job_states.entry(job.id.clone()).or_default();
        let (status_color, status_label) = if state.running {
            (theme.accent, "Synchronisiert…".to_string())
        } else if state.offline {
            (theme.error, "Offline".to_string())
        } else if state.paused {
            (theme.warning, "Pausiert".to_string())
        } else if let Some(stats) = state.stats {
            if stats.conflicts > 0 {
                (theme.error, format!("{} Konflikt(e)", stats.conflicts))
            } else if stats.errors > 0 {
                (theme.error, format!("{} Fehler", stats.errors))
            } else if stats.transfers > 0 {
                (theme.text_dim, format!("{} ausstehend", stats.transfers))
            } else {
                (theme.success, "Aktuell".to_string())
            }
        } else {
            (theme.text_dim, "Noch nie synchronisiert".to_string())
        };
        let mut details = Vec::new();
        if let Some(stats) = state.stats {
            if let Some(last_sync) = stats.last_sync {
                details.push(format!("Letzter Sync {}", format_unix(last_sync)));
            }
            details.push(format!("{} Dateien · {} Ordner", stats.files, stats.dirs));
        }
        if !job.include.is_empty() {
            details.push(format!("{} ausgewählte Ordner", job.include.len()));
        }
        let mut open_folder = false;
        let mut edit_folders = false;
        let mut remove = false;

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::avatar(ui, &initial_of(&space), theme.accent, 38.0);
                // Right side first (fixed width), the text block takes
                // what is left and ellipsizes.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button(
                        egui::RichText::new("\u{2630}").size(15.0).color(theme.text),
                        |ui| {
                            if ui.button("Ordner im Dateimanager öffnen").clicked() {
                                open_folder = true;
                                ui.close();
                            }
                            if ui.button("Ordnerauswahl bearbeiten…").clicked() {
                                edit_folders = true;
                                ui.close();
                            }
                            ui.separator();
                            if ui
                                .button(egui::RichText::new("Job entfernen…").color(theme.error))
                                .clicked()
                            {
                                remove = true;
                                ui.close();
                            }
                        },
                    )
                    .response
                    .on_hover_text("Weitere Aktionen");
                    if state.paused {
                        if theme::icon_button(ui, "\u{25b6}", "Fortsetzen").clicked() {
                            state.paused = false;
                            self.backend.send(UiCommand::JobCmd {
                                job_id: job.id.clone(),
                                command: JobCommand::Resume,
                            });
                        }
                    } else if theme::icon_button(ui, "\u{23f8}", "Pausieren").clicked() {
                        state.paused = true;
                        self.backend.send(UiCommand::JobCmd {
                            job_id: job.id.clone(),
                            command: JobCommand::Pause,
                        });
                    }
                    let sync_now = ui.add_enabled_ui(!state.running, |ui| {
                        theme::primary(ui, "Jetzt synchronisieren")
                    });
                    if sync_now.inner.clicked() {
                        state.paused = false;
                        self.backend.send(UiCommand::JobCmd {
                            job_id: job.id.clone(),
                            command: JobCommand::SyncNow,
                        });
                    }
                    theme::pill(ui, status_color, status_label);
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        theme::ellipsis(
                            ui,
                            egui::RichText::new(&space)
                                .strong()
                                .size(15.0)
                                .color(theme.text),
                        );
                        theme::ellipsis(
                            ui,
                            egui::RichText::new(job.local_path.display().to_string())
                                .small()
                                .color(theme.text_dim),
                        )
                        .on_hover_text(job.local_path.display().to_string());
                    });
                });
            });

            if state.running {
                if let Some(progress) = state.progress.filter(|p| p.transfers_total > 0) {
                    let remaining = progress.bytes_total.saturating_sub(progress.bytes_done);
                    let speed = state.speed_bps;
                    let label = format!(
                        "{} von {} Dateien · {} / {}",
                        progress.transfers_done,
                        progress.transfers_total,
                        format_bytes(progress.bytes_done),
                        format_bytes(progress.bytes_total),
                    );
                    let label = if speed >= 1.0 {
                        let eta = Duration::from_secs((remaining as f64 / speed) as u64);
                        format!(
                            "{label} · {}/s · noch {}",
                            format_bytes(speed as u64),
                            format_eta(eta)
                        )
                    } else {
                        label
                    };
                    ui.add_space(8.0);
                    ui.add(
                        egui::ProgressBar::new(progress.fraction())
                            .desired_width(f32::INFINITY)
                            .desired_height(18.0)
                            .corner_radius(egui::CornerRadius::same(5))
                            .text(egui::RichText::new(label).small().color(theme.text)),
                    );
                }
                if !state.transfers.is_empty() {
                    theme::ellipsis(
                        ui,
                        egui::RichText::new(format!("Aktuell: {}", state.transfers.join(", ")))
                            .small()
                            .color(theme.accent),
                    );
                }
            }

            if !details.is_empty() || state.last_summary.is_some() {
                theme::divider(ui);
                if !details.is_empty() {
                    theme::ellipsis(
                        ui,
                        egui::RichText::new(details.join("  ·  "))
                            .small()
                            .color(theme.text_dim),
                    );
                }
                if let Some(summary) = &state.last_summary {
                    theme::ellipsis(
                        ui,
                        egui::RichText::new(format!(
                            "Letzter Lauf: {summary}{}",
                            if summary.remote_scan_skipped {
                                " (Remote-Scan übersprungen)"
                            } else {
                                ""
                            }
                        ))
                        .small()
                        .color(theme.text_dim),
                    );
                }
            }
        });

        if open_folder {
            open_in_file_manager(&job.local_path);
        }
        if edit_folders {
            self.wizard = Some(Wizard::edit_folders(job));
        }
        if remove {
            self.confirm_remove = Some(job.id.clone());
        }
    }

    /// The "Konflikte" tab: all pending conflicts with both sides' state
    /// and the resolution buttons (see `PLAN.md`, Phase 6/7).
    fn show_conflicts(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        let mut conflicts: Vec<(String, String, Vec<Conflict>)> = Vec::new();
        for job in self.jobs.jobs.clone() {
            let title = self.display_space_name(&job);
            let state = self.job_states.entry(job.id.clone()).or_default();
            conflicts.push((job.id, title, state.conflicts.clone()));
        }
        let total: usize = conflicts.iter().map(|(_, _, list)| list.len()).sum();
        if total == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(70.0);
                ui.label(
                    egui::RichText::new("Keine offenen Konflikte")
                        .size(20.0)
                        .strong()
                        .color(theme.success),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Bei Änderungen auf beiden Seiten erscheinen sie hier.")
                        .color(theme.text_dim),
                );
            });
            return;
        }
        ScrollArea::vertical().show(ui, |ui| {
            theme::content_column(ui, |ui| {
                ui.add_space(4.0);
                for (job_id, title, list) in conflicts {
                    if list.is_empty() {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        theme::heading(ui, &title);
                        theme::badge(ui, list.len() as u64, theme.error);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::quiet(ui, "Alle: Client behalten").clicked() {
                                for conflict in &list {
                                    self.backend.send(UiCommand::ResolveConflict {
                                        job_id: job_id.clone(),
                                        conflict_id: conflict.id,
                                        resolution: ConflictResolution::Local,
                                    });
                                }
                            }
                            if theme::quiet(ui, "Alle: Server übernehmen").clicked() {
                                for conflict in &list {
                                    self.backend.send(UiCommand::ResolveConflict {
                                        job_id: job_id.clone(),
                                        conflict_id: conflict.id,
                                        resolution: ConflictResolution::Server,
                                    });
                                }
                            }
                        });
                    });
                    ui.add_space(4.0);
                    for conflict in list {
                        theme::card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                // Buttons first (right edge), the path block
                                // takes the remaining width and ellipsizes.
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if theme::primary(ui, "Client behalten").clicked() {
                                            self.backend.send(UiCommand::ResolveConflict {
                                                job_id: job_id.clone(),
                                                conflict_id: conflict.id,
                                                resolution: ConflictResolution::Local,
                                            });
                                        }
                                        if theme::quiet(ui, "Server übernehmen").clicked() {
                                            self.backend.send(UiCommand::ResolveConflict {
                                                job_id: job_id.clone(),
                                                conflict_id: conflict.id,
                                                resolution: ConflictResolution::Server,
                                            });
                                        }
                                        ui.with_layout(
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                theme::ellipsis(
                                                    ui,
                                                    egui::RichText::new(&conflict.path)
                                                        .strong()
                                                        .size(15.0)
                                                        .color(theme.text),
                                                )
                                                .on_hover_text(&conflict.path);
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "Erkannt {}",
                                                        format_unix(conflict.detected_at)
                                                    ))
                                                    .small()
                                                    .color(theme.text_dim),
                                                );
                                            },
                                        );
                                    },
                                );
                            });
                            theme::divider(ui);
                            Grid::new(("conflict", &job_id, conflict.id))
                                .num_columns(2)
                                .spacing(egui::Vec2::new(16.0, 4.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new("Lokal")
                                            .strong()
                                            .small()
                                            .color(theme.accent),
                                    );
                                    ui.label(
                                        egui::RichText::new(describe_side(
                                            conflict.local_size,
                                            conflict.local_mtime,
                                        ))
                                        .small()
                                        .color(theme.text),
                                    );
                                    ui.end_row();
                                    ui.label(
                                        egui::RichText::new("Server")
                                            .strong()
                                            .small()
                                            .color(theme.success),
                                    );
                                    ui.label(
                                        egui::RichText::new(describe_side(
                                            conflict.remote_size,
                                            conflict.remote_mtime,
                                        ))
                                        .small()
                                        .color(theme.text),
                                    );
                                    ui.end_row();
                                });
                        });
                        ui.add_space(6.0);
                    }
                    ui.add_space(10.0);
                }
            });
        });
    }

    /// The wizard (full-area overlay while open).
    fn show_wizard(&mut self, ctx: &Context) {
        // The wizard is taken out of `self` for the frame: its rendering
        // helpers borrow both `self` and the wizard mutably.
        let Some(mut wizard) = self.wizard.take() else {
            return;
        };
        let theme = theme::palette();
        let is_new = matches!(wizard.mode, Mode::New);
        let mut close = false;
        let mut finished = false;
        CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ctx, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    theme::content_column(ui, |ui| {
                        ui.add_space(4.0);
                        let title = match &wizard.mode {
                            Mode::New => "Neuen Sync-Job anlegen",
                            Mode::EditFolders { .. } => "Ordnerauswahl bearbeiten",
                        };
                        ui.label(
                            egui::RichText::new(title)
                                .size(20.0)
                                .strong()
                                .color(theme.text),
                        );
                        ui.add_space(10.0);
                        if is_new {
                            theme::card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                theme::step(ui, 1, "Konto");
                                self.show_wizard_account(ui, &mut wizard);
                            });
                        }
                        if let Some(account_id) = wizard.account_id.clone() {
                            if is_new {
                                ui.add_space(8.0);
                                theme::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    theme::step(ui, 2, "Space");
                                    self.show_wizard_space(ui, &mut wizard, &account_id);
                                });
                            }
                            if let Some(space_id) = wizard.space_id.clone() {
                                if is_new {
                                    ui.add_space(8.0);
                                    theme::card().show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        theme::step(ui, 3, "Lokales Verzeichnis");
                                        self.show_wizard_local(ui, &mut wizard);
                                    });
                                }
                                ui.add_space(8.0);
                                theme::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    theme::step(ui, 4, "Ordnerauswahl");
                                    self.show_wizard_tree(ui, &mut wizard, &account_id, &space_id);
                                });
                                ui.add_space(12.0);
                                let mut finish = false;
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if wizard.can_finish() {
                                            let label = match &wizard.mode {
                                                Mode::New => "Job anlegen",
                                                Mode::EditFolders { .. } => "Auswahl speichern",
                                            };
                                            if theme::primary(ui, label).clicked() {
                                                finish = true;
                                            }
                                        } else {
                                            ui.label(
                                                egui::RichText::new(
                                                    "Konto, Space und lokales Verzeichnis \
                                                 auswählen…",
                                                )
                                                .small()
                                                .color(theme.text_dim),
                                            );
                                        }
                                        if theme::quiet(ui, "Abbrechen").clicked() {
                                            close = true;
                                        }
                                    },
                                );
                                if finish {
                                    finished = self.finish_wizard(&mut wizard);
                                }
                            } else if theme::quiet(ui, "Abbrechen").clicked() {
                                close = true;
                            }
                        } else if theme::quiet(ui, "Abbrechen").clicked() {
                            close = true;
                        }
                        if let Some(status) = &wizard.status {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(status).small().color(theme.error));
                        }
                    });
                });
            });
        if !close && !finished {
            self.wizard = Some(wizard);
        }
    }

    /// Wizard step 1: the account.
    fn show_wizard_account(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard) {
        let theme = theme::palette();
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
                egui::RichText::new("Keine Konten — zuerst im Tab „Konten“ anlegen.")
                    .small()
                    .color(theme.warning),
            );
        }
    }

    /// Wizard step 2: the space. The list comes from the account's
    /// already loaded spaces when available, otherwise it is requested
    /// automatically (Graph API) — no extra click needed.
    fn show_wizard_space(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard, account_id: &str) {
        let theme = theme::palette();
        let account = self.accounts.get(account_id);
        let logged_in = account.and_then(|state| state.logged_in) == Some(true);
        if !logged_in {
            ui.label(
                egui::RichText::new(
                    "Das Konto ist nicht angemeldet — zuerst im Tab „Konten“ anmelden.",
                )
                .small()
                .color(theme.warning),
            );
            return;
        }
        if wizard.spaces.list.is_none() && !wizard.spaces.loading {
            match account.and_then(|state| state.spaces.as_ref()) {
                Some(Ok(list)) => wizard.spaces.list = Some(list.clone()),
                _ => {
                    wizard.spaces.loading = true;
                    self.backend.send(UiCommand::ListSpaces {
                        account_id: account_id.to_owned(),
                    });
                }
            }
        }
        if wizard.spaces.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new("Lade Spaces…")
                        .small()
                        .color(theme.text_dim),
                );
            });
            return;
        }
        let Some(spaces) = wizard.spaces.list.clone() else {
            return;
        };
        if spaces.is_empty() {
            ui.label(
                egui::RichText::new("Keine Spaces verfügbar.")
                    .small()
                    .color(theme.text_dim),
            );
            return;
        }
        let selected_id = wizard.space_id.clone();
        let mut selected = selected_id.clone();
        Grid::new("wizard-spaces")
            .num_columns(3)
            .spacing(egui::Vec2::new(14.0, 6.0))
            .show(ui, |ui| {
                for space in &spaces {
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    if ui
                        .add(egui::Button::selectable(
                            is_selected,
                            egui::RichText::new(&space.name).strong(),
                        ))
                        .clicked()
                    {
                        selected = Some(space.id.clone());
                    }
                    theme::chip(theme.card_stroke).show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(&space.drive_type)
                                .small()
                                .color(theme.text_dim),
                        );
                    });
                    match &space.quota {
                        Some(quota) => {
                            theme::quota_bar(ui, quota.used.unwrap_or(0), quota.total, format_bytes)
                        }
                        None => {
                            ui.label("");
                        }
                    }
                    ui.end_row();
                }
            });
        if selected != selected_id {
            let space = spaces
                .iter()
                .find(|space| Some(&space.id) == selected.as_ref());
            wizard.reset_space(space);
        }
    }

    /// Wizard step 3: the local directory.
    fn show_wizard_local(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard) {
        let theme = theme::palette();
        ui.horizontal(|ui| {
            // The browse button is allocated first (rightmost edge), so a
            // long path in the edit field clips instead of running under it.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::quiet(ui, "Durchsuchen…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        wizard.local_path = path.display().to_string();
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut wizard.local_path).desired_width(f32::INFINITY),
                );
            });
        });
        if !wizard.local_path.trim().is_empty() {
            let path = PathBuf::from(wizard.local_path.trim());
            if !path.is_absolute() {
                ui.label(
                    egui::RichText::new("Der Pfad muss absolut sein.")
                        .small()
                        .color(theme.error),
                );
            } else {
                ui.label(
                    egui::RichText::new("Der Ordner wird angelegt, falls er fehlt.")
                        .small()
                        .color(theme.text_dim),
                );
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
        let theme = theme::palette();
        let backend = self.backend.clone();
        // Root children load automatically once the space is chosen.
        wizard.tree.fetch(account_id, space_id, "", "", |command| {
            backend.send(command)
        });
        if wizard.tree.loading.contains_key("") {
            ui.label(
                egui::RichText::new("Lade Struktur…")
                    .small()
                    .color(theme.text_dim),
            );
        }
        let children = wizard.tree.children.get("").cloned().unwrap_or_default();
        for child in children {
            self.show_tree_node(ui, wizard, &child, 0, account_id, space_id);
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!(
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
            ))
            .small()
            .color(theme.text_dim),
        );
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
        let server_path = resource.server_path.clone();
        let name = resource.name.clone();
        // Right-to-left: the expand toggle is allocated first (rightmost
        // edge), so long folder names clip instead of running under it.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                        .fetch(account_id, space_id, &path, &server_path, |command| {
                            backend.send(command)
                        });
                } else if expanded {
                    wizard.tree.expanded.remove(&path);
                } else {
                    wizard.tree.expanded.insert(path.clone());
                }
            }
            if wizard.tree.descendant_selected(&path) {
                ui.weak("(teilweise)");
            }
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
            ui.add_space(depth as f32 * 16.0);
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
                // Ids whose status database still exists are not reused:
                // its baseline belongs to the removed job.
                let config_dir = self.config_dir.clone();
                let id = unique_job_id(&self.jobs, "job-1", |id| {
                    db_path(&config_dir, id).is_ok_and(|path| path.exists())
                });
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
        let theme = theme::palette();
        let Some(job_id) = self.confirm_remove.clone() else {
            return;
        };
        egui::Window::new("Job entfernen")
            .collapsible(false)
            .resizable(false)
            .min_width(460.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!("Job '{job_id}' wirklich entfernen?"))
                        .strong()
                        .size(15.0)
                        .color(theme.text),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Die lokalen Dateien bleiben erhalten; die Statusdatenbank wird behalten.",
                    )
                    .small()
                    .color(theme.text_dim),
                );
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::danger(ui, "Entfernen").clicked() {
                        self.jobs.jobs.retain(|job| job.id != job_id);
                        self.job_states.remove(&job_id);
                        self.dbs.remove(&job_id);
                        self.confirm_remove = None;
                        self.persist_jobs();
                    }
                    if theme::quiet(ui, "Abbrechen").clicked() {
                        self.confirm_remove = None;
                    }
                });
            });
    }
}

impl App for SyncApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            theme::apply(ctx);
            self.styled = true;
        }
        if self.icon.is_none() {
            self.icon = Some(ctx.load_texture(
                "app-icon",
                egui::ColorImage::from_rgba_unmultiplied(
                    [
                        crate::assets::icon::ICON_WIDTH as usize,
                        crate::assets::icon::ICON_HEIGHT as usize,
                    ],
                    crate::assets::icon::ICON_RGBA,
                ),
                egui::TextureOptions::LINEAR,
            ));
        }
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
            let theme = theme::palette();
            CentralPanel::default()
                .frame(
                    egui::Frame::NONE
                        .fill(theme.panel)
                        .inner_margin(egui::Margin::symmetric(18, 12)),
                )
                .show(ctx, |ui| match self.tab {
                    Tab::Accounts => self.show_accounts(ui),
                    Tab::Jobs => self.show_jobs(ui),
                    Tab::Conflicts => self.show_conflicts(ui),
                    Tab::Settings => self.show_settings(ui),
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
    let mut app = SyncApp::new(config_dir, backend, events)?;
    app.bootstrap_space_names();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
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

/// A fresh, unused job id (`job-1`, `job-2`, ...); ids already in
/// `jobs` or reported by `taken` (e.g. a leftover database file) are
/// skipped.
fn unique_job_id(jobs: &Jobs, base: &str, taken: impl Fn(&str) -> bool) -> String {
    let existing: HashSet<&str> = jobs.jobs.iter().map(|job| job.id.as_str()).collect();
    let (stem, start) = match base.rsplit_once('-') {
        Some((stem, number)) => (stem, number.parse::<u64>().unwrap_or(1)),
        None => (base, 1),
    };
    let mut number = start;
    loop {
        let id = format!("{stem}-{number}");
        if !existing.contains(id.as_str()) && !taken(&id) {
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

/// Formats a remaining time for humans (`45 s`, `2 min 05 s`, `1 h 12 min`).
fn format_eta(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
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

/// One header tab; the selected one is emphasized and underlined in the
/// accent color.
fn tab_button(ui: &mut egui::Ui, selected: bool, label: &str) -> bool {
    let theme = theme::palette();
    let text = egui::RichText::new(label).color(if selected { theme.text } else { theme.text_dim });
    let text = if selected { text.strong() } else { text };
    let response = ui.add(
        egui::Button::new(text)
            .fill(egui::Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(8)),
    );
    if selected {
        let rect = response.rect;
        let underline = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 5.0, rect.bottom() - 3.0),
            egui::pos2(rect.right() - 5.0, rect.bottom() - 1.0),
        );
        ui.painter()
            .rect_filled(underline, egui::CornerRadius::same(1), theme.accent);
    }
    response.clicked()
}

/// One space as three grid cells: name, type chip, quota bar (empty
/// cell when the server reports no quota, e.g. for the virtual shares
/// space).
fn space_row(ui: &mut egui::Ui, space: &Space) {
    let theme = theme::palette();
    ui.label(egui::RichText::new(&space.name).strong().color(theme.text));
    theme::chip(theme.card_stroke).show(ui, |ui| {
        ui.label(
            egui::RichText::new(&space.drive_type)
                .small()
                .color(theme.text_dim),
        );
    });
    match &space.quota {
        Some(quota) => theme::quota_bar(ui, quota.used.unwrap_or(0), quota.total, format_bytes),
        None => {
            ui.label("");
        }
    }
}

/// The first upper-case character of a name (avatar initials).
fn initial_of(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_default()
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
    fn format_eta_scales() {
        assert_eq!(format_eta(Duration::from_secs(45)), "45 s");
        assert_eq!(format_eta(Duration::from_secs(125)), "2 min 05 s");
        assert_eq!(format_eta(Duration::from_secs(4320)), "1 h 12 min");
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
        assert_eq!(unique_job_id(&jobs, "job-1", |_| false), "job-2");
        jobs.jobs[0].id = "job-2".into();
        assert_eq!(unique_job_id(&jobs, "job-1", |_| false), "job-1");
        // A leftover database of a removed job blocks its id.
        assert_eq!(unique_job_id(&jobs, "job-1", |id| id == "job-1"), "job-3");
    }

    #[test]
    fn describe_side_handles_unknown() {
        assert_eq!(describe_side(None, None), "?, —");
    }
}
