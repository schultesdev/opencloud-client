//! Rendering of the new-job wizard (account, space, local folder, remote
//! folder tree); its state lives in `wizard.rs`.

use super::format::format_bytes;
use super::*;

impl SyncApp {
    /// The wizard (full-area overlay while open).
    pub(super) fn show_wizard(&mut self, ctx: &Context) {
        // The wizard is taken out of `self` for the frame: its rendering
        // helpers borrow both `self` and the wizard mutably.
        let Some(mut wizard) = self.wizard.take() else {
            return;
        };
        let theme = theme::palette();
        let is_new = matches!(wizard.mode, Mode::New);
        // Editing skips account, space and local folder (steps 1–3).
        let step_offset = if is_new { 3 } else { 0 };
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
                            Mode::EditFolders { .. } => "Ordner & Ausschlüsse bearbeiten",
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
                                    theme::step(ui, step_offset + 1, "Ordnerauswahl");
                                    if let Mode::EditFolders { .. } = wizard.mode {
                                        ui.label(
                                            egui::RichText::new(
                                                "Abgewählte Ordner werden beim nächsten Sync \
                                                 lokal gelöscht — in der Cloud bleiben sie \
                                                 erhalten. Neue oder geänderte lokale Dateien \
                                                 darin werden nicht gelöscht.",
                                            )
                                            .small()
                                            .color(theme.text_dim),
                                        );
                                        ui.add_space(6.0);
                                    }
                                    // Only meaningful with a selection: an
                                    // empty one syncs everything anyway.
                                    if !wizard.tree.selected.is_empty() {
                                        ui.checkbox(
                                            &mut wizard.root_files,
                                            "Dateien direkt im Hauptordner synchronisieren",
                                        );
                                        ui.add_space(6.0);
                                    }
                                    self.show_wizard_tree(ui, &mut wizard, &account_id, &space_id);
                                });
                                ui.add_space(8.0);
                                theme::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    theme::step(ui, step_offset + 2, "Ausschlüsse");
                                    ui.label(
                                        egui::RichText::new(
                                            "Dateien und Ordner, die dieser Job nie \
                                             synchronisieren soll.",
                                        )
                                        .small()
                                        .color(theme.text_dim),
                                    );
                                    ui.add_space(6.0);
                                    pattern_editor(
                                        ui,
                                        &mut wizard.exclude,
                                        &mut wizard.exclude_input,
                                    );
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
    pub(super) fn show_wizard_account(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard) {
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
        // The displayed first account is also the actual preselection;
        // picking it in the dropdown must not be required.
        if let Some((id, _)) = accounts.first() {
            wizard.preselect_account(id);
        }
        let selected = wizard.account_id.clone();
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
    pub(super) fn show_wizard_space(
        &mut self,
        ui: &mut egui::Ui,
        wizard: &mut Wizard,
        account_id: &str,
    ) {
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
        if let Some(error) = wizard.spaces.error.clone() {
            ui.label(
                egui::RichText::new(format!("Spaces konnten nicht geladen werden: {error}"))
                    .small()
                    .color(theme.error),
            );
            if theme::quiet(ui, "Erneut versuchen").clicked() {
                wizard.spaces.error = None;
                if let Some(state) = self.accounts.get_mut(account_id) {
                    // Drop a cached failure so the request is sent again.
                    if matches!(state.spaces, Some(Err(_))) {
                        state.spaces = None;
                    }
                }
            }
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
    pub(super) fn show_wizard_local(&mut self, ui: &mut egui::Ui, wizard: &mut Wizard) {
        let theme = theme::palette();
        ui.horizontal(|ui| {
            // The browse button is allocated first (rightmost edge), so a
            // long path in the edit field clips instead of running under it.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::quiet(ui, "Durchsuchen…").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_folder()
                {
                    wizard.local_path = path.display().to_string();
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
    pub(super) fn show_wizard_tree(
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
        if let Some(error) = wizard.tree.failed.get("").cloned() {
            ui.label(
                egui::RichText::new(format!("Ordner konnten nicht geladen werden: {error}"))
                    .small()
                    .color(theme.error),
            );
            if theme::quiet(ui, "Erneut versuchen").clicked() {
                wizard.tree.retry("");
            }
        }
        let children = wizard.tree.children.get("").cloned().unwrap_or_default();
        // Large trees scroll inside the card instead of stretching the page.
        ScrollArea::vertical()
            .id_salt("wizard-tree")
            .max_height(320.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for child in children.iter() {
                    self.show_tree_node(ui, wizard, child, 0, account_id, space_id);
                }
            });
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
    pub(super) fn show_tree_node(
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
        // edge), so long folder names clip instead of running under it. The
        // `horizontal` wrapper keeps the row one line high; a bare
        // `with_layout` would take all remaining height.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let expanded = wizard.tree.expanded.contains(&path);
                let loaded = wizard.tree.children.contains_key(&path);
                let failed = wizard.tree.failed.contains_key(&path);
                let toggle = if wizard.tree.loading.contains_key(&path) {
                    "…"
                } else if failed {
                    "!"
                } else if expanded {
                    "▾"
                } else {
                    "▸"
                };
                let button = ui.small_button(toggle);
                let button = match wizard.tree.failed.get(&path) {
                    Some(error) => {
                        button.on_hover_text(format!("Fehler: {error} — klicken zum Wiederholen"))
                    }
                    None => button,
                };
                if button.clicked() {
                    if failed {
                        wizard.tree.retry(&path);
                    }
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
        });
        if wizard.tree.expanded.contains(&path) {
            let children = wizard.tree.children.get(&path).cloned().unwrap_or_default();
            for child in children.iter() {
                self.show_tree_node(ui, wizard, child, depth + 1, account_id, space_id);
            }
        }
    }

    /// Completes the wizard: saves `jobs.json` and restarts the engine.
    /// Returns true when the wizard is done (the caller drops it).
    pub(super) fn finish_wizard(&mut self, wizard: &mut Wizard) -> bool {
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
                let mut jobs = self.jobs.clone();
                jobs.jobs.push(SyncJob {
                    id: id.clone(),
                    account_id,
                    space_id,
                    space_name: wizard.space_name.clone(),
                    local_path,
                    enabled: true,
                    include,
                    exclude: wizard.exclude.clone(),
                    sync_root_files: wizard.root_files,
                });
                if let Err(error) = self.save_jobs(jobs, true) {
                    wizard.status = Some(error);
                    return false;
                }
                self.notify(format!("Job '{id}' angelegt"));
            }
            Mode::EditFolders { job_id } => {
                let mut jobs = self.jobs.clone();
                let Some(job) = jobs.jobs.iter_mut().find(|job| job.id == job_id) else {
                    return false;
                };
                job.include = include;
                job.exclude = wizard.exclude.clone();
                job.sync_root_files = wizard.root_files;
                if let Err(error) = self.save_jobs(jobs, true) {
                    wizard.status = Some(error);
                    return false;
                }
                self.notify(format!(
                    "Ordnerauswahl und Ausschlüsse von '{job_id}' gespeichert"
                ));
            }
        }
        true
    }
}
