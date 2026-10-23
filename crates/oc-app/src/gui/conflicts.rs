//! The "Konflikte" tab: pending conflicts grouped by job, with both
//! sides' state and the resolution buttons (see `PLAN.md`, Phase 6/7).

use super::format::{describe_side, format_unix};
use super::*;

impl SyncApp {
    /// The "Konflikte" tab: all pending conflicts with both sides' state
    /// and the resolution buttons (see `PLAN.md`, Phase 6/7).
    pub(super) fn show_conflicts(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        let mut conflicts: Vec<(String, String, Vec<Conflict>)> = Vec::new();
        for job in self.jobs.jobs.clone() {
            let title = self.display_space_name(&job);
            let state = self.job_states.entry(job.id.clone()).or_default();
            conflicts.push((job.id, title, state.conflicts.clone()));
        }
        let total: usize = conflicts.iter().map(|(_, _, list)| list.len()).sum();
        if total == 0 {
            theme::empty_state(
                ui,
                "Keine offenen Konflikte",
                theme.success,
                "Bei Änderungen auf beiden Seiten erscheinen sie hier.",
                None,
            );
            return;
        }
        // Clicks are collected while rendering and sent afterwards;
        // conflicts already being resolved keep their buttons disabled.
        let mut requests: Vec<(String, Vec<i64>, ConflictResolution)> = Vec::new();
        let resolving = &self.resolving;
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
                            let open: Vec<i64> = list
                                .iter()
                                .map(|conflict| conflict.id)
                                .filter(|id| !resolving.contains(&(job_id.clone(), *id)))
                                .collect();
                            let any_open = !open.is_empty();
                            if ui
                                .add_enabled_ui(any_open, |ui| {
                                    theme::quiet(ui, "Alle: Client behalten")
                                })
                                .inner
                                .clicked()
                            {
                                requests.push((
                                    job_id.clone(),
                                    open.clone(),
                                    ConflictResolution::Local,
                                ));
                            }
                            if ui
                                .add_enabled_ui(any_open, |ui| {
                                    theme::quiet(ui, "Alle: Server übernehmen")
                                })
                                .inner
                                .clicked()
                            {
                                requests.push((job_id.clone(), open, ConflictResolution::Server));
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
                                        let busy =
                                            resolving.contains(&(job_id.clone(), conflict.id));
                                        if busy {
                                            ui.spinner();
                                        }
                                        if ui
                                            .add_enabled_ui(!busy, |ui| {
                                                theme::primary(ui, "Client behalten")
                                            })
                                            .inner
                                            .clicked()
                                        {
                                            requests.push((
                                                job_id.clone(),
                                                vec![conflict.id],
                                                ConflictResolution::Local,
                                            ));
                                        }
                                        if ui
                                            .add_enabled_ui(!busy, |ui| {
                                                theme::quiet(ui, "Server übernehmen")
                                            })
                                            .inner
                                            .clicked()
                                        {
                                            requests.push((
                                                job_id.clone(),
                                                vec![conflict.id],
                                                ConflictResolution::Server,
                                            ));
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
                                            // Recorded in milliseconds.
                                            conflict.local_mtime.map(|ms| ms.div_euclid(1000)),
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
        for (job_id, conflict_ids, resolution) in requests {
            self.resolve_conflicts(job_id, conflict_ids, resolution);
        }
    }

    /// Sends a resolution request and marks the conflicts as in progress
    /// until the backend answers.
    pub(super) fn resolve_conflicts(
        &mut self,
        job_id: String,
        conflict_ids: Vec<i64>,
        resolution: ConflictResolution,
    ) {
        for id in &conflict_ids {
            self.resolving.insert((job_id.clone(), *id));
        }
        self.backend.send(UiCommand::ResolveConflicts {
            job_id,
            conflict_ids,
            resolution,
        });
    }
}
