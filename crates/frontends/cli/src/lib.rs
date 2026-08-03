mod cli;
mod json;
mod platform;

use std::{fs, str::FromStr};

use anyhow::{Result, anyhow, bail};
use camera_toolbox_adapters::{LocalRawLoader, MemoryArtifactStore, SyntheticCaptureAdapter};
use camera_toolbox_app::{CaptureAndAnalyzeRequest, LocalRawAnalyzeRequest, Workflow};
use camera_toolbox_core::{
    BayerPattern, RawEncoding, RawSpec, Roi, compile_eeprom_map_config_text,
    dump_builtin_eeprom_map_config, list_builtin_eeprom_map_configs, sensor::CaptureRequest,
};

pub use cli::Cli;
use cli::{AnalyzeRawArgs, BayerArg, Command, EncodingArg, ImportEepromMapArgs};

/// Execute one already-parsed CLI command.
///
/// # Errors
///
/// Returns profile, binding, resolution, submission, terminal-job, persistence, or workflow
/// failures so the process entrypoint exits nonzero.
pub fn run(cli: Cli) -> Result<()> {
    if cli.list_configs {
        if cli.command.is_some() || cli.dump_config.is_some() {
            bail!("--list-configs cannot be combined with another command or --dump-config");
        }
        return list_eeprom_map_configs();
    }
    if let Some(name) = cli.dump_config {
        if cli.command.is_some() {
            bail!("--dump-config cannot be combined with a subcommand");
        }
        return dump_eeprom_map_config(&name);
    }
    let Some(command) = cli.command else {
        bail!("missing CLI command")
    };
    match command {
        Command::Smoke => run_smoke(),
        Command::AnalyzeRaw(args) => analyze_raw(args),
        Command::ImportEepromMap(args) => import_eeprom_map(args),
        Command::Profile { command } => platform::run_profile(command),
        Command::Platform { command } => platform::run_platform(command),
        #[cfg(feature = "platform-cv610")]
        Command::Cv610 { command } => platform::run_cv610(command),
        #[cfg(feature = "platform-cv610")]
        Command::StreamRecord(args) => platform::run_stream_record(&args),
        #[cfg(feature = "platform-ssh")]
        Command::Ssh { command } => platform::run_ssh(command),
    }
}

fn list_eeprom_map_configs() -> Result<()> {
    let configs = list_builtin_eeprom_map_configs()
        .iter()
        .map(|config| {
            json::json!({
                "name": config.name,
                "display_name": config.display_name,
                "source_map_id": config.source_map_id,
            })
        })
        .collect::<Vec<_>>();
    println!("{}", json::json!({"configs": configs}));
    Ok(())
}

fn dump_eeprom_map_config(name: &str) -> Result<()> {
    let text = dump_builtin_eeprom_map_config(name)?;
    print!("{text}");
    Ok(())
}

fn import_eeprom_map(args: ImportEepromMapArgs) -> Result<()> {
    let text = match (args.config_file, args.config_text) {
        (Some(path), None) => fs::read_to_string(&path)
            .map_err(|error| anyhow!("failed to read {}: {error}", path.display()))?,
        (None, Some(text)) => text,
        (Some(_), Some(_)) => bail!("--config-file and --config-text are mutually exclusive"),
        (None, None) => bail!("import-eeprom-map requires --config-file or --config-text"),
    };
    let compiled =
        compile_eeprom_map_config_text("imported-eeprom-map", "Imported EEPROM map", &text)?;
    println!(
        "{}",
        json::json!({
            "id": compiled.id,
            "display_name": compiled.display_name,
            "header_name": compiled.header_name,
            "bus_label": compiled.bus_label,
            "total_bytes": compiled.total_bytes,
            "field_count": compiled.fields.len(),
            "i2c_address": compiled.transport.i2c_address,
            "address_width_bits": compiled.transport.address_width_bits,
            "page_size_bytes": compiled.transport.page_size_bytes,
            "write_cycle_ms": compiled.transport.write_cycle_ms,
        })
    );
    Ok(())
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
