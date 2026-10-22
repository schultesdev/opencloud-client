//! Autostart (Phase 7): start the app at login, per OS.
//!
//! - Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! - macOS: `~/Library/LaunchAgents/eu.opencloud.sync.plist`
//! - Linux: `~/.config/autostart/opencloud-sync.desktop`
//!
//! All mechanisms reference the currently running executable.

/// Value name / file label of the autostart entry.
const AUTOSTART_ID: &str = "opencloud-sync";

/// The path of the autostart entry, for messages and existence checks
/// (the registry value name on Windows).
fn autostart_path() -> Result<std::path::PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").ok_or("HOME ist nicht gesetzt")?;
        let dir = std::path::PathBuf::from(home).join("Library/LaunchAgents");
        Ok(dir.join(format!("{AUTOSTART_ID}.plist")))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = std::env::var_os("HOME").ok_or("HOME ist nicht gesetzt")?;
        let dir = std::path::PathBuf::from(home).join(".config/autostart");
        Ok(dir.join(format!("{AUTOSTART_ID}.desktop")))
    }
    #[cfg(windows)]
    {
        Ok(std::path::PathBuf::from(AUTOSTART_ID))
    }
}

/// Enables autostart for the current executable.
pub fn enable() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = exe.display().to_string();
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                winreg::enums::KEY_SET_VALUE,
            )
            .map_err(|error| error.to_string())?;
        key.set_value(AUTOSTART_ID, &exe)
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let path = autostart_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>{AUTOSTART_ID}</string>\n\
             <key>ProgramArguments</key><array><string>{exe}</string></array>\n\
             <key>RunAtLoad</key><true/>\n\
             </dict></plist>\n"
        );
        std::fs::write(&path, plist).map_err(|error| error.to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = autostart_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        let desktop = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=OpenCloud Sync\n\
             Exec={exe}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(&path, desktop).map_err(|error| error.to_string())
    }
}

/// Disables autostart (removes the entry).
pub fn disable() -> Result<(), String> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                winreg::enums::KEY_SET_VALUE,
            )
            .map_err(|error| error.to_string())?;
        match key.delete_value(AUTOSTART_ID) {
            Ok(()) | Err(_) => Ok(()),
        }
    }
    #[cfg(not(windows))]
    {
        let path = autostart_path()?;
        match std::fs::remove_file(&path) {
            Ok(()) | Err(_) => Ok(()),
        }
    }
}

/// Whether the autostart entry exists.
pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .and_then(|key| key.get_value::<String, _>(AUTOSTART_ID))
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        autostart_path().is_ok_and(|path| path.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_path_is_absolute() {
        // On Windows the "path" is the registry value name; skip.
        #[cfg(not(windows))]
        {
            let path = autostart_path().expect("path");
            assert!(path.is_absolute(), "got {}", path.display());
        }
    }
}
