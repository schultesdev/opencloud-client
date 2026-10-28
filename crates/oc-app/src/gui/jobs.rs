//! The "Sync-Jobs" tab: job cards with status, progress, and actions,
//! the error log, space names, and the removal dialog.

use super::format::{format_bytes, format_eta, format_speed, format_unix, initial_of};
use super::*;

impl SyncApp {
    /// The "Sync-Jobs" tab.
    pub(super) fn show_jobs(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        if self.jobs.jobs.is_empty() {
            if theme::empty_state(
                ui,
                "Noch keine Sync-Jobs",
                theme.text,
                "Lege den ersten Job an, um einen Space mit einem lokalen Ordner zu \
                 synchronisieren.",
                Some("Neuen Job anlegen…"),
            ) {
                self.wizard = Some(Wizard::new());
            }
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
    pub(super) fn show_error_log(&mut self, ui: &mut egui::Ui) {
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
    pub(super) fn display_space_name(&mut self, job: &SyncJob) -> String {
        if let Some(name) = &job.space_name {
            return name.clone();
        }
        let Some(name) = self.lookup_space_name(&job.account_id, &job.space_id) else {
            return job.space_id.clone();
        };
        let mut jobs = self.jobs.clone();
        if let Some(stored) = jobs.jobs.iter_mut().find(|stored| stored.id == job.id) {
            stored.space_name = Some(name.clone());
            // Informational only — the engine does not read it, so no
            // restart is triggered. A failed save is retried next frame
            // at the latest; the name is shown either way.
            let _ = self.save_jobs(jobs, false);
        }
        name
    }

    /// Looks a space name up in the account's loaded spaces list.
    pub(super) fn lookup_space_name(&self, account_id: &str, space_id: &str) -> Option<String> {
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
    pub(super) fn bootstrap_space_names(&mut self) {
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
    pub(super) fn show_job_row(&mut self, ui: &mut egui::Ui, job: &SyncJob) {
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
            details.push(if job.sync_root_files {
                "Hauptordner-Dateien an".to_owned()
            } else {
                "Hauptordner-Dateien aus".to_owned()
            });
        }
        if !job.exclude.is_empty() {
            details.push(format!("{} Ausschluss-Muster", job.exclude.len()));
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
                            if ui.button("Ordner & Ausschlüsse bearbeiten…").clicked() {
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
                        // One pass now; a paused job stays paused.
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

            if state.running
                && let Some(progress) = state.progress.as_ref().filter(|p| p.transfers_total > 0)
            {
                let remaining = progress.bytes_total.saturating_sub(progress.bytes_done);
                let mut label = format!(
                    "{} von {} Dateien · {} / {}",
                    progress.transfers_done,
                    progress.transfers_total,
                    format_bytes(progress.bytes_done),
                    format_bytes(progress.bytes_total),
                );
                if let Some(eta) = state.speed.eta(remaining) {
                    label.push_str(&format!(
                        " · {} · noch {}",
                        format_speed(state.speed.bps()),
                        format_eta(eta)
                    ));
                }
                ui.add_space(8.0);
                ui.add(
                    egui::ProgressBar::new(progress.fraction())
                        .desired_width(f32::INFINITY)
                        .desired_height(18.0)
                        .corner_radius(egui::CornerRadius::same(5))
                        .text(egui::RichText::new(label).small().color(theme.text)),
                );
                ui.add_space(4.0);
                for transfer in &progress.active {
                    let speed = state
                        .file_speeds
                        .get(&transfer.path)
                        .map(SpeedEstimate::bps)
                        .unwrap_or(0.0);
                    show_transfer_row(ui, transfer, speed);
                }
                let pending = progress.pending();
                if pending > 0 {
                    ui.label(
                        egui::RichText::new(format!("noch {pending} wartend"))
                            .small()
                            .color(theme.text_dim),
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

    /// The removal confirmation dialog for a job.
    pub(super) fn show_confirm_remove(&mut self, ctx: &Context) {
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
                        "Die lokalen Dateien bleiben erhalten; die Statusdatenbank des Jobs \
                         wird gelöscht.",
                    )
                    .small()
                    .color(theme.text_dim),
                );
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::danger(ui, "Entfernen").clicked() {
                        let mut jobs = self.jobs.clone();
                        jobs.jobs.retain(|job| job.id != job_id);
                        if self.save_jobs(jobs, true).is_ok() {
                            self.job_states.remove(&job_id);
                            self.notify(format!("Job '{job_id}' entfernt"));
                        }
                        self.confirm_remove = None;
                    }
                    if theme::quiet(ui, "Abbrechen").clicked() {
                        self.confirm_remove = None;
                    }
                });
            });
    }
}

/// One running transfer under the job's progress bar: direction arrow,
/// ellipsized path, mini bar, and `done / total · speed`. The fixed parts
/// are laid out from the right so only the path shrinks.
fn show_transfer_row(ui: &mut egui::Ui, transfer: &oc_sync::TransferProgress, speed: f64) {
    let theme = theme::palette();
    let up = transfer.direction == TransferDirection::Upload;
    let color = if up { theme.accent } else { theme.success };
    // The planned size comes from the scan; the file may have grown since,
    // so the fraction is clamped and an unknown size (0) shows no bar fill.
    let fraction = if transfer.bytes_total > 0 {
        (transfer.bytes_done as f64 / transfer.bytes_total as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let mut text = if transfer.bytes_total > 0 {
        format!(
            "{} / {}",
            format_bytes(transfer.bytes_done),
            format_bytes(transfer.bytes_total)
        )
    } else {
        format_bytes(transfer.bytes_done)
    };
    if speed >= 1.0 {
        text.push_str(&format!(" · {}", format_speed(speed)));
    }
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Fixed width keeps the bar from drifting as the numbers grow.
            ui.add_sized(
                [190.0, ui.spacing().interact_size.y],
                egui::Label::new(egui::RichText::new(text).small().color(theme.text_dim))
                    .truncate(),
            );
            theme::mini_bar(ui, fraction, color);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                theme::arrow(ui, up, color);
                theme::ellipsis(
                    ui,
                    egui::RichText::new(&transfer.path)
                        .small()
                        .color(theme.text),
                )
                .on_hover_text(&transfer.path);
            });
        });
    });
}
