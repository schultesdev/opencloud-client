//! The "Konten" tab: account cards (login state, spaces, quota) and the
//! add-account form.

use super::format::initial_of;
use super::*;

impl SyncApp {
    /// The "Konten" tab: one card per account (login state, spaces and
    /// quota), followed by the collapsible add-account form.
    pub(super) fn show_accounts(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        let accounts = self.config.accounts.clone();
        if accounts.is_empty() && !self.show_new_account {
            if theme::empty_state(
                ui,
                "Noch kein Konto",
                theme.text,
                "Verbinde einen OpenCloud-Server, um Spaces synchronisieren zu können.",
                Some("Konto hinzufügen…"),
            ) {
                self.show_new_account = true;
            }
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
    pub(super) fn show_account_card(&mut self, ui: &mut egui::Ui, account: &Account) {
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
    pub(super) fn show_new_account_form(&mut self, ui: &mut egui::Ui) {
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

    /// Adds the account from the form to `config.json`; returns whether
    /// it was added (false: validation error shown in the form).
    pub(super) fn add_account(&mut self) -> bool {
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
        let account_id = account.id.clone();
        let mut config = self.config.clone();
        config.accounts.push(account);
        if let Err(error) = self.save_config(config) {
            self.new_account.error = Some(format!("Speichern fehlgeschlagen: {error}"));
            return false;
        }
        self.new_account.error = None;
        self.accounts.insert(
            account_id,
            AccountUi {
                logged_in: Some(false),
                ..AccountUi::default()
            },
        );
        self.new_account = NewAccountForm::default();
        true
    }
}
