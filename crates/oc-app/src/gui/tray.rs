//! Tray icon (Phase 7): status tooltip (running/errors/conflicts), menu
//! with show/pause/quit, and close-to-tray. Events are polled from the
//! global `tray-icon` channels each frame — no extra event loop needed.

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

/// The tray icon handle; keep it alive or the icon disappears.
pub struct Tray {
    icon: TrayIcon,
    show: MenuItem,
    toggle: MenuItem,
}

impl Tray {
    /// Builds the tray icon; `None` when the platform rejected it (the
    /// app keeps running without tray).
    pub fn new() -> Option<Self> {
        let show = MenuItem::with_id("show", "Fenster zeigen", true, None);
        let toggle = MenuItem::with_id("toggle-pause", "Alle pausieren", true, None);
        let quit = MenuItem::with_id("quit", "Beenden", true, None);
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
        Some(Self { icon, show, toggle })
    }

    /// Updates the pause label ("Alle pausieren" / "Alle fortsetzen").
    pub fn set_paused(&self, paused: bool) {
        self.toggle.set_text(if paused {
            "Alle fortsetzen"
        } else {
            "Alle pausieren"
        });
    }

    /// Sets the status tooltip, e.g. `OpenCloud Sync — 2 Konflikte`.
    pub fn set_status(&mut self, running: bool, errors: u64, conflicts: u64) {
        let mut tooltip = String::from("OpenCloud Sync");
        if running {
            tooltip.push_str(" — Synchronisierung läuft");
        }
        if errors > 0 {
            tooltip.push_str(&format!(" — {errors} Fehler"));
        }
        if conflicts > 0 {
            tooltip.push_str(&format!(" — {conflicts} Konflikte"));
        }
        let _ = self.icon.set_tooltip(Some(&tooltip));
    }

    /// Drains pending menu and tray events into actions.
    pub fn poll(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.show.id() {
                actions.push(TrayAction::Show);
            } else if event.id == self.toggle.id() {
                actions.push(TrayAction::TogglePause);
            } else if event.id.0 == "quit" {
                actions.push(TrayAction::Quit);
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
