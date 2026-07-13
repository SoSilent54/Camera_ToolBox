use anyhow::Result;

fn main() -> Result<()> {
    let _logging = camera_toolbox_logging::init();
    tracing::debug!(operation = "tui_start", "starting TUI frontend");
    println!("Camera Toolbox TUI placeholder: P0 workflow wiring lives in CLI for now.");
    tracing::debug!(operation = "tui_exit", "TUI frontend exited");
    Ok(())
}
