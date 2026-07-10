use anyhow::Result;
use camera_toolbox_adapters::{MemoryArtifactStore, SyntheticCaptureAdapter};
use camera_toolbox_app::{CaptureAndAnalyzeRequest, Workflow};
use camera_toolbox_core::{Roi, sensor::CaptureRequest};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "camera-toolbox")]
#[command(about = "Rust-only ISP calibration toolbox CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a synthetic P0 capture/analyze smoke path.
    Smoke,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Smoke => run_smoke()?,
    }
    Ok(())
}

fn run_smoke() -> Result<()> {
    let mut capture = SyntheticCaptureAdapter::default();
    let store = MemoryArtifactStore::default();
    let report = Workflow::run_capture_and_analyze(
        &mut capture,
        &store,
        CaptureAndAnalyzeRequest {
            capture: CaptureRequest {
                mode_id: "default".to_owned(),
                frame_count: 1,
            },
            roi: Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        },
    )?;

    println!(
        "artifact={} min={} max={} mean={:.2} saturated={}/{}",
        report.artifact.raw_path.display(),
        report.stats.min,
        report.stats.max,
        report.stats.mean,
        report.stats.saturated_pixels,
        report.stats.total_pixels
    );
    Ok(())
}
