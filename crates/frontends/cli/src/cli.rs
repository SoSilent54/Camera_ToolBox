use std::{num::NonZeroU64, path::PathBuf};

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "camera-toolbox")]
#[command(about = "Rust-only ISP calibration toolbox CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

impl Cli {
    #[must_use]
    pub const fn command_name(&self) -> &'static str {
        self.command.name()
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a synthetic P0 capture/analyze smoke path.
    Smoke,
    /// Analyze a local unpacked u16le RAW file with an explicit spec.
    AnalyzeRaw(AnalyzeRawArgs),
    /// Inspect or validate the versioned platform profile store.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Resolve platform capabilities without opening a network connection.
    Platform {
        #[command(subcommand)]
        command: PlatformCommand,
    },
    /// Run a CV610 production operation.
    Cv610 {
        #[command(subcommand)]
        command: Cv610Command,
    },
    /// Record a CV610 stream for an explicit finite duration.
    StreamRecord(StreamRecordArgs),
    /// Run an SSH-managed production operation.
    Ssh {
        #[command(subcommand)]
        command: SshCommand,
    },
}

impl Command {
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::AnalyzeRaw(_) => "analyze_raw",
            Self::Profile { command } => command.name(),
            Self::Platform { command } => command.name(),
            Self::Cv610 { command } => command.name(),
            Self::StreamRecord(_) => "stream_record",
            Self::Ssh { command } => command.name(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// List platform and Sensor profiles in stable key order.
    List(ProfileStoreArgs),
    /// Load and validate the complete profile store.
    Validate(ProfileStoreArgs),
}

impl ProfileCommand {
    const fn name(&self) -> &'static str {
        match self {
            Self::List(_) => "profile_list",
            Self::Validate(_) => "profile_validate",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum PlatformCommand {
    /// Bind and resolve one exact platform/Sensor selection.
    Probe(TargetArgs),
}

impl PlatformCommand {
    const fn name(&self) -> &'static str {
        match self {
            Self::Probe(_) => "platform_probe",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Cv610Command {
    /// Submit one verified still Dump job.
    Dump(Cv610DumpArgs),
}

impl Cv610Command {
    const fn name(&self) -> &'static str {
        match self {
            Self::Dump(_) => "cv610_dump",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum SshCommand {
    /// Run the profile's allowlisted one-shot capture recipe.
    Capture(SshCaptureArgs),
    /// Fetch one explicit remote artifact into bounded memory.
    Fetch(SshFetchArgs),
}

impl SshCommand {
    const fn name(&self) -> &'static str {
        match self {
            Self::Capture(_) => "ssh_capture",
            Self::Fetch(_) => "ssh_fetch",
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct ProfileStoreArgs {
    /// Versioned profile-store JSON; defaults to the per-user project path.
    #[arg(long)]
    pub profile_store: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct TargetArgs {
    /// Versioned profile-store JSON; defaults to the per-user project path.
    #[arg(long)]
    pub profile_store: Option<PathBuf>,
    /// Exact `PlatformProfileId` to bind.
    #[arg(long)]
    pub platform: String,
    /// Exact `SensorId`; must be paired with --mode-id.
    #[arg(long, requires = "mode_id")]
    pub sensor_id: Option<String>,
    /// Exact `SensorModeId`; must be paired with --sensor-id.
    #[arg(long, requires = "sensor_id")]
    pub mode_id: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct Cv610DumpArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Verified `PQTools` payload kind.
    #[arg(long, value_enum)]
    pub kind: DumpKindArg,
    /// Optional new local target. Existing files are never overwritten.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("recording_output")
        .required(true)
        .multiple(true)
        .args(["transport_output", "annexb_output"])
))]
pub struct StreamRecordArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Finite recording duration in seconds.
    #[arg(long)]
    pub duration: NonZeroU64,
    /// Aggregate recorder quota across the enabled branches.
    #[arg(long)]
    pub quota_bytes: NonZeroU64,
    /// Optional exact transport-evidence destination.
    #[arg(long)]
    pub transport_output: Option<PathBuf>,
    /// Optional Annex-B destination; requires --timestamp-output.
    #[arg(long, requires = "timestamp_output")]
    pub annexb_output: Option<PathBuf>,
    /// Required RTP timestamp sidecar for --annexb-output.
    #[arg(long, requires = "annexb_output")]
    pub timestamp_output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct SshCaptureArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Explicit allowlisted format value supplied to the production recipe.
    #[arg(long)]
    pub format: String,
}

#[derive(Debug, Clone, Args)]
pub struct SshFetchArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Exact remote path inside the profile's configured artifact root.
    #[arg(long)]
    pub remote_path: String,
    /// Explicit media interpretation for the fetched bytes.
    #[arg(long, value_enum)]
    pub format: MediaFormatArg,
    /// Optional expected payload SHA-256.
    #[arg(long)]
    pub expected_sha256: Option<String>,
    /// Optional new local target. Existing files are never overwritten.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct AnalyzeRawArgs {
    /// Local RAW file path.
    #[arg(long)]
    pub raw: PathBuf,
    /// Active image width.
    #[arg(long)]
    pub width: u32,
    /// Active image height.
    #[arg(long)]
    pub height: u32,
    /// Valid sample bit depth.
    #[arg(long)]
    pub bit_depth: u8,
    /// Bayer metadata. Display is grayscale in this phase.
    #[arg(long, value_enum, default_value = "rggb")]
    pub bayer: BayerArg,
    /// Storage encoding. Only unpacked u16le is supported in this phase.
    #[arg(long, value_enum, default_value = "u16le")]
    pub encoding: EncodingArg,
    /// ROI as x,y,width,height. Defaults to full active image.
    #[arg(long)]
    pub roi: Option<crate::RoiArg>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DumpKindArg {
    Raw10,
    Raw12,
    Jpeg,
    Nv21,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MediaFormatArg {
    Raw10Packed,
    Raw12Packed,
    Raw10U16le,
    Raw12U16le,
    Jpeg,
    Nv12,
    Nv21,
    H264,
    H265,
    Binary,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BayerArg {
    #[value(name = "rggb")]
    Rggb,
    #[value(name = "grbg")]
    Grbg,
    #[value(name = "gbrg")]
    Gbrg,
    #[value(name = "bggr")]
    Bggr,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EncodingArg {
    #[value(name = "u16le")]
    U16Le,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_existing_and_platform_commands() {
        assert!(matches!(
            Cli::try_parse_from(["camera-toolbox", "smoke"])
                .unwrap()
                .command,
            Command::Smoke
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "camera-toolbox",
                "analyze-raw",
                "--raw",
                "frame.raw",
                "--width",
                "2",
                "--height",
                "2",
                "--bit-depth",
                "12"
            ])
            .unwrap()
            .command,
            Command::AnalyzeRaw(_)
        ));
        let parsed = Cli::try_parse_from([
            "camera-toolbox",
            "cv610",
            "dump",
            "--platform",
            "lab",
            "--kind",
            "raw12",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Command::Cv610 {
                command: Cv610Command::Dump(_)
            }
        ));
    }

    #[test]
    fn clap_rejects_unpaired_sensor_selection_and_unbounded_recording() {
        assert!(
            Cli::try_parse_from([
                "camera-toolbox",
                "platform",
                "probe",
                "--platform",
                "lab",
                "--sensor-id",
                "imx415"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "camera-toolbox",
                "stream-record",
                "--platform",
                "lab",
                "--duration",
                "0",
                "--quota-bytes",
                "1",
                "--transport-output",
                "record.bin"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "camera-toolbox",
                "stream-record",
                "--platform",
                "lab",
                "--duration",
                "1",
                "--quota-bytes",
                "1"
            ])
            .is_err()
        );
    }
}
