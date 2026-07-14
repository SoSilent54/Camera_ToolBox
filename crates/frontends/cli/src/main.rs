use anyhow::Result;
use camera_toolbox_cli::{Cli, run};
use clap::Parser;

fn main() -> Result<()> {
    let _logging = camera_toolbox_logging::init();
    let cli = Cli::parse();
    let command = cli.command_name();
    tracing::debug!(operation = "cli_start", command, "starting CLI command");
    let result = run(cli);
    if let Err(error) = result {
        tracing::error!(operation = "cli_command", command, error = %format_args!("{error:#}"), "CLI command failed");
        return Err(error);
    }
    tracing::debug!(operation = "cli_command", command, "CLI command completed");
    Ok(())
}
