//! CV610 profile 到 typed Dump handle 的绑定。

use std::{collections::BTreeSet, sync::Arc};

use camera_toolbox_app::{
    CapabilityKind, CapabilityLimits, CapabilityRequirement, CapabilityResolutionError,
    CapabilityVariant, ConcurrencyPolicy, Cv610Bindings, DumpInitializationPolicy, DumpService,
    EvidenceMaturity, InitializationState, PlatformCapabilityDescriptor, PlatformCapabilityHandle,
    PlatformConfig, PlatformKind, PlatformProfile, ProfileError, StreamService,
};
use thiserror::Error;

use super::dump_service::{Cv610DumpEndpoint, Cv610DumpService, ValidatedInitializerRegistry};
use super::pqstream_protocol::DEFAULT_MAX_RECORD_BYTES;
use super::pqtools_protocol::DEFAULT_MAX_RESPONSE_BYTES;
use super::stream_service::{Cv610StreamEndpoint, Cv610StreamService};

pub struct HisiliconCv610Provider {
    initializers: ValidatedInitializerRegistry,
    max_response_bytes: usize,
}

impl Default for HisiliconCv610Provider {
    fn default() -> Self {
        Self {
            initializers: ValidatedInitializerRegistry::new(),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

impl HisiliconCv610Provider {
    #[must_use]
    pub fn new(initializers: ValidatedInitializerRegistry, max_response_bytes: usize) -> Self {
        Self {
            initializers,
            max_response_bytes,
        }
    }

    /// 绑定 profile 时只构造 service；不会连接设备或执行初始化。
    ///
    /// # Errors
    ///
    /// profile variant/字段、recipe hook、service 或 capability binding 无效时返回错误。
    pub fn bind(&self, profile: &PlatformProfile) -> Result<Cv610Bindings, Cv610ProviderError> {
        let platform_snapshot_hash = profile.snapshot_hash()?;
        let PlatformConfig::HisiliconCv610(config) = &profile.config else {
            return Err(Cv610ProviderError::WrongProfileVariant);
        };
        let address = config
            .host
            .parse()
            .map_err(|error: std::net::AddrParseError| {
                camera_toolbox_app::DumpServiceError::AddressResolution {
                    endpoint: format!("{}:{}", config.host, config.dump.port),
                    reason: error.to_string(),
                }
            })?;
        let service = Cv610DumpService::new(
            format!("cv610:{}:dump", profile.id.as_str()),
            Cv610DumpEndpoint {
                address,
                port: config.dump.port,
            },
            config.dump.initialization.clone(),
            &self.initializers,
            self.max_response_bytes,
        )?;
        let initialization = match &config.dump.initialization {
            DumpInitializationPolicy::Auto | DumpInitializationPolicy::DirectOnly => {
                InitializationState::DirectProbe
            }
            DumpInitializationPolicy::ValidatedRecipe { recipe_id } => {
                InitializationState::ValidatedRecipe {
                    recipe_id: recipe_id.clone(),
                }
            }
        };
        let descriptor = Arc::new(PlatformCapabilityDescriptor {
            capability: CapabilityKind::Dump,
            requirement: CapabilityRequirement::PlatformOnly,
            provider: PlatformKind::HisiliconCv610,
            driver: "pqtools-dump-v1".to_owned(),
            evidence: EvidenceMaturity::ProtocolVerified,
            minimum_sensor_evidence: EvidenceMaturity::Unknown,
            initialization,
            concurrency: ConcurrencyPolicy::UnverifiedExclusive,
            hard_limits: CapabilityLimits {
                max_payload_bytes: Some(self.max_response_bytes as u64),
                max_concurrent_operations: Some(1),
            },
            supported_variants: BTreeSet::from([
                CapabilityVariant::Raw10,
                CapabilityVariant::Raw12,
                CapabilityVariant::Jpeg,
                CapabilityVariant::Nv21,
            ]),
            platform_snapshot_hash,
        });
        let dump: PlatformCapabilityHandle<dyn DumpService> = PlatformCapabilityHandle {
            service: Arc::new(service),
            descriptor,
        };
        let stream_service = Cv610StreamService::new(
            format!("cv610:{}:stream", profile.id.as_str()),
            Cv610StreamEndpoint {
                address,
                port: config.stream.port,
            },
        )?;
        let stream_descriptor = Arc::new(PlatformCapabilityDescriptor {
            capability: CapabilityKind::Stream,
            requirement: CapabilityRequirement::PlatformOnly,
            provider: PlatformKind::HisiliconCv610,
            driver: "pqstream-v1".to_owned(),
            evidence: EvidenceMaturity::ProtocolVerified,
            minimum_sensor_evidence: EvidenceMaturity::Unknown,
            initialization: InitializationState::NotRequired,
            concurrency: ConcurrencyPolicy::UnverifiedExclusive,
            hard_limits: CapabilityLimits {
                max_payload_bytes: Some(DEFAULT_MAX_RECORD_BYTES as u64),
                max_concurrent_operations: Some(1),
            },
            // H.264 depacketization is implemented, but real CV610 H.264 capability remains unknown.
            supported_variants: BTreeSet::from([CapabilityVariant::H265]),
            platform_snapshot_hash,
        });
        let stream: PlatformCapabilityHandle<dyn StreamService> = PlatformCapabilityHandle {
            service: Arc::new(stream_service),
            descriptor: stream_descriptor,
        };
        Cv610Bindings::new(
            profile.id.clone(),
            platform_snapshot_hash,
            Some(dump),
            Some(stream),
        )
        .map_err(Into::into)
    }
}

#[derive(Debug, Error)]
pub enum Cv610ProviderError {
    #[error("CV610 provider requires a HisiliconCv610 profile")]
    WrongProfileVariant,
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Service(#[from] camera_toolbox_app::DumpServiceError),
    #[error(transparent)]
    StreamService(#[from] camera_toolbox_app::StreamServiceError),
    #[error(transparent)]
    Capability(#[from] CapabilityResolutionError),
}

#[cfg(test)]
mod tests {
    use camera_toolbox_app::{Cv610Config, Cv610DumpConfig, Cv610StreamConfig, PlatformProfileId};

    use super::*;

    #[test]
    fn binds_all_verified_dump_variants_without_network_side_effects() {
        let profile = PlatformProfile {
            id: PlatformProfileId::new("cv610-a").unwrap(),
            display_name: "CV610 A".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "10.21.12.102".to_owned(),
                dump: Cv610DumpConfig::default(),
                stream: Cv610StreamConfig::default(),
            }),
        };
        let bindings = HisiliconCv610Provider::default().bind(&profile).unwrap();
        let dump = bindings.dump.unwrap();
        assert_eq!(dump.service.service_id(), "cv610:cv610-a:dump");
        assert_eq!(
            dump.descriptor.supported_variants,
            BTreeSet::from([
                CapabilityVariant::Raw10,
                CapabilityVariant::Raw12,
                CapabilityVariant::Jpeg,
                CapabilityVariant::Nv21,
            ])
        );
        assert_eq!(
            dump.descriptor.initialization,
            InitializationState::DirectProbe
        );
        let stream = bindings.stream.unwrap();
        assert_eq!(stream.service.service_id(), "cv610:cv610-a:stream");
        assert_eq!(
            stream.descriptor.supported_variants,
            BTreeSet::from([CapabilityVariant::H265])
        );
    }

    #[test]
    fn validated_recipe_requires_injected_exact_hook() {
        let profile = PlatformProfile {
            id: camera_toolbox_app::PlatformProfileId::new("cv610-a").unwrap(),
            display_name: "CV610 A".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "10.21.12.102".to_owned(),
                dump: Cv610DumpConfig {
                    initialization: DumpInitializationPolicy::ValidatedRecipe {
                        recipe_id: "not-injected".to_owned(),
                    },
                    ..Cv610DumpConfig::default()
                },
                stream: Cv610StreamConfig::default(),
            }),
        };
        assert!(matches!(
            HisiliconCv610Provider::default().bind(&profile),
            Err(Cv610ProviderError::Service(
                camera_toolbox_app::DumpServiceError::InitializationRecipeUnavailable { .. }
            ))
        ));
    }
}
