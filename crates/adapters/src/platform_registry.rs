//! Tagged profile 到独立 provider variant 的唯一 adapter registry。

use std::{collections::BTreeSet, sync::Arc};

use camera_toolbox_app::{
    CapabilityKind, CapabilityLimits, CapabilityRequirement, CapabilityResolutionError,
    CapabilityVariant, ConcurrencyPolicy, EvidenceMaturity, InitializationState, LocalBindings,
    LocalOpenService, PlatformBindings, PlatformCapabilityDescriptor, PlatformCapabilityHandle,
    PlatformConfig, PlatformKind, PlatformProfile, ProfileError,
};
use thiserror::Error;

#[cfg(feature = "platform-cv610")]
use crate::platforms::hisilicon_cv610::{Cv610ProviderError, HisiliconCv610Provider};
#[cfg(feature = "platform-ssh")]
use crate::platforms::ssh_managed::{SshManagedPlatformProvider, SshManagedProviderError};

#[derive(Debug)]
struct LocalOpenAdapter {
    service_id: String,
}

impl LocalOpenService for LocalOpenAdapter {
    fn service_id(&self) -> &str {
        &self.service_id
    }
}

#[derive(Default)]
pub struct PlatformRegistry {
    #[cfg(feature = "platform-cv610")]
    cv610: Option<HisiliconCv610Provider>,
    #[cfg(feature = "platform-ssh")]
    ssh_managed: Option<SshManagedPlatformProvider>,
}

impl PlatformRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(feature = "platform-cv610")]
    pub fn register_cv610(&mut self, provider: HisiliconCv610Provider) {
        self.cv610 = Some(provider);
    }

    #[cfg(feature = "platform-ssh")]
    pub fn register_ssh(&mut self, provider: SshManagedPlatformProvider) {
        self.ssh_managed = Some(provider);
    }

    /// Local 在 registry 内构造无连接 handle；网络 variant 委派给独立 provider。
    pub fn bind(
        &self,
        profile: &PlatformProfile,
    ) -> Result<PlatformBindings, PlatformRegistryError> {
        match &profile.config {
            PlatformConfig::HisiliconCv610(_) => {
                #[cfg(feature = "platform-cv610")]
                {
                    let provider =
                        self.cv610
                            .as_ref()
                            .ok_or(PlatformRegistryError::PlatformNotCompiled(
                                PlatformKind::HisiliconCv610,
                            ))?;
                    return Ok(PlatformBindings::Cv610(Arc::new(provider.bind(profile)?)));
                }
                #[cfg(not(feature = "platform-cv610"))]
                {
                    return Err(PlatformRegistryError::PlatformNotCompiled(
                        PlatformKind::HisiliconCv610,
                    ));
                }
            }
            PlatformConfig::SshManaged(_) => {
                #[cfg(feature = "platform-ssh")]
                {
                    let provider = self.ssh_managed.as_ref().ok_or(
                        PlatformRegistryError::PlatformNotCompiled(PlatformKind::SshManaged),
                    )?;
                    return Ok(PlatformBindings::SshManaged(Arc::new(
                        provider.bind(profile)?,
                    )));
                }
                #[cfg(not(feature = "platform-ssh"))]
                {
                    return Err(PlatformRegistryError::PlatformNotCompiled(
                        PlatformKind::SshManaged,
                    ));
                }
            }
            PlatformConfig::Local(_) => {
                profile.validate()?;
                let platform_hash = profile.snapshot_hash()?;
                let descriptor = PlatformCapabilityDescriptor {
                    capability: CapabilityKind::LocalOpen,
                    requirement: CapabilityRequirement::PlatformOnly,
                    provider: PlatformKind::Local,
                    driver: "local-file-v1".to_owned(),
                    evidence: EvidenceMaturity::ProtocolVerified,
                    minimum_sensor_evidence: EvidenceMaturity::Unknown,
                    initialization: InitializationState::NotRequired,
                    concurrency: ConcurrencyPolicy::Independent,
                    hard_limits: CapabilityLimits {
                        max_payload_bytes: None,
                        max_concurrent_operations: None,
                    },
                    supported_variants: BTreeSet::from([CapabilityVariant::LocalRaw]),
                    platform_snapshot_hash: platform_hash,
                };
                let service: Arc<dyn LocalOpenService> = Arc::new(LocalOpenAdapter {
                    service_id: format!("local:{}", profile.id),
                });
                let bindings = LocalBindings::new(
                    profile.id.clone(),
                    platform_hash,
                    Some(PlatformCapabilityHandle {
                        service,
                        descriptor: Arc::new(descriptor),
                    }),
                )?;
                Ok(PlatformBindings::Local(Arc::new(bindings)))
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum PlatformRegistryError {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Capability(#[from] CapabilityResolutionError),
    #[error("platform provider is not compiled or registered: {0:?}")]
    PlatformNotCompiled(PlatformKind),
    #[cfg(feature = "platform-cv610")]
    #[error(transparent)]
    Cv610(#[from] Cv610ProviderError),
    #[cfg(feature = "platform-ssh")]
    #[error(transparent)]
    SshManaged(#[from] SshManagedProviderError),
}

#[cfg(test)]
mod tests {
    use camera_toolbox_app::{
        Cv610Config, Cv610DumpConfig, Cv610StreamConfig, LocalConfig, PlatformProfileId,
        SshManagedConfig,
    };

    use super::*;

    fn local_profile() -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new("local-a").unwrap(),
            display_name: "Local A".to_owned(),
            config: PlatformConfig::Local(LocalConfig::default()),
        }
    }

    fn cv610_profile() -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new("cv610-a").unwrap(),
            display_name: "CV610 A".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "192.0.2.10".to_owned(),
                dump: Cv610DumpConfig::default(),
                stream: Cv610StreamConfig::default(),
            }),
        }
    }

    fn ssh_profile() -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new("ssh-a").unwrap(),
            display_name: "SSH A".to_owned(),
            config: PlatformConfig::SshManaged(SshManagedConfig {
                host: "192.0.2.10".to_owned(),
                port: 22,
                username: "camera".to_owned(),
                expected_host_key: "memory-host-key".to_owned(),
                credential_ref: "test-keychain:ssh-a".to_owned(),
                command_subsystem: None,
                capture_recipe: String::new(),
                remote_artifact_dir: "/data/captures".to_owned(),
                remote_artifact_glob: "*.raw".to_owned(),
                remote_event_subsystem: None,
                passive_watch_auto_open: false,
                stable_samples: 2,
                stability_interval_ms: 100,
                max_fetch_bytes: 1024,
                command_output_bytes: 128,
                eeprom: None,
            }),
        }
    }

    #[test]
    fn empty_registry_binds_local() {
        assert!(matches!(
            PlatformRegistry::new().bind(&local_profile()),
            Ok(PlatformBindings::Local(_))
        ));
    }

    #[cfg(not(feature = "platform-cv610"))]
    #[test]
    fn disabled_cv610_profile_reports_typed_unavailability() {
        assert!(matches!(
            PlatformRegistry::default().bind(&cv610_profile()),
            Err(PlatformRegistryError::PlatformNotCompiled(
                PlatformKind::HisiliconCv610
            ))
        ));
    }

    #[cfg(not(feature = "platform-ssh"))]
    #[test]
    fn disabled_ssh_profile_reports_typed_unavailability() {
        assert!(matches!(
            PlatformRegistry::default().bind(&ssh_profile()),
            Err(PlatformRegistryError::PlatformNotCompiled(
                PlatformKind::SshManaged
            ))
        ));
    }

    #[cfg(feature = "platform-cv610")]
    #[test]
    fn unregistered_cv610_provider_reports_typed_unavailability() {
        assert!(matches!(
            PlatformRegistry::default().bind(&cv610_profile()),
            Err(PlatformRegistryError::PlatformNotCompiled(
                PlatformKind::HisiliconCv610
            ))
        ));
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn unregistered_ssh_provider_reports_typed_unavailability() {
        assert!(matches!(
            PlatformRegistry::default().bind(&ssh_profile()),
            Err(PlatformRegistryError::PlatformNotCompiled(
                PlatformKind::SshManaged
            ))
        ));
    }

    #[cfg(feature = "platform-cv610")]
    #[test]
    fn registered_cv610_provider_binds_cv610_profile() {
        let mut registry = PlatformRegistry::default();
        registry.register_cv610(HisiliconCv610Provider::default());
        assert!(matches!(
            registry.bind(&cv610_profile()),
            Ok(PlatformBindings::Cv610(_))
        ));
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn registered_ssh_provider_binds_ssh_profile() {
        use crate::platforms::ssh_managed::{
            CommandRecipeRegistry, CredentialResolver, MemorySshTransport,
            SshManagedPlatformProvider, SshTransportFactory,
        };

        let transport = MemorySshTransport::new("memory-host-key");
        let resolver: Arc<dyn CredentialResolver> = Arc::new(transport.clone());
        let factory: Arc<dyn SshTransportFactory> = Arc::new(transport);
        let provider = SshManagedPlatformProvider::new(
            resolver,
            factory,
            Arc::new(CommandRecipeRegistry::new()),
        );
        let mut registry = PlatformRegistry::default();
        registry.register_ssh(provider);
        assert!(matches!(
            registry.bind(&ssh_profile()),
            Ok(PlatformBindings::SshManaged(_))
        ));
    }
}
