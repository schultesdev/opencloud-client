//! OpenCloud sync client application.
//!
//! Binary crate: GUI, tray icon, and `main()`. The sync engine is attached
//! via the event/command channels (see `oc-sync`) so that it can run without
//! the GUI in `--headless` mode.

use clap::Parser;

/// OpenCloud sync client.
#[derive(Debug, Parser)]
#[command(name = "opencloud-sync", version, about = "Sync client for OpenCloud")]
struct Args {
    /// Run without GUI: start the sync engine only (CLI mode).
    #[arg(long)]
    headless: bool,
}

fn main() {
    let args = Args::parse();
    if args.headless {
        // The engine itself lands in Phase 5; for now headless mode only
        // reports that configuration handling is in place.
        println!("headless mode: sync engine not implemented yet (Phase 5)");
    } else {
        // The GUI lands in Phase 7; until then point users at headless mode.
        println!("GUI not implemented yet (Phase 7); use --headless for engine-only mode");
    }
}
