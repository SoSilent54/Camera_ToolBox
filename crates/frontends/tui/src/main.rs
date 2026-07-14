use std::process::ExitCode;

use camera_toolbox_tui::{Args, run, write_fatal_error};
use clap::Parser;

fn main() -> ExitCode {
    let _logging = camera_toolbox_logging::init();
    let args = Args::parse();
    tracing::debug!(
        operation = "tui_start",
        snapshot = args.snapshot,
        "starting TUI frontend"
    );
    match run(args) {
        Ok(()) => {
            tracing::debug!(operation = "tui_exit", "TUI frontend exited");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(operation = "tui_exit", error = %error, "TUI frontend failed");
            let _ = write_fatal_error(&mut std::io::stderr(), error.root_cause());
            ExitCode::FAILURE
        }
    }
}
