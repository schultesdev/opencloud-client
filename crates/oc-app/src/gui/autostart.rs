//! Autostart (Phase 7): start the app at login, per OS.
//!
//! - Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! - macOS: `~/Library/LaunchAgents/opencloud-sync.plist`
//! - Linux: `~/.config/autostart/opencloud-sync.desktop`
//!
//! All mechanisms reference the currently running executable.

/// Value name / file label of the autostart entry.
const AUTOSTART_ID: &str = "opencloud-sync";

/// The path of the autostart entry, for messages and existence checks
/// (the registry value name on Windows).
#[cfg_attr(windows, allow(dead_code))]
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

/// The autostart entry's reference to `exe`, as written by [`enable`]:
/// quoted for the Windows `Run` key (paths with spaces), XML-escaped in
/// the plist, quoted and escaped per the Desktop Entry spec on Linux.
fn entry_reference(exe: &str) -> String {
    #[cfg(windows)]
    {
        format!("\"{exe}\"")
    }
    #[cfg(target_os = "macos")]
    {
        xml_escape(exe)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        desktop_exec_quote(exe)
    }
}

/// Escapes text for an XML element.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Quotes a program path for a `.desktop` `Exec` key: inside double
/// quotes, `"`, `` ` ``, `$`, and `\` are backslash-escaped; then the
/// value-level escaping doubles every backslash, and `%` (field codes)
/// becomes `%%`.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn desktop_exec_quote(path: &str) -> String {
    let mut quoted = String::from("\"");
    for character in path.chars() {
        match character {
            // `\$` in the argument, written as `\\$` in the file.
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            // `\\` in the argument, written as `\\\\` in the file.
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// The running executable as text.
fn current_exe() -> Result<String, String> {
    std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .map_err(|error| error.to_string())
}

/// Enables autostart for the current executable.
pub fn enable() -> Result<(), String> {
    let exe = entry_reference(&current_exe()?);
    #[cfg(windows)]
    {
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
        use winreg::enums::HKEY_CURRENT_USER;
        let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                winreg::enums::KEY_SET_VALUE,
            )
            .map_err(|error| error.to_string())?;
        // Only a missing entry counts as "already disabled".
        match key.delete_value(AUTOSTART_ID) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(not(windows))]
    {
        let path = autostart_path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }
}

/// Whether an autostart entry exists that starts the running executable
/// (an entry left by an older install at another path does not count).
pub fn is_enabled() -> bool {
    let Ok(exe) = current_exe() else {
        return false;
    };
    let reference = entry_reference(&exe);
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .and_then(|key| key.get_value::<String, _>(AUTOSTART_ID))
            .is_ok_and(|value| value == reference || value == exe)
    }
    #[cfg(not(windows))]
    {
        autostart_path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|entry| entry.contains(&reference))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_paths_are_quoted_and_escaped() {
        assert_eq!(
            desktop_exec_quote("/opt/Open Cloud/bin"),
            "\"/opt/Open Cloud/bin\""
        );
        assert_eq!(desktop_exec_quote("/a$b%c"), "\"/a\\\\$b%%c\"");
        assert_eq!(xml_escape("/A&B/<x>"), "/A&amp;B/&lt;x&gt;");
    }

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
