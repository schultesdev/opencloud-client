//! The GUI (Phase 7): main window with the three areas "Konten & Server",
//! "Sync-Jobs", and "Konflikte", plus the new-job wizard and the tray icon.
//!
//! The engine is attached exclusively through the backend's event and
//! command channels (see [`backend`]); job statistics and pending
//! conflicts arrive as snapshots the backend reads from the jobs' SQLite
//! databases off the UI thread.
//!
//! Layout: this module holds the app state, event handling, tray, status
//! line, and the frame loop; each tab renders in its own module
//! (`accounts`, `jobs`, `conflicts`, `settings`), the wizard in
//! `wizard_view` (state in `wizard`), formatting helpers in `format`, and
//! the visual building blocks in `theme`.

mod accounts;
mod autostart;
pub mod backend;
mod conflicts;
mod format;
mod jobs;
mod settings;
mod theme;
mod tray;
mod wizard;
mod wizard_view;

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
use oc_db::{Conflict, ConflictResolution, Stats, db_path};
use oc_sync::EngineEvent;
use oc_sync::JobCommand;
use oc_sync::RunProgress;
use oc_sync::RunSummary;

use crate::gui::backend::BackendEvent;
use crate::gui::backend::BackendHandle;
use crate::gui::backend::JobSnapshot;
use crate::gui::backend::UiCommand;
use crate::gui::format::format_bytes;
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
    /// Set by the tray's "Beenden": the next close request is honored
    /// instead of hiding the window.
    quitting: bool,
    /// Conflicts with a resolution request in flight (job id, conflict
    /// id); their buttons stay disabled until the backend answers.
    resolving: HashSet<(String, i64)>,
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
            new_account: NewAccountForm::default(),
            show_new_account: false,
            tab: Tab::Jobs,
            wizard: None,
            confirm_remove: None,
            message: None,
            errors: Vec::new(),
            paused_all: false,
            // Created in eframe's app creator, once the event loop runs.
            tray: None,
            quitting: false,
            resolving: HashSet::new(),
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
            } => {
                self.resolving.remove(&(job_id.clone(), conflict_id));
                match result {
                    Ok(true) => {
                        self.notify(format!("Konflikt #{conflict_id} in '{job_id}' gelöst"));
                        self.request_snapshots();
                    }
                    Ok(false) => {
                        self.notify(format!(
                            "Konflikt #{conflict_id} in '{job_id}' bleibt offen: \
                             der Server hat sich zwischenzeitlich geändert"
                        ));
                        self.request_snapshots();
                    }
                    Err(error) => self.notify(format!("Konfliktlösung fehlgeschlagen: {error}")),
                }
            }
            BackendEvent::Message { message } => self.notify(message),
            BackendEvent::Snapshots(snapshots) => self.apply_snapshots(snapshots),
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
                self.request_snapshots();
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
        self.show_paused_all(false);
        for state in self.job_states.values_mut() {
            state.offline = false;
        }
    }

    /// Pauses or resumes every job (header button, tray menu).
    fn set_paused_all(&mut self, paused: bool) {
        self.show_paused_all(paused);
        if paused {
            self.backend.send(UiCommand::PauseAll);
            self.notify("Alle Jobs pausiert");
        } else {
            self.backend.send(UiCommand::ResumeAll);
            self.notify("Alle Jobs fortgesetzt");
        }
    }

    /// Reflects a global pause state in the header, the tray menu, and
    /// every job card.
    fn show_paused_all(&mut self, paused: bool) {
        self.paused_all = paused;
        if let Some(tray) = &self.tray {
            tray.set_paused(paused);
        }
        for state in self.job_states.values_mut() {
            state.paused = paused;
        }
    }

    /// Asks the backend for fresh database snapshots of all jobs (stats,
    /// pending conflicts, transfers). The databases are read on the
    /// backend's blocking threads, never in the frame.
    fn request_snapshots(&mut self) {
        self.last_refresh = Instant::now();
        let job_ids = self.jobs.jobs.iter().map(|job| job.id.clone()).collect();
        self.backend.send(UiCommand::RefreshSnapshots { job_ids });
    }

    /// Applies database snapshots from the backend.
    fn apply_snapshots(&mut self, snapshots: Vec<JobSnapshot>) {
        for snapshot in snapshots {
            if !self.jobs.jobs.iter().any(|job| job.id == snapshot.job_id) {
                continue;
            }
            if let Some(error) = &snapshot.error {
                self.notify(format!("Status-Datenbank '{}': {error}", snapshot.job_id));
            }
            let state = self.job_states.entry(snapshot.job_id).or_default();
            state.stats = snapshot.stats;
            state.conflicts = snapshot.conflicts;
            state.transfers = snapshot.transfers;
        }
    }

    /// Handles tray menu actions and the close-to-tray behavior.
    fn poll_tray(&mut self, ctx: &Context) {
        // With a tray, the close button hides the window and quitting
        // goes through the tray menu. Without one (platform refused it),
        // closing quits — a hidden window could never be brought back.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.tray.is_some() {
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
                TrayAction::TogglePause => self.set_paused_all(!self.paused_all),
                TrayAction::Quit => {
                    self.quitting = true;
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
                        let label = if self.paused_all {
                            "Alle fortsetzen"
                        } else {
                            "Alle pausieren"
                        };
                        if theme::quiet(ui, label).clicked() {
                            self.set_paused_all(!self.paused_all);
                        }
                    });
                });
            });
    }

    /// Saves `config` and, only on success, makes it the GUI's copy — a
    /// failed save must not leave the GUI showing state that is not on
    /// disk (and that a later save would write after all).
    fn save_config(&mut self, config: Config) -> Result<(), String> {
        match oc_config::save_config(&self.config_dir, &config) {
            Ok(()) => {
                self.config = config;
                self.notify("Konfiguration gespeichert");
                Ok(())
            }
            Err(error) => {
                self.notify(format!("Speichern fehlgeschlagen: {error}"));
                Err(error.to_string())
            }
        }
    }

    /// Saves `jobs` and, only on success, makes it the GUI's copy (see
    /// [`SyncApp::save_config`]). With `restart`, the engine picks the
    /// change up.
    fn save_jobs(&mut self, jobs: Jobs, restart: bool) -> Result<(), String> {
        match oc_config::save_jobs(&self.config_dir, &jobs) {
            Ok(()) => {
                self.jobs = jobs;
                if restart {
                    self.backend.send(UiCommand::ApplyJobs);
                    self.engine_restarted();
                }
                Ok(())
            }
            Err(error) => {
                self.notify(format!("Speichern fehlgeschlagen: {error}"));
                Err(error.to_string())
            }
        }
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
        if self.last_refresh.elapsed() >= Duration::from_secs(1) {
            self.request_snapshots();
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
        // Backend events wake the GUI themselves. The idle frame drives
        // the once-a-second snapshot refresh and the tray polling; while a
        // pass runs, speed and ETA update more often.
        let running = self.job_states.values().any(|state| state.running);
        ctx.request_repaint_after(if running {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(1)
        });
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
        Box::new(move |cc| {
            app.backend.set_repaint_context(cc.egui_ctx.clone());
            // tray-icon wants the event loop running before the icon is
            // created (macOS); this closure runs inside it.
            app.tray = Tray::new();
            Ok(Box::new(app))
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
