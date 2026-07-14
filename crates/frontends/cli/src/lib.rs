mod cli;
mod json;
mod platform;

use std::str::FromStr;

use anyhow::{Result, anyhow};
use camera_toolbox_adapters::{LocalRawLoader, MemoryArtifactStore, SyntheticCaptureAdapter};
use camera_toolbox_app::{CaptureAndAnalyzeRequest, LocalRawAnalyzeRequest, Workflow};
use camera_toolbox_core::{BayerPattern, RawEncoding, RawSpec, Roi, sensor::CaptureRequest};

pub use cli::Cli;
use cli::{AnalyzeRawArgs, BayerArg, Command, EncodingArg};

/// Execute one already-parsed CLI command.
///
/// # Errors
///
/// Returns profile, binding, resolution, submission, terminal-job, persistence, or workflow
/// failures so the process entrypoint exits nonzero.
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Smoke => run_smoke(),
        Command::AnalyzeRaw(args) => analyze_raw(args),
        Command::Profile { command } => platform::run_profile(command),
        Command::Platform { command } => platform::run_platform(command),
        Command::Cv610 { command } => platform::run_cv610(command),
        Command::StreamRecord(args) => platform::run_stream_record(&args),
        Command::Ssh { command } => platform::run_ssh(command),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RoiArg(Roi);

impl From<RoiArg> for Roi {
    fn from(value: RoiArg) -> Self {
        value.0
    }
}

impl FromStr for RoiArg {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts = value
            .split(',')
            .map(str::trim)
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()?;
        if parts.len() != 4 {
            return Err(anyhow!("roi must be x,y,width,height"));
        }
        Ok(Self(Roi {
            x: parts[0],
            y: parts[1],
            width: parts[2],
            height: parts[3],
        }))
    }
}

impl From<BayerArg> for BayerPattern {
    fn from(value: BayerArg) -> Self {
        match value {
            BayerArg::Rggb => Self::Rggb,
            BayerArg::Grbg => Self::Grbg,
            BayerArg::Gbrg => Self::Gbrg,
            BayerArg::Bggr => Self::Bggr,
        }
    }
}

impl From<EncodingArg> for RawEncoding {
    fn from(value: EncodingArg) -> Self {
        match value {
            EncodingArg::U16Le => Self::U16Le,
        }
    }
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

fn analyze_raw(args: AnalyzeRawArgs) -> Result<()> {
    let AnalyzeRawArgs {
        raw,
        width,
        height,
        bit_depth,
        bayer,
        encoding,
        roi,
    } = args;
    let spec = RawSpec {
        width,
        height,
        bit_depth,
        bayer: bayer.into(),
    };
    let roi = roi.map_or(
        Roi {
            x: 0,
            y: 0,
            width,
            height,
        },
        Into::into,
    );
    let loader = LocalRawLoader;
    let report = Workflow::load_raw_and_analyze(
        &loader,
        LocalRawAnalyzeRequest {
            path: raw,
            spec,
            encoding: encoding.into(),
            roi,
        },
    )?;

    println!(
        "raw={} width={} height={} bit_depth={} roi={},{},{},{} min={} max={} mean={:.2} saturated={}/{}",
        report.path.display(),
        report.frame.spec.width,
        report.frame.spec.height,
        report.frame.spec.bit_depth,
        report.roi.x,
        report.roi.y,
        report.roi.width,
        report.roi.height,
        report.stats.min,
        report.stats.max,
        report.stats.mean,
        report.stats.saturated_pixels,
        report.stats.total_pixels
    );
    Ok(())
}
