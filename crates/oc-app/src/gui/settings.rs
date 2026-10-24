//! The "Einstellungen" tab: sync settings, autostart, log level.

use super::*;

impl SyncApp {
    /// The "Einstellungen" tab: sync behavior and program options.
    pub(super) fn show_settings(&mut self, ui: &mut egui::Ui) {
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
                                    .range(1..=oc_config::MAX_SYNC_INTERVAL_SECS)
                                    .suffix(" s"),
                            );
                            ui.end_row();
                            ui.label("Parallele Transfers");
                            ui.add(
                                DragValue::new(&mut self.settings_edit.max_parallel_transfers)
                                    .range(1..=oc_config::MAX_PARALLEL_TRANSFERS),
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
                            // The checkbox shows the OS state, which is
                            // what actually starts the app.
                            let mut autostart = self.autostart_active;
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
                            ui.label("Nach Updates suchen");
                            ui.checkbox(&mut self.settings_edit.check_updates, "")
                                .on_hover_text(
                                    "Beim Start gegen den Releases-Feed des Projekts prüfen \
                                     und im Info-Tab benachrichtigen; kein Auto-Update.",
                                );
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
    pub(super) fn toggle_autostart(&mut self, autostart: bool) {
        let result = if autostart {
            crate::gui::autostart::enable()
        } else {
            crate::gui::autostart::disable()
        };
        match result {
            Ok(()) => {
                // The OS entry changed right away, so the setting is saved
                // right away too ("Verwerfen" must not undo only half).
                self.settings_edit.autostart = autostart;
                self.autostart_active = autostart;
                let mut config = self.config.clone();
                config.settings.autostart = autostart;
                let _ = self.save_config(config);
                self.notify(if autostart {
                    "Autostart aktiviert"
                } else {
                    "Autostart deaktiviert"
                });
            }
            Err(error) => self.notify(format!("Autostart: {error}")),
        }
    }

    /// Saves the edited settings and restarts the engine (interval,
    /// parallel transfers, conflict policy).
    pub(super) fn save_settings(&mut self) {
        let mut config = self.config.clone();
        config.settings = self.settings_edit.clone();
        if self.save_config(config).is_ok() {
            crate::logging::set_level(self.config.settings.log_level);
            self.backend.send(UiCommand::ApplyJobs);
            self.engine_restarted();
        }
    }
}

/// German label of a conflict policy.
pub(super) fn conflict_policy_label(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Ask => "Nachfragen",
        ConflictPolicy::KeepServer => "Server übernehmen",
        ConflictPolicy::KeepLocal => "Client behalten",
        ConflictPolicy::ConflictCopy => "Konfliktkopie + Server",
    }
}

/// German label of a log level.
pub(super) fn log_level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "Fehler",
        LogLevel::Warn => "Warnungen",
        LogLevel::Info => "Info",
        LogLevel::Debug => "Debug",
        LogLevel::Trace => "Trace",
    }
}
