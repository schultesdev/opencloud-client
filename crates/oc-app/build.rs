//! Build script: embeds the application icon into the Windows executable,
//! so Explorer and the taskbar show it for the binary itself (the window
//! and tray icons are set at runtime from the embedded RGBA data).
//!
//! The `.ico` ships in `packaging/icons/` and is derived from
//! `src/assets/icon.rs` with `packaging/icons/generate.py`; this script
//! fails the build on Windows when it is missing, because a silently
//! un-iconed executable would go unnoticed.

fn main() {
    let icon = std::path::Path::new("../../packaging/icons/opencloud-sync.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    #[cfg(target_os = "windows")]
    {
        if !icon.is_file() {
            panic!(
                "packaging/icons/opencloud-sync.ico is missing; \
                 regenerate it with packaging/icons/generate.py"
            );
        }
        if let Err(error) = winresource::WindowsResource::new()
            .set_icon(&icon.display().to_string())
            .compile()
        {
            panic!("failed to embed the executable icon: {error}");
        }
    }
}
