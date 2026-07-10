use std::{path::PathBuf, str::FromStr};

use anyhow::{Result, anyhow};
use camera_toolbox_adapters::{LocalRawLoader, MemoryArtifactStore, SyntheticCaptureAdapter};
use camera_toolbox_app::{CaptureAndAnalyzeRequest, LocalRawAnalyzeRequest, Workflow};
use camera_toolbox_core::{BayerPattern, RawEncoding, RawSpec, Roi, sensor::CaptureRequest};
use clap::{Parser, Subcommand, ValueEnum};

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
    /// Analyze a local unpacked u16le RAW file with an explicit spec.
    AnalyzeRaw(AnalyzeRawArgs),
}

#[derive(Debug, Parser)]
struct AnalyzeRawArgs {
    /// Local RAW file path.
    #[arg(long)]
    raw: PathBuf,
    /// Active image width, excluding stride padding.
    #[arg(long)]
    width: u32,
    /// Active image height.
    #[arg(long)]
    height: u32,
    /// Valid sample bit depth.
    #[arg(long)]
    bit_depth: u8,
    /// Decoded row stride in pixels; defaults to width.
    #[arg(long)]
    stride_pixels: Option<u32>,
    /// Bayer metadata. Display is grayscale in this phase.
    #[arg(long, value_enum, default_value = "rggb")]
    bayer: BayerArg,
    /// Storage encoding. Only unpacked u16le is supported in this phase.
    #[arg(long, value_enum, default_value = "u16le")]
    encoding: EncodingArg,
    /// ROI as x,y,width,height. Defaults to full active image.
    #[arg(long)]
    roi: Option<RoiArg>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BayerArg {
    #[value(name = "rggb")]
    Rggb,
    #[value(name = "grbg")]
    Grbg,
    #[value(name = "gbrg")]
    Gbrg,
    #[value(name = "bggr")]
    Bggr,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncodingArg {
    #[value(name = "u16le")]
    U16Le,
}

impl From<EncodingArg> for RawEncoding {
    fn from(value: EncodingArg) -> Self {
        match value {
            EncodingArg::U16Le => Self::U16Le,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RoiArg(Roi);

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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Smoke => run_smoke()?,
        Command::AnalyzeRaw(args) => analyze_raw(args)?,
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

fn analyze_raw(args: AnalyzeRawArgs) -> Result<()> {
    let spec = RawSpec {
        width: args.width,
        height: args.height,
        bit_depth: args.bit_depth,
        stride_pixels: args.stride_pixels.unwrap_or(args.width),
        bayer: args.bayer.into(),
    };
    let roi = args.roi.map(Into::into).unwrap_or(Roi {
        x: 0,
        y: 0,
        width: args.width,
        height: args.height,
    });
    let loader = LocalRawLoader;
    let report = Workflow::load_raw_and_analyze(
        &loader,
        LocalRawAnalyzeRequest {
            path: args.raw.clone(),
            spec,
            encoding: args.encoding.into(),
            roi,
        },
    )?;

    println!(
        "raw={} width={} height={} stride_pixels={} bit_depth={} roi={},{},{},{} min={} max={} mean={:.2} saturated={}/{}",
        report.path.display(),
        report.frame.spec.width,
        report.frame.spec.height,
        report.frame.spec.stride_pixels,
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
