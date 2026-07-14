//! Tagged profile 到独立 provider variant 的唯一 adapter registry。

use std::{collections::BTreeSet, sync::Arc};

use camera_toolbox_app::{
    CapabilityKind, CapabilityLimits, CapabilityRequirement, CapabilityResolutionError,
    CapabilityVariant, ConcurrencyPolicy, EvidenceMaturity, InitializationState, LocalBindings,
    LocalOpenService, PlatformBindings, PlatformCapabilityDescriptor, PlatformCapabilityHandle,
    PlatformConfig, PlatformKind, PlatformProfile, ProfileError,
};
use thiserror::Error;

use crate::platforms::{
    hisilicon_cv610::{Cv610ProviderError, HisiliconCv610Provider},
    ssh_managed::{SshManagedPlatformProvider, SshManagedProviderError},
};

#[derive(Debug)]
struct LocalOpenAdapter {
    service_id: String,
}

impl LocalOpenService for LocalOpenAdapter {
    fn service_id(&self) -> &str {
        &self.service_id
    }
}

pub struct PlatformRegistry {
    cv610: HisiliconCv610Provider,
    ssh_managed: SshManagedPlatformProvider,
}

impl PlatformRegistry {
    #[must_use]
    pub fn new(cv610: HisiliconCv610Provider, ssh_managed: SshManagedPlatformProvider) -> Self {
        Self { cv610, ssh_managed }
    }

    /// Local 在 registry 内构造无连接 handle；网络 variant 委派给独立 provider。
    pub fn bind(
        &self,
        profile: &PlatformProfile,
    ) -> Result<PlatformBindings, PlatformRegistryError> {
        match &profile.config {
            PlatformConfig::HisiliconCv610(_) => {
                Ok(PlatformBindings::Cv610(Arc::new(self.cv610.bind(profile)?)))
            }
            PlatformConfig::SshManaged(_) => Ok(PlatformBindings::SshManaged(Arc::new(
                self.ssh_managed.bind(profile)?,
            ))),
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
    #[error(transparent)]
    Cv610(#[from] Cv610ProviderError),
    #[error(transparent)]
    SshManaged(#[from] SshManagedProviderError),
}
