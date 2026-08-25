//! SSH-managed profile 到独立 Command/RemoteFile handles 的绑定。

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use camera_toolbox_app::{
    CapabilityKind, CapabilityLimits, CapabilityRequirement, CapabilityResolutionError,
    CapabilityVariant, CommandService, ConcurrencyPolicy, EvidenceMaturity, InitializationState,
    PlatformCapabilityDescriptor, PlatformCapabilityHandle, PlatformConfig, PlatformKind,
    PlatformProfile, ProfileError, RemoteFileService, RtspCodec, SshManagedBindings, StreamService,
};
use thiserror::Error;

use super::{
    command::{CommandRecipeRegistry, SshCommandService},
    connection::{
        CredentialResolver, RusshTransportFactory, SshConnectionTarget, SshTransportFactory,
    },
    remote_file::SshRemoteFileService,
};
use crate::media::FfmpegRtspStreamService;

pub struct SshManagedPlatformProvider {
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
    recipes: Arc<CommandRecipeRegistry>,
}

impl SshManagedPlatformProvider {
    #[must_use]
    pub fn production(
        resolver: Arc<dyn CredentialResolver>,
        recipes: Arc<CommandRecipeRegistry>,
    ) -> Self {
        Self {
            resolver,
            transport: Arc::new(RusshTransportFactory),
            recipes,
        }
    }

    #[must_use]
    pub fn new(
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
        recipes: Arc<CommandRecipeRegistry>,
    ) -> Self {
        Self {
            resolver,
            transport,
            recipes,
        }
    }

    /// bind 只固化 profile 与 handles；不解析 secret、不连接远端。
    ///
    /// # Errors
    ///
    /// variant/profile/recipe/limit 或 capability binding 无效时返回。
    pub fn bind(
        &self,
        profile: &PlatformProfile,
    ) -> Result<SshManagedBindings, SshManagedProviderError> {
        let platform_snapshot_hash = profile.snapshot_hash()?;
        let PlatformConfig::SshManaged(config) = &profile.config else {
            return Err(SshManagedProviderError::WrongProfileVariant);
        };
        let command = if config.capture_recipe.is_empty() {
            None
        } else {
            if !self.recipes.contains(&config.capture_recipe) {
                return Err(SshManagedProviderError::CaptureRecipeNotRegistered(
                    config.capture_recipe.clone(),
                ));
            }
            let command_output_bytes =
                usize::try_from(config.command_output_bytes).map_err(|_| {
                    SshManagedProviderError::LimitUnsupported {
                        name: "command_output_bytes",
                        value: config.command_output_bytes,
                    }
                })?;
            let command_service = SshCommandService::new(
                format!("ssh-managed:{}:command", profile.id.as_str()),
                SshConnectionTarget {
                    host: config.host.clone(),
                    port: config.port,
                    username: config.username.clone(),
                    expected_host_key: Some(config.expected_host_key.clone()),
                    command_subsystem: config.command_subsystem.clone(),
                    remote_event_subsystem: config.remote_event_subsystem.clone(),
                },
                config.credential_ref.clone(),
                config.capture_recipe.clone(),
                Arc::clone(&self.resolver),
                Arc::clone(&self.transport),
                Arc::clone(&self.recipes),
                command_output_bytes,
            );
            let descriptor = Arc::new(command_descriptor(config, platform_snapshot_hash));
            Some(PlatformCapabilityHandle {
                service: Arc::new(command_service) as Arc<dyn CommandService>,
                descriptor,
            })
        };
        let target = SshConnectionTarget {
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            expected_host_key: Some(config.expected_host_key.clone()),
            command_subsystem: config.command_subsystem.clone(),
            remote_event_subsystem: config.remote_event_subsystem.clone(),
        };
        let remote_file_service = SshRemoteFileService::new(
            format!("ssh-managed:{}:remote-file", profile.id.as_str()),
            target,
            config.credential_ref.clone(),
            Arc::clone(&self.resolver),
            Arc::clone(&self.transport),
            config.remote_artifact_dir.clone(),
            config.remote_artifact_glob.clone(),
            config.stable_samples,
            Duration::from_millis(config.stability_interval_ms),
            config.passive_watch_auto_open,
            config.max_fetch_bytes,
        );
        let remote_descriptor = Arc::new(PlatformCapabilityDescriptor {
            capability: CapabilityKind::RemoteFile,
            requirement: CapabilityRequirement::PlatformOnly,
            provider: PlatformKind::SshManaged,
            driver: "russh-sftp-bounded-v1".to_owned(),
            evidence: EvidenceMaturity::ProtocolVerified,
            minimum_sensor_evidence: EvidenceMaturity::Unknown,
            initialization: InitializationState::NotRequired,
            concurrency: ConcurrencyPolicy::Independent,
            hard_limits: CapabilityLimits {
                max_payload_bytes: Some(config.max_fetch_bytes),
                max_concurrent_operations: Some(1),
            },
            supported_variants: BTreeSet::from([
                CapabilityVariant::RemoteFetch,
                CapabilityVariant::RemoteWatch,
            ]),
            platform_snapshot_hash,
        });
        let command: Option<PlatformCapabilityHandle<dyn CommandService>> = command;
        let remote_file: PlatformCapabilityHandle<dyn RemoteFileService> =
            PlatformCapabilityHandle {
                service: Arc::new(remote_file_service),
                descriptor: remote_descriptor,
            };
        let stream = config.rtsp.as_ref().map(|rtsp| {
            let descriptor = Arc::new(PlatformCapabilityDescriptor {
                capability: CapabilityKind::Stream,
                requirement: CapabilityRequirement::PlatformOnly,
                provider: PlatformKind::SshManaged,
                driver: "ffmpeg-rtsp-showinfo-v1".to_owned(),
                evidence: EvidenceMaturity::ProfileDeclared,
                minimum_sensor_evidence: EvidenceMaturity::Unknown,
                initialization: InitializationState::NotRequired,
                concurrency: ConcurrencyPolicy::Independent,
                hard_limits: CapabilityLimits {
                    max_payload_bytes: None,
                    max_concurrent_operations: Some(1),
                },
                supported_variants: BTreeSet::from([match rtsp.codec {
                    RtspCodec::H264 => CapabilityVariant::H264,
                    RtspCodec::H265 => CapabilityVariant::H265,
                }]),
                platform_snapshot_hash,
            });
            PlatformCapabilityHandle {
                service: Arc::new(FfmpegRtspStreamService::new(
                    format!("ssh-managed:{}:rtsp", profile.id.as_str()),
                    rtsp.clone(),
                )) as Arc<dyn StreamService>,
                descriptor,
            }
        });
        SshManagedBindings::new(
            profile.id.clone(),
            platform_snapshot_hash,
            Some(remote_file),
            command,
            stream,
        )
        .map_err(Into::into)
    }
}

fn command_descriptor(
    config: &camera_toolbox_app::SshManagedConfig,
    platform_snapshot_hash: camera_toolbox_app::SnapshotHash,
) -> PlatformCapabilityDescriptor {
    PlatformCapabilityDescriptor {
        capability: CapabilityKind::Command,
        requirement: CapabilityRequirement::PlatformOnly,
        provider: PlatformKind::SshManaged,
        driver: if config.command_subsystem.is_some() {
            "russh-ctargv1-subsystem-v1".to_owned()
        } else {
            "russh-standard-exec-posix-argv-v1".to_owned()
        },
        evidence: EvidenceMaturity::ProtocolVerified,
        minimum_sensor_evidence: EvidenceMaturity::Unknown,
        initialization: InitializationState::NotRequired,
        concurrency: ConcurrencyPolicy::Independent,
        hard_limits: CapabilityLimits {
            max_payload_bytes: Some(config.command_output_bytes),
            max_concurrent_operations: Some(1),
        },
        supported_variants: BTreeSet::from([if config.command_subsystem.is_some() {
            CapabilityVariant::ArgvSubsystemCommand
        } else {
            CapabilityVariant::StandardExecCommand
        }]),
        platform_snapshot_hash,
    }
}

#[derive(Debug, Error)]
pub enum SshManagedProviderError {
    #[error("SSH-managed provider requires an SshManaged profile")]
    WrongProfileVariant,
    #[error("capture recipe is not registered: {0}")]
    CaptureRecipeNotRegistered(String),
    #[error("profile limit {name}={value} cannot fit this host")]
    LimitUnsupported { name: &'static str, value: u64 },
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Capability(#[from] CapabilityResolutionError),
}
