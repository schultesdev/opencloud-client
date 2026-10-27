//! Tray icon (Phase 7): status tooltip (running/errors/conflicts), menu
//! with show/pause/quit, and close-to-tray. Events are polled from the
//! global `tray-icon` channels each frame. On Linux the icon runs on its
//! own gtk thread (tray-icon requires a gtk main loop there).

use tray_icon::TrayIcon;
use tray_icon::TrayIconBuilder;
use tray_icon::TrayIconEvent;
use tray_icon::menu::Menu;
use tray_icon::menu::MenuEvent;
use tray_icon::menu::MenuItem;
use tray_icon::menu::PredefinedMenuItem;

use crate::assets::icon::ICON_HEIGHT;
use crate::assets::icon::ICON_RGBA;
use crate::assets::icon::ICON_WIDTH;

/// What a tray menu interaction asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Show (and focus) the main window.
    Show,
    /// Toggle pause of all jobs.
    TogglePause,
    /// Quit the app.
    Quit,
}

/// What the tray tooltip summarizes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrayStatus {
    /// At least one job is running a sync pass.
    pub running: bool,
    /// Files finished and planned by the running passes (`None` when no
    /// pass is moving files).
    pub files: Option<(usize, usize)>,
    /// Remaining time of the running passes, coarsely formatted (`None`
    /// while the speed is unknown).
    pub eta: Option<String>,
    /// Recorded errors across all jobs.
    pub errors: u64,
    /// Pending conflicts across all jobs.
    pub conflicts: u64,
}

impl TrayStatus {
    /// The tooltip text, e.g. `OpenCloud Sync — Synchronisierung läuft ·
    /// 12/40 Dateien · noch 2 min — 1 Fehler`.
    fn tooltip(&self) -> String {
        let mut tooltip = String::from("OpenCloud Sync");
        if self.running {
            tooltip.push_str(" — Synchronisierung läuft");
            if let Some((done, total)) = self.files {
                tooltip.push_str(&format!(" · {done}/{total} Dateien"));
            }
            if let Some(eta) = &self.eta {
                tooltip.push_str(&format!(" · noch {eta}"));
            }
        }
        if self.errors > 0 {
            tooltip.push_str(&format!(" — {} Fehler", self.errors));
        }
        if self.conflicts > 0 {
            tooltip.push_str(&format!(" — {} Konflikte", self.conflicts));
        }
        tooltip
    }
}

/// A change for the tray icon.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Update {
    /// Switch the pause menu label.
    Paused(bool),
    /// Replace the status tooltip.
    Tooltip(String),
}

/// The platform tray objects; they must stay on the thread that created
/// them (they are not `Send`).
struct Parts {
    icon: TrayIcon,
    toggle: MenuItem,
}

/// Builds icon and menu; `None` when the platform rejected the icon.
fn build() -> Option<Parts> {
    let show = MenuItem::with_id(SHOW_ID, "Fenster zeigen", true, None);
    let toggle = MenuItem::with_id(TOGGLE_ID, "Alle pausieren", true, None);
    let quit = MenuItem::with_id(QUIT_ID, "Beenden", true, None);
    let separator = PredefinedMenuItem::separator();
    let menu = Menu::new();
    let _ = menu.append(&show);
    let _ = menu.append(&toggle);
    let _ = menu.append(&separator);
    let _ = menu.append(&quit);
    let icon = tray_icon::Icon::from_rgba(ICON_RGBA.to_vec(), ICON_WIDTH, ICON_HEIGHT).ok()?;
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("OpenCloud Sync")
        .with_icon(icon)
        .build()
        .ok()?;
    Some(Parts { icon, toggle })
}

/// Applies one update to the tray objects.
fn apply(parts: &Parts, update: Update) {
    match update {
        Update::Paused(paused) => parts.toggle.set_text(if paused {
            "Alle fortsetzen"
        } else {
            "Alle pausieren"
        }),
        Update::Tooltip(tooltip) => {
            let _ = parts.icon.set_tooltip(Some(&tooltip));
        }
    }
}

/// Menu item ids; menu events are matched by id on every platform.
const SHOW_ID: &str = "show";
/// See [`SHOW_ID`].
const TOGGLE_ID: &str = "toggle-pause";
/// See [`SHOW_ID`].
const QUIT_ID: &str = "quit";

/// The tray icon handle; keep it alive or the icon disappears.
///
/// On macOS and Windows the icon lives on the GUI thread (create it once
/// the event loop runs, i.e. in eframe's app creator). On Linux it needs a
/// running gtk main loop on its own thread: the icon is built there and
/// receives updates through a channel.
pub struct Tray {
    #[cfg(not(target_os = "linux"))]
    parts: Parts,
    #[cfg(target_os = "linux")]
    updates: std::sync::mpsc::Sender<Update>,
    /// The last tooltip sent (the status is refreshed every second;
    /// unchanged values are not re-applied).
    tooltip: Option<String>,
}

impl Tray {
    /// Builds the tray icon; `None` when the platform rejected it (the
    /// app keeps running without tray).
    pub fn new() -> Option<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            build().map(|parts| Self {
                parts,
                tooltip: None,
            })
        }
        #[cfg(target_os = "linux")]
        {
            linux::spawn().map(|updates| Self {
                updates,
                tooltip: None,
            })
        }
    }

    /// Hands an update to the tray objects.
    fn send(&self, update: Update) {
        #[cfg(not(target_os = "linux"))]
        apply(&self.parts, update);
        #[cfg(target_os = "linux")]
        let _ = self.updates.send(update);
    }

    /// Updates the pause label ("Alle pausieren" / "Alle fortsetzen").
    pub fn set_paused(&self, paused: bool) {
        self.send(Update::Paused(paused));
    }

    /// Sets the status tooltip (see [`TrayStatus::tooltip`]); an
    /// unchanged text is not re-sent.
    pub fn set_status(&mut self, status: &TrayStatus) {
        let tooltip = status.tooltip();
        if self.tooltip.as_deref() != Some(tooltip.as_str()) {
            self.tooltip = Some(tooltip.clone());
            self.send(Update::Tooltip(tooltip));
        }
    }

    /// Drains pending menu and tray events into actions.
    pub fn poll(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                SHOW_ID => actions.push(TrayAction::Show),
                TOGGLE_ID => actions.push(TrayAction::TogglePause),
                QUIT_ID => actions.push(TrayAction::Quit),
                _ => {}
            }
        }
        // Left-click shows the window (Windows/Linux tray behavior).
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            else {
                continue;
            };
            actions.push(TrayAction::Show);
        }
        actions
    }
}

/// The gtk thread hosting the tray icon on Linux.
#[cfg(target_os = "linux")]
mod linux {
    use std::sync::mpsc;
    use std::time::Duration;

    use gtk::glib;
    use gtk::glib::LogLevels;

    use super::Update;

    /// The critical libayatana-appindicator emits when the panel's
    /// StatusNotifierWatcher is not on the bus (yet) and the library
    /// queries its internal GtkStatusIcon fallback — always `NULL` on
    /// modern GTK, since GtkStatusIcon is a stub there. Harmless (the
    /// icon appears once the watcher registers), but it spams stderr
    /// on every start.
    const APPINDICATOR_CRITICAL: &str =
        "gtk_widget_get_scale_factor: assertion 'GTK_IS_WIDGET (widget)' failed";

    /// Installs a "Gtk" log handler that drops only the known
    /// libappindicator critical (see [`APPINDICATOR_CRITICAL`]) and
    /// forwards every other message to the default handler unchanged.
    /// GLib handlers are process-wide, so this also covers the menus in
    /// `muda` and the file dialogs in `rfd`.
    fn drop_known_appindicator_critical() {
        let _ = glib::log_set_handler(
            Some("Gtk"),
            LogLevels::LEVEL_CRITICAL,
            true,
            false,
            |domain, level, message| {
                if message.contains(APPINDICATOR_CRITICAL) {
                    tracing::debug!("suppressed libappindicator critical: {message}");
                    return;
                }
                glib::log_default_handler(domain, level, Some(message));
            },
        );
    }

    /// Starts the gtk thread and builds the icon there; returns the
    /// update channel, or `None` when gtk or the icon is unavailable
    /// (e.g. no appindicator) — the app then runs without tray.
    pub(super) fn spawn() -> Option<mpsc::Sender<Update>> {
        let (updates, inbox) = mpsc::channel::<Update>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("oc-tray".to_owned())
            .spawn(move || {
                if gtk::init().is_err() {
                    let _ = ready_tx.send(false);
                    return;
                }
                drop_known_appindicator_critical();
                let Some(parts) = super::build() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                gtk::glib::timeout_add_local(Duration::from_millis(250), move || {
                    loop {
                        match inbox.try_recv() {
                            Ok(update) => super::apply(&parts, update),
                            Err(mpsc::TryRecvError::Empty) => {
                                return gtk::glib::ControlFlow::Continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                // The app dropped its tray handle.
                                gtk::main_quit();
                                return gtk::glib::ControlFlow::Break;
                            }
                        }
                    }
                });
                gtk::main();
            })
            .ok()?;
        ready_rx.recv().ok().filter(|ready| *ready).map(|_| updates)
    }
}
