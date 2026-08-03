#[cfg(feature = "platform-cv610")]
use std::num::NonZeroU64;
use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "camera-toolbox")]
#[command(about = "Rust-only ISP calibration toolbox CLI")]
pub struct Cli {
    /// List built-in EEPROM map configs.
    #[arg(long)]
    pub(crate) list_configs: bool,
    /// Dump one built-in EEPROM map config as canonical text.
    #[arg(long, value_name = "NAME")]
    pub(crate) dump_config: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

impl Cli {
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        if self.list_configs {
            "eeprom_config_list"
        } else if self.dump_config.is_some() {
            "eeprom_config_dump"
        } else {
            self.command.as_ref().map_or("none", Command::name)
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a synthetic P0 capture/analyze smoke path.
    Smoke,
    /// Analyze a local unpacked u16le RAW file with an explicit spec.
    AnalyzeRaw(AnalyzeRawArgs),
    /// Import and validate a canonical EEPROM map config.
    ImportEepromMap(ImportEepromMapArgs),
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
    #[cfg(feature = "platform-cv610")]
    /// Run a CV610 production operation.
    Cv610 {
        #[command(subcommand)]
        command: Cv610Command,
    },
    #[cfg(feature = "platform-cv610")]
    /// Record a CV610 stream for an explicit finite duration.
    StreamRecord(StreamRecordArgs),
    #[cfg(feature = "platform-ssh")]
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
            Self::ImportEepromMap(_) => "import_eeprom_map",
            Self::Profile { command } => command.name(),
            Self::Platform { command } => command.name(),
            #[cfg(feature = "platform-cv610")]
            Self::Cv610 { command } => command.name(),
            #[cfg(feature = "platform-cv610")]
            Self::StreamRecord(_) => "stream_record",
            #[cfg(feature = "platform-ssh")]
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

#[cfg(feature = "platform-cv610")]
#[derive(Debug, Subcommand)]
pub enum Cv610Command {
    /// Submit one verified still Dump job.
    Dump(Cv610DumpArgs),
}

#[cfg(feature = "platform-cv610")]
impl Cv610Command {
    const fn name(&self) -> &'static str {
        match self {
            Self::Dump(_) => "cv610_dump",
        }
    }
}

#[cfg(feature = "platform-ssh")]
#[derive(Debug, Subcommand)]
pub enum SshCommand {
    /// Run the profile's allowlisted one-shot capture recipe.
    Capture(SshCaptureArgs),
    /// Fetch one explicit remote artifact into bounded memory.
    Fetch(SshFetchArgs),
}
#[cfg(feature = "platform-ssh")]

impl SshCommand {
    const fn name(&self) -> &'static str {
        match self {
            Self::Capture(_) => "ssh_capture",
            Self::Fetch(_) => "ssh_fetch",
        }
    }
}

#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("config_source")
        .required(true)
        .args(["config_file", "config_text"])
))]
pub struct ImportEepromMapArgs {
    /// Canonical EEPROM map config file to validate/import.
    #[arg(long, value_name = "PATH")]
    pub config_file: Option<PathBuf>,
    /// Canonical EEPROM map config text to validate/import.
    #[arg(long, value_name = "TEXT")]
    pub config_text: Option<String>,
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

#[cfg(feature = "platform-cv610")]
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

#[cfg(feature = "platform-cv610")]
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

#[cfg(feature = "platform-ssh")]
#[derive(Debug, Clone, Args)]
pub struct SshCaptureArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Explicit allowlisted format value supplied to the production recipe.
    #[arg(long)]
    pub format: String,
}

#[cfg(feature = "platform-ssh")]
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

#[cfg(feature = "platform-cv610")]
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DumpKindArg {
    Raw10,
    Raw12,
    Jpeg,
    Nv21,
}

#[cfg(feature = "platform-ssh")]
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
            Some(Command::Smoke)
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
            Some(Command::AnalyzeRaw(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["camera-toolbox", "platform", "probe", "--platform", "lab"])
                .unwrap()
                .command,
            Some(Command::Platform { .. })
        ));
        #[cfg(feature = "platform-cv610")]
        {
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
                Some(Command::Cv610 {
                    command: Cv610Command::Dump(_)
                })
            ));
        }
        #[cfg(feature = "platform-ssh")]
        {
            let parsed = Cli::try_parse_from([
                "camera-toolbox",
                "ssh",
                "capture",
                "--platform",
                "lab",
                "--format",
                "raw12",
            ])
            .unwrap();
            assert!(matches!(parsed.command, Some(Command::Ssh { .. })));
        }
        #[cfg(not(feature = "platform-cv610"))]
        assert!(Cli::try_parse_from(["camera-toolbox", "cv610", "dump"]).is_err());
        #[cfg(not(feature = "platform-ssh"))]
        assert!(Cli::try_parse_from(["camera-toolbox", "ssh", "capture"]).is_err());
    }

    #[test]
    fn parses_eeprom_map_config_entrypoints() {
        let parsed = Cli::try_parse_from(["camera-toolbox", "--list-configs"]).unwrap();
        assert!(parsed.list_configs);
        assert!(parsed.command.is_none());

        let parsed =
            Cli::try_parse_from(["camera-toolbox", "--dump-config", "pueo-edu-df9-40-pinout"])
                .unwrap();
        assert_eq!(
            parsed.dump_config.as_deref(),
            Some("pueo-edu-df9-40-pinout")
        );

        let parsed = Cli::try_parse_from([
            "camera-toolbox",
            "import-eeprom-map",
            "--config-text",
            "PUEO-EDU DF9-40, I2C0, 0x50, Addr16, Page16, Size1\nRemark / Offset / Size / Type\nA / 0x0000 / 1 / U8\n",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Some(Command::ImportEepromMap(_))));

        assert!(
            Cli::try_parse_from([
                "camera-toolbox",
                "import-eeprom-map",
                "--config-file",
                "a.txt",
                "--config-text",
                "text",
            ])
            .is_err()
        );
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
        #[cfg(feature = "platform-cv610")]
        {
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
}
