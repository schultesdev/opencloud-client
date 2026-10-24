//! The "Info" tab: version, author, license, project links, and the
//! directories that hold configuration and logs.
//!
//! Program metadata comes from the package manifest (`[workspace.package]`
//! in the root `Cargo.toml`), so it has a single source of truth.

use super::*;

/// The program version.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The author(s), colon-separated as Cargo provides them.
const AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
/// The author's website.
const HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");
/// The SPDX license identifier.
const LICENSE: &str = env!("CARGO_PKG_LICENSE");
/// The full license text for [`LICENSE`].
const LICENSE_URL: &str = "https://www.apache.org/licenses/LICENSE-2.0";
/// The source repository; empty if `repository` is not set in the manifest,
/// in which case the tab shows a placeholder instead of links.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// The OpenCloud project this client talks to.
const OPENCLOUD_URL: &str = "https://opencloud.eu";

impl SyncApp {
    /// The "Info" tab: about the program and where it keeps its files.
    pub(super) fn show_info(&mut self, ui: &mut egui::Ui) {
        let theme = theme::palette();
        ScrollArea::vertical().show(ui, |ui| {
            theme::content_column(ui, |ui| {
                ui.add_space(4.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        if let Some(icon) = &self.icon {
                            ui.add(
                                egui::Image::from_texture(icon)
                                    .fit_to_exact_size(egui::Vec2::splat(64.0)),
                            );
                            ui.add_space(10.0);
                        }
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("OpenCloud Sync")
                                        .size(20.0)
                                        .strong()
                                        .color(theme.text),
                                );
                                ui.add_space(6.0);
                                theme::pill(ui, theme.accent, format!("Version {VERSION}"));
                            });
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(
                                    "Synchronisiert Ordner zwischen diesem Rechner und \
                                     OpenCloud-Spaces.",
                                )
                                .color(theme.text_dim),
                            );
                        });
                    });
                });
                ui.add_space(8.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::heading(ui, "Projekt");
                    ui.add_space(6.0);
                    Grid::new("info-project")
                        .num_columns(2)
                        .spacing(egui::Vec2::new(24.0, 10.0))
                        .show(ui, |ui| {
                            ui.label("Autor");
                            ui.label(AUTHORS.replace(':', ", "));
                            ui.end_row();
                            ui.label("Website");
                            ui.hyperlink_to(display_url(HOMEPAGE), HOMEPAGE)
                                .on_hover_text(HOMEPAGE);
                            ui.end_row();
                            ui.label("Lizenz");
                            ui.hyperlink_to(format!("{LICENSE} (Apache License 2.0)"), LICENSE_URL)
                                .on_hover_text(LICENSE_URL);
                            ui.end_row();
                            ui.label("Quellcode");
                            if REPOSITORY.is_empty() {
                                ui.label(
                                    egui::RichText::new("GitHub-Projekt (Link folgt)")
                                        .color(theme.text_dim),
                                );
                            } else {
                                ui.hyperlink_to("GitHub-Projekt", REPOSITORY)
                                    .on_hover_text(REPOSITORY);
                            }
                            ui.end_row();
                            ui.label("Fehler melden");
                            if REPOSITORY.is_empty() {
                                ui.label(
                                    egui::RichText::new("Issue-Tracker (Link folgt)")
                                        .color(theme.text_dim),
                                );
                            } else {
                                let issues = format!("{}/issues", REPOSITORY.trim_end_matches('/'));
                                ui.hyperlink_to("Issue auf GitHub anlegen", &issues)
                                    .on_hover_text(issues);
                            }
                            ui.end_row();
                            ui.label("OpenCloud");
                            ui.hyperlink_to(display_url(OPENCLOUD_URL), OPENCLOUD_URL)
                                .on_hover_text(OPENCLOUD_URL);
                            ui.end_row();
                        });
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Ein unabhängiger Sync-Client für OpenCloud, kein offizielles \
                             Produkt des OpenCloud-Projekts.",
                        )
                        .small()
                        .color(theme.text_dim),
                    );
                });
                ui.add_space(8.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::heading(ui, "Update");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if self.update_pending {
                            ui.label(
                                egui::RichText::new("Suche nach neuerer Version …")
                                    .color(theme.text_dim),
                            );
                        } else {
                            match self.update.as_ref() {
                                Some(Ok(crate::updates::UpdateCheck::Available {
                                    version,
                                    url,
                                })) => {
                                    theme::pill(ui, theme.accent, "Neue Version");
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(format!("{version} verfügbar"))
                                            .strong()
                                            .color(theme.text),
                                    );
                                    ui.add_space(6.0);
                                    ui.hyperlink_to("Release öffnen", url).on_hover_text(url);
                                }
                                Some(Ok(crate::updates::UpdateCheck::UpToDate)) => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Kein Update verfügbar — Version {VERSION} ist aktuell"
                                        ))
                                        .color(theme.text_dim),
                                    );
                                }
                                Some(Ok(crate::updates::UpdateCheck::NotConfigured)) => {
                                    ui.label(
                                        egui::RichText::new(
                                            "Update-Prüfung folgt mit der ersten Veröffentlichung",
                                        )
                                        .color(theme.text_dim),
                                    );
                                }
                                Some(Err(error)) => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Update-Prüfung fehlgeschlagen: {error}"
                                        ))
                                        .small()
                                        .color(theme.text_dim),
                                    );
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new(
                                            "Automatische Prüfung ist deaktiviert \
                                             (siehe Einstellungen)",
                                        )
                                        .color(theme.text_dim),
                                    );
                                }
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    !self.update_pending,
                                    egui::Button::new("Jetzt prüfen"),
                                )
                                .clicked()
                            {
                                self.update_pending = true;
                                self.backend.send(super::UiCommand::CheckUpdates);
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Es wird nur benachrichtigt — das Programm aktualisiert sich \
                             nicht selbst.",
                        )
                        .small()
                        .color(theme.text_dim),
                    );
                });
                ui.add_space(8.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::heading(ui, "System");
                    ui.add_space(6.0);
                    let log_dir = self.config_dir.join("logs");
                    Grid::new("info-system")
                        .num_columns(2)
                        .spacing(egui::Vec2::new(24.0, 10.0))
                        .show(ui, |ui| {
                            ui.label("Plattform");
                            ui.label(format!(
                                "{} ({}), {}-Build",
                                std::env::consts::OS,
                                std::env::consts::ARCH,
                                if cfg!(debug_assertions) {
                                    "Debug"
                                } else {
                                    "Release"
                                },
                            ));
                            ui.end_row();
                            ui.label("Konfiguration");
                            directory_row(ui, &self.config_dir);
                            ui.end_row();
                            ui.label("Protokolle");
                            directory_row(ui, &log_dir);
                            ui.end_row();
                        });
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Beim Melden eines Fehlers helfen die Protokolldateien \
                             aus dem Protokollordner bei der Analyse.",
                        )
                        .small()
                        .color(theme.text_dim),
                    );
                });
            });
        });
    }
}

/// A directory path with an "Öffnen" button that shows it in the file
/// manager; the button is disabled while the directory does not exist.
fn directory_row(ui: &mut egui::Ui, path: &Path) {
    ui.horizontal(|ui| {
        let exists = path.is_dir();
        let open = ui
            .add_enabled_ui(exists, |ui| theme::quiet(ui, "Öffnen"))
            .inner
            .on_disabled_hover_text("Ordner existiert noch nicht");
        if open.clicked() {
            open_in_file_manager(path);
        }
        theme::ellipsis(ui, path.display().to_string()).on_hover_text(path.display().to_string());
    });
}

/// A URL without its scheme, for display ("https://www.x.dev" -> "www.x.dev").
fn display_url(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_url_strips_scheme_and_trailing_slash() {
        assert_eq!(display_url("https://www.schultes.dev"), "www.schultes.dev");
        assert_eq!(display_url("http://opencloud.eu/"), "opencloud.eu");
        assert_eq!(display_url("opencloud.eu"), "opencloud.eu");
    }

    #[test]
    fn manifest_metadata_is_set() {
        assert!(!VERSION.is_empty());
        assert!(!AUTHORS.is_empty());
        assert!(HOMEPAGE.starts_with("https://"));
        assert_eq!(LICENSE, "Apache-2.0");
        assert_eq!(
            REPOSITORY,
            "https://github.com/schultesdev/opencloud-client"
        );
    }
}
